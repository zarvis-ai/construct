//! Drives the real web client in headless Chromium and asserts the
//! spec-0074 shared-dialect behavior — widget smart clips, program
//! projections, action chips in the Program editor, and the serialization
//! round-trip invariant.

use std::time::{Duration, Instant};

use construct_e2e::Daemon;
use chromiumoxide::browser::{Browser, BrowserConfig};
use futures::StreamExt;

fn inject_userinfo(url: &str, user: &str, pw: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("http://{user}:{pw}@{rest}")
    } else if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{user}:{pw}@{rest}")
    } else {
        url.to_string()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_dialect_web_surfaces() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(true, None)
        .await
        .expect("remote.start");

    let config = BrowserConfig::builder()
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .build()
        .expect("browser config");
    let (browser, mut handler) = match Browser::launch(config).await {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("skipping dialect_web: could not launch Chromium ({e}).");
            return;
        }
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });
    let page = browser.new_page("about:blank").await.expect("new page");
    page.goto(&inject_userinfo(&r.url, "remote", &r.password))
        .await
        .expect("goto");

    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state: String = page
            .evaluate("document.getElementById('conn')?.dataset?.state || ''")
            .await
            .and_then(|r| r.into_value::<String>().map_err(Into::into))
            .unwrap_or_default();
        if state == "open" {
            break;
        }
        assert!(Instant::now() < deadline, "web client never connected");
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // --- 1. Widget markdown: inline @{session:...} renders a live chip;
    //        a session/state push repaints it in place. ---------------------
    let widget_chip: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const saved = {
                sessions: state.sessions,
                currentId: state.currentId,
                widgetsById: state.widgetsById,
                mode: state.mode,
              };
              try {
                state.mode = 'chat';
                state.sessions = [
                  { id: 's-owner', title: 'Owner', harness: 'smith', state: 'running', kind: 'user' },
                  { id: 's-work', title: 'Worker', harness: 'shell', state: 'running', kind: 'user' },
                ];
                state.currentId = 's-owner';
                state.widgetsById = new Map([['s-owner', [
                  { id: 'status', markdown: '# Status\n- [~] worker @{session:s-work} running\n[Pause](agentd:action/pause?key=p)' },
                ]]]);
                state.widgetTemporaryUntilById.set('s-owner', performance.now() + 60000);
                state.widgetsDropdownOpen = true;
                renderWidgets();
                const chip = sessionWidgetsEl.querySelector('.program-clip[data-raw]');
                const out = {
                  chipMounted: !!chip,
                  chipRaw: chip?.dataset?.raw || '',
                  chipStatus: chip?.dataset?.status || '',
                  chipLabelHasTitle: (chip?.textContent || '').includes('Worker'),
                  actionButton: !!sessionWidgetsEl.querySelector('.widget-action[data-action-id="pause"]'),
                };
                // Live repaint: the worker errors; the widget chip must flip
                // without a re-render call.
                handleNotification('session/state', { session: { id: 's-work', title: 'Worker', harness: 'shell', state: 'errored', kind: 'user' } });
                const chip2 = sessionWidgetsEl.querySelector('.program-clip[data-raw]');
                out.statusAfterPush = chip2?.dataset?.status || '';
                return out;
              } finally {
                state.sessions = saved.sessions;
                state.currentId = saved.currentId;
                state.widgetsById = saved.widgetsById;
                state.mode = saved.mode;
                state.widgetTemporaryUntilById.delete('s-owner');
                state.widgetsDropdownOpen = false;
                renderWidgets();
              }
            })()
            "#,
        )
        .await
        .expect("evaluate widget chip")
        .into_value()
        .expect("json");
    assert_eq!(widget_chip["chipMounted"], true, "{widget_chip:?}");
    assert_eq!(
        widget_chip["chipRaw"], "@{session:s-work}",
        "{widget_chip:?}"
    );
    assert_eq!(widget_chip["chipStatus"], "running", "{widget_chip:?}");
    assert_eq!(widget_chip["chipLabelHasTitle"], true, "{widget_chip:?}");
    assert_eq!(widget_chip["actionButton"], true, "{widget_chip:?}");
    assert_eq!(
        widget_chip["statusAfterPush"], "errored",
        "widget chip must repaint on session/state push: {widget_chip:?}"
    );

    // --- 2. Widget :::clip program projection: loading placeholder, fetch,
    //        section extraction, program/state refresh. ---------------------
    let projection: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const saved = {
                sessions: state.sessions,
                currentId: state.currentId,
                widgetsById: state.widgetsById,
                mode: state.mode,
                ws: state.ws,
              };
              const calls = [];
              try {
                state.mode = 'chat';
                state.sessions = [{ id: 's-proj', title: 'Owner', harness: 'smith', state: 'running', kind: 'user' }];
                state.currentId = 's-proj';
                state.widgetProgramById.clear();
                state.widgetsById = new Map([['s-proj', [
                  { id: 'progress', markdown: '# Progress\n:::clip program section="Progress"\n:::' },
                ]]]);
                state.widgetTemporaryUntilById.set('s-proj', performance.now() + 60000);
                state.widgetsDropdownOpen = true;
                state.ws = {
                  readyState: 1,
                  send(raw) {
                    const msg = JSON.parse(raw);
                    calls.push(msg);
                    const pending = state.pending.get(msg.id);
                    state.pending.delete(msg.id);
                    let result = {};
                    if (msg.method === 'program.get') {
                      result = { program: { session_id: 's-proj', markdown: '# Plan\n## Progress\n- [x] step one\n## Next\n- [ ] later', version: 2, template_id: null }, blocks: [], active_run: null, revisions: [] };
                    }
                    queueMicrotask(() => pending.resolve(result));
                  },
                };
                renderWidgets();
                const loading = !!sessionWidgetsEl.querySelector('.widget-projection.is-loading');
                for (let i = 0; i < 20 && !sessionWidgetsEl.querySelector('.widget-projection:not(.is-loading)'); i++) {
                  await new Promise((resolve) => requestAnimationFrame(resolve));
                }
                const proj = sessionWidgetsEl.querySelector('.widget-projection:not(.is-loading)');
                const out = {
                  loadingFirst: loading,
                  fetches: calls.filter((c) => c.method === 'program.get').length,
                  projected: proj ? proj.textContent : '',
                };
                // A program/state push refreshes the projection.
                handleNotification('program/state', { program: { session_id: 's-proj', markdown: '# Plan\n## Progress\n- [x] step one\n- [~] step two live\n## Next\n- [ ] later', version: 3, template_id: null }, blocks: [], active_run: null });
                await new Promise((resolve) => requestAnimationFrame(resolve));
                const proj2 = sessionWidgetsEl.querySelector('.widget-projection:not(.is-loading)');
                out.projectedAfterPush = proj2 ? proj2.textContent : '';
                return out;
              } finally {
                state.sessions = saved.sessions;
                state.currentId = saved.currentId;
                state.widgetsById = saved.widgetsById;
                state.mode = saved.mode;
                state.ws = saved.ws;
                state.widgetProgramById.clear();
                state.widgetTemporaryUntilById.delete('s-proj');
                state.widgetsDropdownOpen = false;
                renderWidgets();
              }
            })()
            "#,
        )
        .await
        .expect("evaluate projection")
        .into_value()
        .expect("json");
    assert_eq!(projection["loadingFirst"], true, "{projection:?}");
    assert_eq!(projection["fetches"], 1, "{projection:?}");
    let projected = projection["projected"].as_str().unwrap_or_default();
    assert!(
        projected.contains("step one")
            && projected.contains("Progress")
            && !projected.contains("later"),
        "projection extracts only the named section: {projection:?}"
    );
    assert!(
        projection["projectedAfterPush"]
            .as_str()
            .is_some_and(|t| t.contains("step two live")),
        "projection must follow program/state pushes: {projection:?}"
    );

    // --- 3. Program editor: action chips render atomically, serialize
    //        byte-identically, dim fence classes applied, click dispatches
    //        ui.action with no panel_id. --------------------------------------
    let editor: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const saved = {
                sessions: state.sessions,
                currentId: state.currentId,
                mode: state.mode,
                mountedId: state.program.mountedId,
                ws: state.ws,
                html: programInputEl.innerHTML,
                wrapHidden: programWrapEl.hidden,
              };
              const calls = [];
              try {
                const markdown = '# Plan\n:::timeline\n- [x] done step\n- [~] active [Re-run](agentd:action/re-run?key=r) now\n:::\n| a | b |\n| --- | --- |\n| 1 | 2 |\n@{session:s-w1} works';
                state.sessions = [
                  { id: 's-owner2', title: 'Owner', harness: 'smith', state: 'running', kind: 'user' },
                  { id: 's-w1', title: 'W1', harness: 'shell', state: 'running', kind: 'user' },
                ];
                state.currentId = 's-owner2';
                state.mode = 'program';
                state.program.mountedId = 's-owner2';
                state.ws = {
                  readyState: 1,
                  send(raw) {
                    const msg = JSON.parse(raw);
                    calls.push(msg);
                    const pending = state.pending.get(msg.id);
                    state.pending.delete(msg.id);
                    queueMicrotask(() => pending.resolve({}));
                  },
                };
                programWrapEl.hidden = false;
                programRenderDoc(markdown);
                const roundTrip = programSerialize();
                const chip = programInputEl.querySelector('.program-action[data-action-id]');
                const out = {
                  roundTripIdentical: roundTrip === markdown,
                  roundTrip,
                  actionChipMounted: !!chip,
                  actionChipAtomic: chip?.getAttribute('contenteditable') === 'false',
                  actionChipLabel: chip?.textContent || '',
                  fenceDimCount: programInputEl.querySelectorAll('.program-line.pl-fence').length,
                  tableDelimDim: programInputEl.querySelectorAll('.program-line.pl-table-delim').length,
                  checkMarkers: Array.from(programInputEl.querySelectorAll('.program-check')).map((el) => [el.textContent, el.className]),
                  sessionChip: !!programInputEl.querySelector('.program-clip[data-session-id="s-w1"]'),
                };
                chip.dispatchEvent(new MouseEvent('click', { bubbles: true, cancelable: true }));
                for (let i = 0; i < 20 && !calls.some((c) => c.method === 'session.input'); i++) {
                  await new Promise((resolve) => requestAnimationFrame(resolve));
                }
                const input = calls.find((c) => c.method === 'session.input');
                out.dispatchSession = input?.params?.session_id || '';
                out.dispatchText = input?.params?.text || '';
                return out;
              } finally {
                state.sessions = saved.sessions;
                state.currentId = saved.currentId;
                state.mode = saved.mode;
                state.program.mountedId = saved.mountedId;
                state.ws = saved.ws;
                programInputEl.innerHTML = saved.html;
                programWrapEl.hidden = saved.wrapHidden;
              }
            })()
            "#,
        )
        .await
        .expect("evaluate editor")
        .into_value()
        .expect("json");
    assert_eq!(
        editor["roundTripIdentical"], true,
        "program serialization must be byte-identical: {editor:?}"
    );
    assert_eq!(editor["actionChipMounted"], true, "{editor:?}");
    assert_eq!(editor["actionChipAtomic"], true, "{editor:?}");
    assert_eq!(
        editor["actionChipLabel"], "Re-run",
        "no ?key= prefix in the editor label: {editor:?}"
    );
    assert!(
        editor["fenceDimCount"].as_u64().unwrap_or(0) >= 2,
        ":::timeline and ::: lines get pl-fence: {editor:?}"
    );
    assert_eq!(editor["tableDelimDim"], 1, "{editor:?}");
    assert_eq!(editor["sessionChip"], true, "{editor:?}");
    let markers = editor["checkMarkers"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        markers
            .iter()
            .any(|m| m[0] == "[x]" && m[1].as_str().unwrap_or("").contains("done"))
            && markers
                .iter()
                .any(|m| m[0] == "[~]" && m[1].as_str().unwrap_or("").contains("active")),
        "checklist markers styled with exact source text: {editor:?}"
    );
    assert_eq!(editor["dispatchSession"], "s-owner2", "{editor:?}");
    let text = editor["dispatchText"].as_str().unwrap_or_default();
    assert!(
        text.starts_with("OBSERVATION: ui.action ")
            && text.contains("\"action_id\":\"re-run\"")
            && !text.contains("panel_id"),
        "action chip dispatches ui.action without panel_id: {editor:?}"
    );

    drop(page);
    drop(browser);
}
