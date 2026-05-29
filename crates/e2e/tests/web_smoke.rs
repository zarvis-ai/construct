//! End-to-end: drive the bundled web client in a real headless
//! Chromium via the Chrome DevTools Protocol. Catches the kind
//! of regressions that wire-level tests miss — JS boot, the
//! HTTP-vs-WS demux on the same port, xterm.js init, the
//! `setConnState("open", ...)` path that fires after the WS
//! upgrade succeeds.
//!
//! Skipped (not failed) when Chrome / Chromium isn't installed
//! on the host, so dev machines without a browser don't see
//! spurious failures. GitHub-hosted `ubuntu-latest` runners
//! ship Google Chrome pre-installed, so this runs in CI by
//! default.

use std::path::Path;
use std::time::{Duration, Instant};

use agentd_e2e::{artifact_dir, Daemon};
use base64::engine::general_purpose::STANDARD as B64;
use base64::Engine as _;
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::cdp::browser_protocol::page::{
    EventScreencastFrame, ScreencastFrameAckParams, StartScreencastFormat, StartScreencastParams,
    StopScreencastParams,
};
use chromiumoxide::page::Page;
use futures::StreamExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_client_loads_and_websocket_connects() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(/* local_only */ true, /* password */ None)
        .await
        .expect("remote.start");

    // Headless Chrome with the conservative flag set Linux CI
    // expects. `--no-sandbox` is required because GitHub runners
    // run as root inside a container; `--disable-gpu` avoids
    // shader-compile failures on headless servers without a GPU.
    let config = BrowserConfig::builder()
        .arg("--no-sandbox")
        .arg("--disable-gpu")
        .arg("--disable-dev-shm-usage")
        .build()
        .expect("browser config");
    let launch = Browser::launch(config).await;
    let (browser, mut handler) = match launch {
        Ok(pair) => pair,
        Err(e) => {
            // No Chrome on this host — emit a hint and pass.
            // We can't easily `#[ignore]` conditionally, so this
            // is the next best thing for dev machines.
            eprintln!(
                "skipping web_smoke: could not launch Chromium ({e}). \
                 Install Google Chrome to run this test locally."
            );
            return;
        }
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.expect("new page");

    // Start a CDP screencast so the test produces a real video
    // artifact reviewers can play back. Returns a guard that
    // stops the screencast + assembles the video on drop.
    let recording = start_screencast(&page, "web_smoke")
        .await
        .expect("start screencast");

    // Embed Basic credentials directly in the URL. Chrome still
    // sends the resulting `Authorization` header for the initial
    // navigation (it only hides the userinfo in the address bar
    // for spoofing reasons) and caches them in its per-origin
    // HTTP auth credentials store. The subsequent WebSocket
    // upgrade — which can't take its own header from CDP because
    // the browser's WS API doesn't expose request headers —
    // picks the cached creds up automatically. Modern CDP
    // `Fetch`-domain interception (`Page::authenticate`) is the
    // documented alternative but is unreliable on the first
    // navigation in headless mode (see chromiumoxide#issues).
    let url_with_creds = inject_userinfo(&r.url, "remote", &r.password);
    page.goto(&url_with_creds).await.expect("goto");

    // The web client's JS sets `#conn`'s `data-state` to `"open"`
    // after the WebSocket upgrade succeeds. Polling that
    // attribute is a direct signal that the whole stack
    // (HTTP+WS demux, token gating, Basic auth, ws.onopen) is
    // working.
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
        if Instant::now() > deadline {
            // Pull the body text to surface what the page is
            // showing — usually an error from the JS console or
            // an empty body if the JS never ran.
            let body: String = page
                .evaluate("document.body?.innerText || ''")
                .await
                .ok()
                .and_then(|r| r.into_value::<String>().ok())
                .unwrap_or_else(|| "(no body)".into());
            panic!(
                "web client never reached conn state='open' (last={state:?}).\n\
                 --- page body ---\n{body}\n-----------------"
            );
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }

    // Sanity: the static HTML / bundled JS rendered. The empty
    // session-list label is visible in the layout regardless of
    // whether any sessions exist on the daemon.
    let body: String = page
        .evaluate("document.body.innerText || ''")
        .await
        .expect("body innerText")
        .into_value::<String>()
        .expect("string");
    assert!(
        body.contains("sessions") || body.contains("session"),
        "expected 'session(s)' in rendered body, got:\n{body}"
    );

    // Connection state is rendered as a tiny matrix canvas rather than
    // a static "connected" text label. The accessible label remains
    // for screen readers.
    let mini_matrix: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const canvas = document.getElementById('miniMatrix');
              const headerTitle = document.querySelector('header .title');
              const conn = document.getElementById('conn');
              const rect = canvas ? canvas.getBoundingClientRect() : null;
              return {
                tag: canvas ? canvas.tagName : null,
                label: canvas ? canvas.getAttribute('aria-label') : null,
                role: canvas ? canvas.getAttribute('role') : null,
                connState: conn ? conn.dataset.state : null,
                connLabel: conn ? conn.getAttribute('aria-label') : null,
                visibleConnText: conn ? conn.textContent.trim() : null,
                width: rect ? rect.width : 0,
                height: rect ? rect.height : 0,
                hasStaticTitle: !!headerTitle,
                painted: canvas
                  ? Array.from(canvas.getContext('2d').getImageData(0, 0, canvas.width, canvas.height).data)
                      .some((v, i) => (i % 4 === 3) && v > 0)
                  : false,
              };
            })()
            "#,
        )
        .await
        .expect("evaluate mini matrix")
        .into_value()
        .expect("json");
    assert_eq!(mini_matrix["tag"], "CANVAS");
    assert_eq!(mini_matrix["label"], "agentd connected");
    assert_eq!(mini_matrix["role"], "img");
    assert_eq!(mini_matrix["connState"], "open");
    assert_eq!(mini_matrix["connLabel"], "connected");
    assert_eq!(mini_matrix["visibleConnText"], "");
    assert_eq!(mini_matrix["hasStaticTitle"], false);
    assert!(
        mini_matrix["width"].as_f64().unwrap_or_default() >= 40.0
            && mini_matrix["height"].as_f64().unwrap_or_default() >= 16.0,
        "mini matrix should have visible dimensions, got {mini_matrix:?}"
    );
    assert_eq!(mini_matrix["painted"], true);

    // Page-level sanity: the bundled xterm.js was loaded (i.e.
    // the embedded `/t/<token>/static/xterm.js` request
    // succeeded). The web client puts `Terminal` on `window` as
    // a side effect of importing the script.
    let xterm_present: bool = page
        .evaluate("typeof window.Terminal === 'function'")
        .await
        .expect("evaluate xterm")
        .into_value::<bool>()
        .expect("bool");
    assert!(
        xterm_present,
        "bundled xterm.js never loaded (window.Terminal !== 'function')"
    );

    // Mobile regression: selecting a PTY-backed session from the list must not
    // focus xterm when the native keyboard is hidden, or iOS/Android pop the
    // keyboard just because the user changed selection. If the keyboard was
    // already visible, selection should preserve terminal focus.
    let switch_focus: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const saved = {
                currentId: state.currentId,
                sessions: state.sessions,
                mode: state.mode,
                term: state.term,
                fitAddon: state.fitAddon,
                ptyBuffer: state.ptyBuffer,
                ptyBuffering: state.ptyBuffering,
                lastReportedSize: state.lastReportedSize,
                waitForXterm,
                initTerminalForSession,
                showTerminalForSession,
                activateTerminalHandle,
                refitTerminal,
                blurActiveTerminalInput,
                renderSessions,
                renderBrowserPreviewForSession,
                setSessionListVisible,
                isNarrowLayout,
                setComposerEnabled,
                renderEditorStateForSession,
                renderVirtualKeyboard,
                shouldFocusTerminalAfterSessionSwitch,
              };
              const focusCalls = [];
              const blurCalls = [];
              const refitCalls = [];
              const handles = new Map();

              try {
                waitForXterm = async () => true;
                initTerminalForSession = (id) => {
                  const handle = {
                    id,
                    host: document.createElement('div'),
                    loaded: true,
                    ptyBuffer: [],
                    ptyBuffering: false,
                    lastReportedSize: { cols: 0, rows: 0 },
                    term: {
                      cols: 80,
                      rows: 24,
                      focus: () => focusCalls.push(id),
                      resize: () => {},
                      write: () => {},
                    },
                    fitAddon: { fit: () => {} },
                  };
                  handles.set(id, handle);
                  return handle;
                };
                showTerminalForSession = (id) => handles.get(id);
                activateTerminalHandle = (handle) => {
                  state.term = handle.term;
                  state.fitAddon = handle.fitAddon;
                  state.ptyBuffer = handle.ptyBuffer;
                  state.ptyBuffering = handle.ptyBuffering;
                  state.lastReportedSize = handle.lastReportedSize;
                };
                refitTerminal = (options) => refitCalls.push(options || {});
                blurActiveTerminalInput = () => blurCalls.push(state.currentId);
                renderSessions = () => {};
                renderBrowserPreviewForSession = () => {};
                setSessionListVisible = () => {};
                isNarrowLayout = () => true;
                setComposerEnabled = () => {};
                renderEditorStateForSession = () => {};
                renderVirtualKeyboard = () => {};

                state.sessions = [
                  { id: 's-keyboard-hidden', has_pty: true, mode: 'interactive' },
                  { id: 's-keyboard-open', has_pty: true, mode: 'interactive' },
                ];
                state.currentId = 's-before-hidden';
                shouldFocusTerminalAfterSessionSwitch = () => false;
                await selectSession('s-keyboard-hidden');
                const hiddenFocusCount = focusCalls.length;
                const hiddenBlurCount = blurCalls.length;

                shouldFocusTerminalAfterSessionSwitch = () => true;
                await selectSession('s-keyboard-open');

                return {
                  hiddenFocusCount,
                  hiddenBlurCount,
                  focusCalls,
                  blurCalls,
                  refitCalls,
                };
              } finally {
                state.currentId = saved.currentId;
                state.sessions = saved.sessions;
                state.mode = saved.mode;
                state.term = saved.term;
                state.fitAddon = saved.fitAddon;
                state.ptyBuffer = saved.ptyBuffer;
                state.ptyBuffering = saved.ptyBuffering;
                state.lastReportedSize = saved.lastReportedSize;
                waitForXterm = saved.waitForXterm;
                initTerminalForSession = saved.initTerminalForSession;
                showTerminalForSession = saved.showTerminalForSession;
                activateTerminalHandle = saved.activateTerminalHandle;
                refitTerminal = saved.refitTerminal;
                blurActiveTerminalInput = saved.blurActiveTerminalInput;
                renderSessions = saved.renderSessions;
                renderBrowserPreviewForSession = saved.renderBrowserPreviewForSession;
                setSessionListVisible = saved.setSessionListVisible;
                isNarrowLayout = saved.isNarrowLayout;
                setComposerEnabled = saved.setComposerEnabled;
                renderEditorStateForSession = saved.renderEditorStateForSession;
                renderVirtualKeyboard = saved.renderVirtualKeyboard;
                shouldFocusTerminalAfterSessionSwitch = saved.shouldFocusTerminalAfterSessionSwitch;
              }
            })()
            "#,
        )
        .await
        .expect("evaluate session switch focus")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(
        switch_focus["hiddenFocusCount"].as_u64(),
        Some(0),
        "hidden keyboard switch should not focus xterm: {switch_focus:?}"
    );
    assert_eq!(
        switch_focus["hiddenBlurCount"].as_u64(),
        Some(1),
        "hidden keyboard switch should blur terminal input: {switch_focus:?}"
    );
    assert_eq!(
        switch_focus["focusCalls"]
            .as_array()
            .cloned()
            .unwrap_or_default(),
        vec![serde_json::Value::String("s-keyboard-open".into())],
        "visible keyboard switch should preserve terminal focus: {switch_focus:?}"
    );

    // Mobile regression: selecting a session auto-hides the narrow session
    // list without changing the stored preference. Keyboard/viewport resizes
    // must preserve that current hidden state instead of re-reading the older
    // stored "visible" preference and expanding the list from the top.
    let list_resize: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const saved = {
                isNarrowLayout,
                sessionListVisible: state.sessionListVisible,
                storage: localStorage.getItem(SESSION_LIST_VISIBLE_KEY),
                mode: state.mode,
              };
              try {
                isNarrowLayout = () => true;
                state.mode = 'chat';
                localStorage.setItem(SESSION_LIST_VISIBLE_KEY, '1');
                setSessionListVisible(false, false);
                const before = {
                  visible: isSessionListVisible(),
                  collapsed: document.getElementById('sessionList').classList.contains('collapsed'),
                  stored: localStorage.getItem(SESSION_LIST_VISIBLE_KEY),
                };
                window.dispatchEvent(new Event('resize'));
                const after = {
                  visible: isSessionListVisible(),
                  collapsed: document.getElementById('sessionList').classList.contains('collapsed'),
                  stored: localStorage.getItem(SESSION_LIST_VISIBLE_KEY),
                };
                return { before, after };
              } finally {
                isNarrowLayout = saved.isNarrowLayout;
                state.sessionListVisible = saved.sessionListVisible;
                state.mode = saved.mode;
                if (saved.storage === null) localStorage.removeItem(SESSION_LIST_VISIBLE_KEY);
                else localStorage.setItem(SESSION_LIST_VISIBLE_KEY, saved.storage);
                setSessionListVisible(saved.sessionListVisible, false);
              }
            })()
            "#,
        )
        .await
        .expect("evaluate session list resize")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(list_resize["before"]["visible"], false);
    assert_eq!(list_resize["before"]["collapsed"], true);
    assert_eq!(list_resize["before"]["stored"], "1");
    assert_eq!(list_resize["after"]["visible"], false);
    assert_eq!(list_resize["after"]["collapsed"], true);
    assert_eq!(list_resize["after"]["stored"], "1");

    // Issue #132: the web session list exposes pin/unpin in the
    // selected-session toolbar and marks pinned rows visibly.
    let pin_ui: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const calls = [];
              const oldRpc = rpc;
              rpc = async (method, params) => {
                calls.push({ method, params });
                return null;
              };
              state.currentId = 's1';
              state.sessions = [
                {
                  id: 's1',
                  title: 'Alpha',
                  harness: 'shell',
                  state: 'running',
                  kind: 'user',
                  pinned: false,
                  position: 0,
                },
                {
                  id: 's2',
                  title: 'Beta',
                  harness: 'zarvis',
                  state: 'awaiting_input',
                  kind: 'user',
                  pinned: true,
                  position: 1,
                },
              ];
              state.groups = [];
              renderSessions();
              const initialButton = document.getElementById('toolbarPinBtn');
              const initial = {
                rowText: document.querySelector('[data-id="s2"]')?.innerText || '',
                buttonText: initialButton?.textContent || '',
                aria: initialButton?.getAttribute('aria-label') || '',
                disabled: initialButton?.disabled === true,
              };
              await handleRowAction('pin', 's1');
              state.currentId = 's2';
              renderSessions();
              const pinnedButton = document.getElementById('toolbarPinBtn');
              const pinned = {
                buttonText: pinnedButton?.textContent || '',
                aria: pinnedButton?.getAttribute('aria-label') || '',
                active: pinnedButton?.classList.contains('is-on') === true,
              };
              rpc = oldRpc;
              return { initial, pinned, calls };
            })()
            "#,
        )
        .await
        .expect("evaluate pin UI")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert!(
        pin_ui["initial"]["rowText"]
            .as_str()
            .unwrap_or_default()
            .contains("★"),
        "pinned session row did not include visible pin marker: {pin_ui:?}"
    );
    assert_eq!(pin_ui["initial"]["buttonText"], "☆");
    assert_eq!(pin_ui["initial"]["aria"], "pin selected");
    assert_eq!(pin_ui["initial"]["disabled"], false);
    assert_eq!(pin_ui["pinned"]["buttonText"], "★");
    assert_eq!(pin_ui["pinned"]["aria"], "unpin selected");
    assert_eq!(pin_ui["pinned"]["active"], true);
    assert_eq!(
        pin_ui["calls"],
        serde_json::json!([
            {
                "method": "session.set_pinned",
                "params": { "session_id": "s1", "pinned": true }
            }
        ])
    );

    // Session rows use the same lifecycle glyph semantics as the TUI
    // instead of spelling the state out in visible English.
    let status_icons: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const oldCurrent = state.currentId;
              const oldSessions = state.sessions;
              const oldGroups = state.groups;
              const now = Date.now();
              state.currentId = 'sp';
              state.groups = [];
              state.sessions = [
                { id: 'sp', title: 'Pending', harness: 'shell', state: 'pending', kind: 'user', position: 0 },
                { id: 'sr', title: 'Running', harness: 'shell', state: 'running', kind: 'user', position: 1 },
                { id: 'sa', title: 'Awaiting', harness: 'shell', state: 'awaiting_input', kind: 'user', position: 2 },
                { id: 'sz', title: 'Paused', harness: 'shell', state: 'paused', kind: 'user', position: 3 },
                { id: 'sd', title: 'Done', harness: 'shell', state: 'done', kind: 'user', position: 4 },
                { id: 'se', title: 'Errored', harness: 'shell', state: 'errored', kind: 'user', position: 5 },
                { id: 'sb', title: 'Busy', harness: 'shell', state: 'running', kind: 'user', position: 6, last_pty_at_ms: now },
              ];
              renderSessions();
              const rows = {};
              for (const el of document.querySelectorAll('#sessionList .item')) {
                const icon = el.querySelector('.state');
                rows[el.dataset.id] = {
                  icon: icon?.textContent || '',
                  label: icon?.getAttribute('aria-label') || '',
                  title: icon?.getAttribute('title') || '',
                  text: el.innerText || '',
                  busy: icon?.classList.contains('is-active') === true,
                };
              }
              state.currentId = oldCurrent;
              state.sessions = oldSessions;
              state.groups = oldGroups;
              renderSessions();
              return rows;
            })()
            "#,
        )
        .await
        .expect("evaluate session status icons")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(status_icons["sp"]["icon"], "○");
    assert_eq!(status_icons["sr"]["icon"], "●");
    assert_eq!(status_icons["sa"]["icon"], "●");
    assert_eq!(status_icons["sz"]["icon"], "⏸");
    assert_eq!(status_icons["sd"]["icon"], "✓");
    assert_eq!(status_icons["se"]["icon"], "✗");
    assert!(
        ["✦", "✧", "✶", "✷", "✸"]
            .contains(&status_icons["sb"]["icon"].as_str().unwrap_or_default()),
        "busy running row should use a TUI spinner glyph: {status_icons:?}"
    );
    assert_eq!(status_icons["sb"]["busy"], true);
    assert_eq!(status_icons["se"]["label"], "errored");
    assert!(
        !status_icons["se"]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("errored"),
        "visible row text should not spell out the status: {status_icons:?}"
    );

    // Issue #75: pasted image/file clipboard items and very large text
    // are uploaded to the daemon as session attachments, and the prompt
    // receives a compact [#file:...] reference instead of raw bytes/text.
    let paste_attachment: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const oldRpc = rpc;
              const oldCurrent = state.currentId;
              const oldWs = state.ws;
              const calls = [];
              rpc = async (method, params) => {
                calls.push({ method, params, byteLength: atob(params.data).length });
                return { path: `/tmp/${params.filename}`, reference: `[#file:/tmp/${params.filename}]` };
              };
              state.currentId = 's-paste';
              state.ws = { readyState: 1 };

              inputEl.value = 'Look';
              inputEl.setSelectionRange(4, 4);
              let filePrevented = false;
              const file = new File([new Uint8Array([1, 2, 3, 4])], 'shot.png', { type: 'image/png' });
              await handleComposerPaste({
                clipboardData: {
                  items: [{ kind: 'file', getAsFile: () => file }],
                  files: [],
                  getData: () => '',
                },
                preventDefault: () => { filePrevented = true; },
              });
              const fileValue = inputEl.value;

              inputEl.value = '';
              inputEl.setSelectionRange(0, 0);
              let textPrevented = false;
              const largeText = 'x'.repeat(LARGE_TEXT_PASTE_CHARS);
              await handleComposerPaste({
                clipboardData: {
                  items: [],
                  files: [],
                  getData: (type) => type === 'text/plain' ? largeText : '',
                },
                preventDefault: () => { textPrevented = true; },
              });
              const textValue = inputEl.value;

              state.currentId = oldCurrent;
              state.ws = oldWs;
              rpc = oldRpc;
              return { filePrevented, textPrevented, fileValue, textValue, calls };
            })()
            "#,
        )
        .await
        .expect("evaluate paste attachment")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(paste_attachment["filePrevented"], true);
    assert_eq!(paste_attachment["textPrevented"], true);
    assert_eq!(paste_attachment["fileValue"], "Look [#file:/tmp/shot.png]");
    assert_eq!(paste_attachment["textValue"], "[#file:/tmp/clipboard.txt]");
    assert_eq!(
        paste_attachment["calls"][0]["method"],
        "session.attach_clipboard"
    );
    assert_eq!(
        paste_attachment["calls"][0]["params"]["session_id"],
        "s-paste"
    );
    assert_eq!(
        paste_attachment["calls"][0]["params"]["filename"],
        "shot.png"
    );
    assert_eq!(paste_attachment["calls"][0]["params"]["mime"], "image/png");
    assert_eq!(paste_attachment["calls"][0]["byteLength"], 4);
    assert_eq!(
        paste_attachment["calls"][1]["params"]["filename"],
        "clipboard.txt"
    );
    assert!(
        paste_attachment["calls"][1]["byteLength"]
            .as_u64()
            .unwrap_or_default()
            >= 16 * 1024
    );

    // Regression coverage for mobile terminal scroll containment:
    // when the native keyboard shrinks the visual viewport, scroll
    // gestures starting on xterm must stay inside the terminal rather
    // than chaining to the app shell and moving the header/list/vkbd.
    let scroll_containment: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const html = getComputedStyle(document.documentElement);
              const body = getComputedStyle(document.body);
              const wrap = getComputedStyle(document.getElementById('terminalWrap'));
              const host = getComputedStyle(document.getElementById('terminal'));
              return {
                htmlOverflow: html.overflow,
                htmlOverscroll: html.overscrollBehavior,
                bodyOverflow: body.overflow,
                bodyOverscroll: body.overscrollBehavior,
                wrapOverflow: wrap.overflow,
                wrapOverscroll: wrap.overscrollBehavior,
                wrapTouchAction: wrap.touchAction,
                hostOverflow: host.overflow,
                hostOverscroll: host.overscrollBehavior,
                hostTouchAction: host.touchAction,
              };
            })()
            "#,
        )
        .await
        .expect("evaluate scroll containment")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(scroll_containment["htmlOverflow"], "hidden");
    assert_eq!(scroll_containment["htmlOverscroll"], "none");
    assert_eq!(scroll_containment["bodyOverflow"], "hidden");
    assert_eq!(scroll_containment["bodyOverscroll"], "none");
    assert_eq!(scroll_containment["wrapOverflow"], "hidden");
    assert_eq!(scroll_containment["wrapOverscroll"], "contain");
    assert_eq!(scroll_containment["wrapTouchAction"], "pan-y");
    assert_eq!(scroll_containment["hostOverflow"], "hidden");
    assert_eq!(scroll_containment["hostOverscroll"], "contain");
    assert_eq!(scroll_containment["hostTouchAction"], "pan-y");

    // Regression coverage for the terminal-specific momentum scroller:
    // xterm's built-in touch path lacks native inertial scrolling on
    // mobile, so the web client installs a custom touch scroller that
    // continues scrollback movement after touchend.
    let momentum_installed: bool = page
        .evaluate(
            "installTerminalScrollContainment(); window.__agentdTerminalMomentumScroll === true",
        )
        .await
        .expect("evaluate momentum scroller hook")
        .into_value::<bool>()
        .expect("bool");
    assert!(
        momentum_installed,
        "terminal momentum scroller was not installed"
    );

    // Overlay buttons are compact terminal-only controls for jumping
    // scrollback to the top or bottom without dragging through a long
    // transcript. Verify they are present, styled as small overlays,
    // and wired to terminal scroll APIs.
    let scroll_buttons: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const overlay = document.getElementById('terminalScrollOverlay');
              const top = document.getElementById('terminalTopBtn');
              const bottom = document.getElementById('terminalBottomBtn');
              const overlayStyle = getComputedStyle(overlay);
              const topStyle = getComputedStyle(top);
              let calls = [];
              state.term = {
                scrollToTop: () => calls.push('top'),
                scrollToBottom: () => calls.push('bottom'),
                focus: () => calls.push('focus'),
              };
              initTerminalScrollButtons();
              top.click();
              bottom.click();
              return {
                topText: top.textContent,
                bottomText: bottom.textContent,
                position: overlayStyle.position,
                top: overlayStyle.top,
                right: overlayStyle.right,
                background: topStyle.backgroundColor,
                fontSize: topStyle.fontSize,
                calls,
                hook: window.__agentdTerminalScrollButtons === true,
              };
            })()
            "#,
        )
        .await
        .expect("evaluate terminal scroll buttons")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(scroll_buttons["topText"], "top");
    assert_eq!(scroll_buttons["bottomText"], "bottom");
    assert_eq!(scroll_buttons["position"], "absolute");
    assert_eq!(scroll_buttons["top"], "8px");
    assert_eq!(scroll_buttons["right"], "8px");
    assert_eq!(scroll_buttons["fontSize"], "11px");
    assert_eq!(scroll_buttons["hook"], true);
    assert!(
        scroll_buttons["background"]
            .as_str()
            .unwrap_or_default()
            .contains("rgba"),
        "expected transparent overlay background, got {scroll_buttons:?}"
    );
    assert_eq!(
        scroll_buttons["calls"],
        serde_json::json!(["top", "focus", "bottom", "focus"])
    );

    // Regression coverage for bottom-following terminal scrollback:
    // opening/closing web UI sheets can trigger xterm fit/resize.
    // If the user was already at the bottom, the fit must keep them
    // there; if they had scrolled up, it must leave their position
    // alone.
    let fit_scroll: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const oldTerm = state.term;
              const oldFit = state.fitAddon;
              const oldMode = state.mode;
              const oldCurrent = state.currentId;
              const oldSessions = state.sessions;
              const oldSize = state.lastReportedSize;
              const oldFollow = state.resizeFollowBottomUntil;
              const oldEngaged = state.webPtyEngagedUntil;
              const oldComposerSuppress = state.composerResizeSuppressPtyResizeUntil;
              const oldTimer = state.ptyResizeTimer;
              const oldPending = state.pendingPtyResize;
              const oldRpc = rpc;
              if (state.ptyResizeTimer) clearTimeout(state.ptyResizeTimer);
              const calls = [];
              rpc = async (method, params) => {
                calls.push({ method, params });
                return null;
              };
              state.mode = 'terminal';
              state.currentId = 's-fit';
              state.lastReportedSize = { cols: 0, rows: 0 };

              const bottom = { viewportY: 30, baseY: 30 };
              state.term = {
                cols: 100,
                rows: 40,
                buffer: { active: bottom },
                scrollToBottom: () => {
                  calls.push('bottom');
                  bottom.viewportY = bottom.baseY;
                },
                write: (_chunk, cb) => {
                  calls.push('write-bottom');
                  bottom.viewportY = 0;
                  if (cb) cb();
                },
              };
              state.fitAddon = {
                fit: () => {
                  calls.push('fit-bottom');
                  bottom.baseY = 80;
                },
              };
              refitTerminal({ claim: true, immediate: true });
              renderEvent({ type: 'pty', data: btoa('resize repaint') });
              await new Promise((resolve) => requestAnimationFrame(resolve));

              const scrolledUp = { viewportY: 10, baseY: 80 };
              state.lastReportedSize = { cols: 0, rows: 0 };
              state.term = {
                cols: 100,
                rows: 40,
                buffer: { active: scrolledUp },
                scrollToBottom: () => {
                  calls.push('unexpected-bottom');
                  scrolledUp.viewportY = scrolledUp.baseY;
                },
                write: (_chunk, cb) => {
                  calls.push('write-scrolled-up');
                  scrolledUp.viewportY = 10;
                  if (cb) cb();
                },
              };
              state.fitAddon = {
                fit: () => {
                  calls.push('fit-scrolled-up');
                  scrolledUp.baseY = 90;
                },
              };
              refitTerminal();
              renderEvent({ type: 'pty', data: btoa('manual scroll repaint') });
              await new Promise((resolve) => requestAnimationFrame(resolve));

              const rowOnly = { viewportY: 20, baseY: 20 };
              state.lastReportedSize = { cols: 100, rows: 40 };
              state.webPtyEngagedUntil = 0;
              state.resizeFollowBottomUntil = 0;
              state.term = {
                cols: 100,
                rows: 25,
                buffer: { active: rowOnly },
                scrollToBottom: () => {
                  calls.push('row-only-bottom');
                  rowOnly.viewportY = rowOnly.baseY;
                },
                write: (_chunk, cb) => {
                  calls.push('row-only-write');
                  if (cb) cb();
                },
              };
              state.fitAddon = {
                fit: () => {
                  calls.push('fit-row-only');
                },
              };
              refitTerminal();
              await new Promise((resolve) => requestAnimationFrame(resolve));

              const activeRowOnly = { viewportY: 20, baseY: 20 };
              state.lastReportedSize = { cols: 100, rows: 40 };
              state.resizeFollowBottomUntil = 0;
              state.term = {
                cols: 100,
                rows: 25,
                buffer: { active: activeRowOnly },
                scrollToBottom: () => {
                  calls.push('active-row-only-bottom');
                  activeRowOnly.viewportY = activeRowOnly.baseY;
                },
                write: (_chunk, cb) => {
                  calls.push('active-row-only-write');
                  if (cb) cb();
                },
              };
              state.fitAddon = {
                fit: () => {
                  calls.push('fit-active-row-only');
                },
              };
              refitTerminal({ claim: true, immediate: true });
              await new Promise((resolve) => requestAnimationFrame(resolve));

              const composerBottom = { viewportY: 42, baseY: 42 };
              state.term = {
                cols: 100,
                rows: 25,
                buffer: { active: composerBottom },
                scrollToBottom: () => {
                  calls.push('composer-bottom');
                  composerBottom.viewportY = composerBottom.baseY;
                },
              };
              input.value = 'line one\nline two';
              resetInputHeightPreservingTerminalScroll();
              await new Promise((resolve) => requestAnimationFrame(resolve));

              const composerScrolledUp = { viewportY: 12, baseY: 42 };
              state.term = {
                cols: 100,
                rows: 25,
                buffer: { active: composerScrolledUp },
                scrollToBottom: () => {
                  calls.push('unexpected-composer-bottom');
                  composerScrolledUp.viewportY = composerScrolledUp.baseY;
                },
              };
              input.value = 'line one\nline two\nline three';
              resetInputHeightPreservingTerminalScroll();
              await new Promise((resolve) => requestAnimationFrame(resolve));

              const composerSuppressed = { viewportY: 55, baseY: 55 };
              state.lastReportedSize = { cols: 100, rows: 40 };
              state.webPtyEngagedUntil = performance.now() + 1000;
              state.term = {
                cols: 100,
                rows: 25,
                buffer: { active: composerSuppressed },
                scrollToBottom: () => {
                  calls.push('composer-suppressed-bottom');
                  composerSuppressed.viewportY = composerSuppressed.baseY;
                },
              };
              state.fitAddon = {
                fit: () => {
                  calls.push('fit-composer-suppressed');
                },
              };
              input.value = 'line one\nline two';
              resetInputHeightPreservingTerminalScroll();
              refitTerminal();
              await new Promise((resolve) => requestAnimationFrame(resolve));

              state.sessions = [{ id: 's-fit', has_pty: true, mode: 'interactive' }];
              state.lastReportedSize = { cols: 100, rows: 40 };
              state.webPtyEngagedUntil = performance.now() + 1000;
              input.value = 'inserted text';
              await submitComposer({ enter: false });

              state.term = oldTerm;
              state.fitAddon = oldFit;
              state.mode = oldMode;
              state.currentId = oldCurrent;
              state.sessions = oldSessions;
              state.lastReportedSize = oldSize;
              state.resizeFollowBottomUntil = oldFollow;
              state.webPtyEngagedUntil = oldEngaged;
              state.composerResizeSuppressPtyResizeUntil = oldComposerSuppress;
              if (state.ptyResizeTimer) clearTimeout(state.ptyResizeTimer);
              state.ptyResizeTimer = oldTimer;
              state.pendingPtyResize = oldPending;
              rpc = oldRpc;
              return { calls, bottom, scrolledUp, composerBottom, composerScrolledUp, composerSuppressed };
            })()
            "#,
        )
        .await
        .expect("evaluate fit scroll preservation")
        .into_value::<serde_json::Value>()
        .expect("json object");
    let calls = fit_scroll["calls"].as_array().cloned().unwrap_or_default();
    let bottom_calls = calls
        .iter()
        .filter(|v| v.as_str() == Some("bottom"))
        .count();
    assert!(
        bottom_calls >= 1,
        "expected refit at bottom to call scrollToBottom, got {fit_scroll:?}"
    );
    assert!(
        calls.iter().any(|v| v.as_str() == Some("write-bottom")),
        "expected delayed PTY repaint after resize to reach xterm: {fit_scroll:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|v| v.as_str() == Some("unexpected-bottom")),
        "refit incorrectly forced a manually-scrolled terminal to bottom: {fit_scroll:?}"
    );
    let row_resize_count = calls
        .iter()
        .filter(|v| {
            v["method"].as_str() == Some("session.pty_resize")
                && v["params"]["cols"].as_i64() == Some(100)
                && v["params"]["rows"].as_i64() == Some(25)
        })
        .count();
    assert_eq!(
        row_resize_count, 1,
        "only the engaged row-only fit should send pty_resize: {fit_scroll:?}"
    );
    assert!(
        calls.iter().any(|v| {
            v["method"].as_str() == Some("session.pty_resize")
                && v["params"]["session_id"].as_str() == Some("s-fit")
                && v["params"]["cols"].as_i64() == Some(100)
                && v["params"]["rows"].as_i64() == Some(25)
        }),
        "engaged row-only terminal fits should claim matching PTY size: {fit_scroll:?}"
    );
    assert_eq!(
        fit_scroll["bottom"]["viewportY"],
        fit_scroll["bottom"]["baseY"]
    );
    assert_ne!(
        fit_scroll["scrolledUp"]["viewportY"], fit_scroll["scrolledUp"]["baseY"],
        "manual scroll position should not be forced to bottom: {fit_scroll:?}"
    );
    assert!(
        calls.iter().any(|v| v.as_str() == Some("composer-bottom")),
        "composer resize should preserve a bottom-following terminal: {fit_scroll:?}"
    );
    assert!(
        !calls
            .iter()
            .any(|v| v.as_str() == Some("unexpected-composer-bottom")),
        "composer resize incorrectly forced a manually-scrolled terminal to bottom: {fit_scroll:?}"
    );
    assert_eq!(
        fit_scroll["composerBottom"]["viewportY"],
        fit_scroll["composerBottom"]["baseY"]
    );
    assert_ne!(
        fit_scroll["composerScrolledUp"]["viewportY"], fit_scroll["composerScrolledUp"]["baseY"],
        "composer resize should preserve manual terminal scroll position: {fit_scroll:?}"
    );
    assert!(
        calls
            .iter()
            .any(|v| v.as_str() == Some("fit-composer-suppressed")),
        "composer-triggered terminal refit should still run locally: {fit_scroll:?}"
    );
    assert_eq!(
        fit_scroll["composerSuppressed"]["viewportY"],
        fit_scroll["composerSuppressed"]["baseY"]
    );
    assert!(
        calls
            .iter()
            .any(|v| v["method"].as_str() == Some("session.pty_input")),
        "Insert should still send PTY input: {fit_scroll:?}"
    );

    // Reconnect regression: mobile keyboard show/hide can churn the browser's
    // viewport and, on some devices, the websocket. Reconnect must not hydrate
    // the selected terminal session through transcript/PTY replay, because that
    // appends old history into xterm and looks like the terminal replayed from
    // the beginning.
    let reconnect_terminal: serde_json::Value = page
        .evaluate(
            r#"
            (async () => {
              const oldTerm = state.term;
              const oldFit = state.fitAddon;
              const oldMode = state.mode;
              const oldCurrent = state.currentId;
              const oldSessions = state.sessions;
              const oldSize = state.lastReportedSize;
              const oldRpc = rpc;
              const calls = [];
              rpc = async (method, params) => {
                calls.push({ method, params });
                return method === 'session.transcript' ? { events: [] } : null;
              };
              state.mode = 'terminal';
              state.currentId = 's-reconnect-terminal';
              state.sessions = [{ id: 's-reconnect-terminal', has_pty: true, mode: 'interactive' }];
              state.lastReportedSize = { cols: 100, rows: 40 };
              state.term = {
                cols: 100,
                rows: 40,
                buffer: { active: { viewportY: 50, baseY: 50 } },
                scrollToBottom: () => calls.push('bottom'),
              };
              state.fitAddon = { fit: () => calls.push('fit') };

              refreshCurrentSessionAfterReconnect();
              await new Promise((resolve) => requestAnimationFrame(resolve));

              state.term = oldTerm;
              state.fitAddon = oldFit;
              state.mode = oldMode;
              state.currentId = oldCurrent;
              state.sessions = oldSessions;
              state.lastReportedSize = oldSize;
              rpc = oldRpc;
              return { calls };
            })()
            "#,
        )
        .await
        .expect("evaluate terminal reconnect")
        .into_value::<serde_json::Value>()
        .expect("json object");
    let reconnect_calls = reconnect_terminal["calls"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        !reconnect_calls.iter().any(|v| {
            matches!(
                v["method"].as_str(),
                Some("session.transcript") | Some("session.pty_replay")
            )
        }),
        "terminal reconnect should not replay transcript/PTY history: {reconnect_terminal:?}"
    );

    // The remote client mirrors zarvis EditorState events in a
    // terminal-mode strip so PTY-backed zarvis input is visible even
    // though zarvis deliberately does not echo its live editor into
    // PTY scrollback. Exercise the renderer directly in the browser so
    // the smoke catches JS/schema regressions without needing a live
    // zarvis adapter in CI.
    page.evaluate(
        r#"
        state.mode = 'terminal';
        renderEditorState({
          type: 'editor_state',
          queued: ['queued prompt'],
          buf: 'hello zarvis',
          cursor: 5,
          completions: ['/help', '/hello']
        }, {
          type: 'agent_status',
          active: true,
          started_at_ms: Date.now() - 2200,
          status: 'Working'
        });
        "#,
    )
    .await
    .expect("render editor_state");
    let editor_text: String = page
        .evaluate("document.getElementById('editorState')?.innerText || ''")
        .await
        .expect("editorState innerText")
        .into_value::<String>()
        .expect("string");
    assert!(
        editor_text.contains("hello zarvis")
            && editor_text.contains("queued prompt")
            && editor_text.contains("/help")
            && editor_text.contains("Working.."),
        "expected editor_state mirror content, got:\n{editor_text}"
    );

    // Headless zarvis streams assistant prose as many Message deltas.
    // Chat-mode rendering should aggregate adjacent assistant deltas
    // into one bubble, while a structured event boundary starts a new
    // assistant bubble for later prose.
    let chat_deltas: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              state.mode = 'chat';
              state.currentId = 's-chat-deltas';
              transcriptEl.innerHTML = '';
              activeTranscriptEl = transcriptEl;
              renderEvent({ type: 'message', role: 'assistant', text: 'Hel' });
              renderEvent({ type: 'message', role: 'assistant', text: 'lo ' });
              renderEvent({ type: 'message', role: 'assistant', text: 'there' });
              renderEvent({ type: 'tool_use', tool: 'shell', args: { command: 'true' } });
              renderEvent({ type: 'message', role: 'assistant', text: 'Done' });
              renderEvent({ type: 'message', role: 'assistant', text: '.' });
              return Array.from(transcriptEl.children).map((row) => ({
                kind: row.dataset.kind || '',
                role: row.dataset.role || '',
                text: row.textContent.trim(),
              }));
            })()
            "#,
        )
        .await
        .expect("evaluate chat delta aggregation")
        .into_value::<serde_json::Value>()
        .expect("json array");
    let chat_rows = chat_deltas.as_array().cloned().unwrap_or_default();
    assert_eq!(
        chat_rows.len(),
        3,
        "expected assistant deltas to coalesce around tool boundary: {chat_deltas:?}"
    );
    assert_eq!(chat_rows[0]["kind"].as_str(), Some("message"));
    assert_eq!(chat_rows[0]["role"].as_str(), Some("assistant"));
    assert!(
        chat_rows[0]["text"]
            .as_str()
            .unwrap_or_default()
            .contains("Hello there"),
        "first assistant bubble should contain concatenated deltas: {chat_deltas:?}"
    );
    let second_assistant = chat_rows[2]["text"].as_str().unwrap_or_default();
    assert!(
        second_assistant.contains("Done."),
        "second assistant bubble should aggregate after boundary: {chat_deltas:?}"
    );

    // Tool-call rendering in terminal mode (issue #134): zarvis emits
    // tool calls as structured events, not PTY bytes, so the xterm view
    // showed nothing for them. `renderEvent` now synthesizes an inline
    // representation. Mock `state.term.write` to capture what reaches the
    // terminal and drive a few events through the real handler.
    let tool_render: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const calls = [];
              state.term = { write: (s) => calls.push(s), reset: () => {} };
              state.mode = 'terminal';
              state.ptyBuffering = false;
              state.currentId = 'sX';
              renderEvent({ type: 'tool_use', tool: 'shell', args: { command: 'ls -la /tmp' } });
              renderEvent({ type: 'tool_result', tool: 'c1', ok: true, output: 'a.txt\nb.txt\nc.txt' });
              renderEvent({ type: 'tool_result', tool: 'c2', ok: false, output: '' });
              // Agent-supplied text must not be able to inject ANSI.
              renderEvent({ type: 'tool_use', tool: 'evil\x1b[31m', args: {} });
              const raw = calls.join('');
              const stripped = raw.replace(/\x1b\[[0-9;]*m/g, '');
              return { text: stripped, hasRawEsc: /\x1b/.test(stripped) };
            })()
            "#,
        )
        .await
        .expect("evaluate tool render")
        .into_value::<serde_json::Value>()
        .expect("json object");
    let tool_text = tool_render["text"].as_str().unwrap_or_default();
    assert!(
        tool_text.contains("→ shell")
            && tool_text.contains("command: ls -la /tmp")
            && tool_text.contains("✓")
            && tool_text.contains("a.txt")
            && tool_text.contains("[+2 more lines]")
            && tool_text.contains("✗")
            && tool_text.contains("(no output)")
            && tool_text.contains("→ evil"),
        "expected synthesized tool-call rendering, got:\n{tool_text}"
    );
    assert_eq!(
        tool_render["hasRawEsc"], false,
        "agent-supplied tool text leaked a raw ESC into the terminal (ANSI injection)"
    );

    // Historical hydration (issue #134): switching to a zarvis session
    // replays its transcript into xterm so PAST tool calls show, not just
    // live ones. Drive `replayTranscriptToTerm` with a synthetic
    // transcript and confirm prose + tool blocks render in order, while
    // prose-bearing structured events (message) are skipped (their text
    // is already in the PTY events — rendering them would double up).
    let replay: String = page
        .evaluate(
            r#"
            (() => {
              const calls = [];
              // Mimic xterm: decode byte chunks (Uint8Array) to text the
              // way the real terminal's UTF-8 decoder would.
              const dec = new TextDecoder();
              state.term = {
                write: (s) => calls.push(typeof s === 'string' ? s : dec.decode(s)),
                reset: () => {},
                resize: () => {},
              };
              state.currentId = 'sH';
              replayTranscriptToTerm([
                { event: { type: 'pty', data: btoa('hello from agent\r\n') } },
                { event: { type: 'tool_use', tool: 'shell', args: { command: 'echo hi' } } },
                { event: { type: 'tool_result', tool: 'c1', ok: true, output: 'hi there' } },
                { event: { type: 'message', role: 'assistant', text: 'SHOULD_NOT_DOUBLE' } },
                { event: { type: 'pty', data: btoa('all done\r\n') } },
              ]);
              return calls.join('').replace(/\x1b\[[0-9;]*m/g, '');
            })()
            "#,
        )
        .await
        .expect("evaluate transcript replay")
        .into_value::<String>()
        .expect("string");
    assert!(
        replay.contains("hello from agent")
            && replay.contains("→ shell")
            && replay.contains("command: echo hi")
            && replay.contains("✓")
            && replay.contains("hi there")
            && replay.contains("all done"),
        "expected transcript replay to render prose + tool blocks, got:\n{replay}"
    );
    assert!(
        !replay.contains("SHOULD_NOT_DOUBLE"),
        "message event was rendered in terminal mode — prose would double up:\n{replay}"
    );

    // Web widgets mirror the TUI's temporary reveal behavior: a live
    // ui_panel update opens the widget popover for the active session,
    // then the autohide deadline closes it unless the user pins it by
    // hover/focus. Updates for background sessions keep their deadline
    // and reveal if the user switches there before it expires.
    let widget_autoshow: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const panel = document.getElementById('sessionWidgets');
              state.currentId = 'sWidget';
              state.mode = 'chat';
              state.widgetsById.delete('sWidget');
              state.widgetsById.delete('sBgWidget');
              state.widgetTemporaryUntilById.delete('sWidget');
              state.widgetTemporaryUntilById.delete('sBgWidget');
              state.widgetsDropdownOpen = false;

              applyWidgetPanel('sWidget', {
                id: 'progress',
                source: 'progress.md',
                markdown: '# Progress\n- [~] Working'
              });
              const out = {
                shownAfterUpdate: !panel.hidden,
                expandedAfterUpdate: document.getElementById('widgetsTrigger').getAttribute('aria-expanded'),
                hasTemporaryDeadline: state.widgetTemporaryUntilById.has('sWidget'),
                renderedText: panel.textContent,
                hideButtonText: panel.querySelector('.widget-hide')?.textContent || '',
                menuDisplay: getComputedStyle(panel.querySelector('.widgets-menu')).display,
              };
              panel.querySelector('.widgets-menu-item[data-widget-id="progress"]').click();
              out.openAfterMenuUncheck = !panel.hidden;
              out.expandedAfterMenuUncheck = document.getElementById('widgetsTrigger').getAttribute('aria-expanded');
              out.hiddenAfterMenuUncheck = !panel.textContent.includes('Working');
              out.menuUncheckedAfterMenuUncheck = panel.querySelector('.widgets-menu-item[data-widget-id="progress"]')?.getAttribute('aria-checked');
              panel.querySelector('.widgets-menu-item[data-widget-id="progress"]').click();
              out.openAfterMenuRecheck = !panel.hidden;
              out.visibleAfterMenuRecheck = panel.textContent.includes('Working');
              out.menuCheckedAfterMenuRecheck = panel.querySelector('.widgets-menu-item[data-widget-id="progress"]')?.getAttribute('aria-checked');
              panel.querySelector('.widget-hide').click();
              out.hiddenByButton = !panel.textContent.includes('Working');
              out.menuUncheckedAfterHide = panel.querySelector('.widgets-menu-item[data-widget-id="progress"]')?.getAttribute('aria-checked');
              out.persistedVisibleAfterHide = JSON.parse(localStorage.getItem(widgetStorageKey('sWidget')) || '[]');

              saveVisibleWidgetIds('sWidget', new Set(['progress']));
              renderWidgets();
              state.widgetTemporaryUntilById.set('sWidget', performance.now() - 1);
              scheduleWidgetAutohide();
              out.hiddenAfterDeadline = panel.hidden;
              out.closedAfterDeadline = !state.widgetsDropdownOpen;

              state.currentId = 'sOther';
              state.widgetsDropdownOpen = false;
              renderWidgets();
              applyWidgetPanel('sBgWidget', {
                id: 'background',
                source: 'background.md',
                markdown: '# Background\n- [~] Updated'
              });
              out.backgroundDidNotStealView = state.currentId === 'sOther' && panel.hidden;
              state.currentId = 'sBgWidget';
              renderWidgets();
              out.backgroundShownOnSwitch = !panel.hidden;
              out.backgroundText = panel.textContent;
              return out;
            })()
            "#,
        )
        .await
        .expect("evaluate widget autoshow")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(widget_autoshow["shownAfterUpdate"], true);
    assert_eq!(widget_autoshow["expandedAfterUpdate"], "true");
    assert_eq!(widget_autoshow["hasTemporaryDeadline"], true);
    assert!(
        widget_autoshow["renderedText"]
            .as_str()
            .unwrap_or_default()
            .contains("Working"),
        "updated widget body was not rendered: {widget_autoshow:?}"
    );
    assert_eq!(widget_autoshow["hideButtonText"], "[-]");
    assert_eq!(widget_autoshow["menuDisplay"], "grid");
    assert_eq!(widget_autoshow["openAfterMenuUncheck"], true);
    assert_eq!(widget_autoshow["expandedAfterMenuUncheck"], "true");
    assert_eq!(widget_autoshow["hiddenAfterMenuUncheck"], true);
    assert_eq!(widget_autoshow["menuUncheckedAfterMenuUncheck"], "false");
    assert_eq!(widget_autoshow["openAfterMenuRecheck"], true);
    assert_eq!(widget_autoshow["visibleAfterMenuRecheck"], true);
    assert_eq!(widget_autoshow["menuCheckedAfterMenuRecheck"], "true");
    assert_eq!(widget_autoshow["hiddenByButton"], true);
    assert_eq!(widget_autoshow["menuUncheckedAfterHide"], "false");
    assert_eq!(
        widget_autoshow["persistedVisibleAfterHide"]
            .as_array()
            .map(|items| items.is_empty()),
        Some(true)
    );
    assert_eq!(widget_autoshow["hiddenAfterDeadline"], true);
    assert_eq!(widget_autoshow["closedAfterDeadline"], true);
    assert_eq!(widget_autoshow["backgroundDidNotStealView"], true);
    assert_eq!(widget_autoshow["backgroundShownOnSwitch"], true);
    assert!(
        widget_autoshow["backgroundText"]
            .as_str()
            .unwrap_or_default()
            .contains("Updated"),
        "background widget was not rendered after switching sessions: {widget_autoshow:?}"
    );

    // Browser preview (issue: TUI parity). Delivered by the LIVE WS
    // notification path (`handleNotification`), stored per session, and
    // shown as a top-right overlay ANCHORED OVER THE TERMINAL (a child of
    // #terminalWrap, position:absolute), with a caption + close button,
    // not the old separate strip below the terminal. The × dismisses it
    // and drops the stored entry. Previews are EPHEMERAL: replaying the
    // transcript (`renderEvent`, as `loadTranscript` does on reload) must
    // NOT resurrect a stale thumbnail.
    let browser_preview: serde_json::Value = page
        .evaluate(
            r#"
            (() => {
              const png1x1 = 'iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mP8/x8AAwMCAO+/p9sAAAAASUVORK5CYII=';
              state.currentId = 'sBrowser';
              const evt = {
                type: 'browser_preview',
                url: 'https://example.test/page',
                title: 'Example Preview',
                image: png1x1,
                width: 1,
                height: 1
              };
              // Live path: arrives as a session/event notification.
              handleNotification('session/event', {
                session_id: 'sBrowser', event: evt, at: 0,
              });
              const panel = document.getElementById('browserPreview');
              const img = document.getElementById('browserPreviewImg');
              const caption = document.getElementById('browserPreviewCaption');
              const cs = getComputedStyle(panel);
              const out = {
                shown: !panel.hidden,
                parentId: panel.parentElement.id,
                position: cs.position,
                top: cs.top,
                srcPrefix: img.getAttribute('src').slice(0, 21),
                caption: caption.textContent.trim(),
                hasClose: !!document.getElementById('browserPreviewClose'),
              };
              // Dismiss via the × and confirm it hides + forgets the entry.
              document.getElementById('browserPreviewClose').click();
              out.hiddenAfterClose = panel.hidden;
              out.storedAfterClose = state.browserPreviewById.has('sBrowser');
              // Ephemeral: replaying the same event through the transcript
              // path must NOT bring the thumbnail back.
              renderEvent(evt);
              out.shownAfterReplay = !panel.hidden;
              out.storedAfterReplay = state.browserPreviewById.has('sBrowser');
              return out;
            })()
            "#,
        )
        .await
        .expect("evaluate browser preview")
        .into_value::<serde_json::Value>()
        .expect("json object");
    assert_eq!(browser_preview["shown"], true);
    assert_eq!(
        browser_preview["parentId"], "terminalWrap",
        "overlay anchored over the terminal"
    );
    assert_eq!(browser_preview["position"], "absolute");
    assert_eq!(browser_preview["top"], "8px", "top-right corner");
    assert_eq!(browser_preview["srcPrefix"], "data:image/png;base64");
    assert_eq!(browser_preview["caption"], "Example Preview");
    assert_eq!(browser_preview["hasClose"], true);
    assert_eq!(
        browser_preview["hiddenAfterClose"], true,
        "× dismisses the overlay"
    );
    assert_eq!(
        browser_preview["storedAfterClose"], false,
        "× forgets the stored preview"
    );
    assert_eq!(
        browser_preview["shownAfterReplay"], false,
        "transcript replay must not resurrect a closed/ephemeral preview"
    );
    assert_eq!(
        browser_preview["storedAfterReplay"], false,
        "transcript replay must not re-store an ephemeral preview"
    );

    // Pause briefly so the final rendered state lands in the
    // video before we stop the screencast — otherwise reviewers
    // see the page mid-load with no payoff frame.
    tokio::time::sleep(Duration::from_millis(500)).await;
    drop(recording);
}

/// Handle returned by `start_screencast` — keeps a background
/// frame-receiver task alive until `Drop`, at which point it
/// stops the screencast, flushes any in-flight frames, and runs
/// ffmpeg to assemble an MP4. ffmpeg failing (e.g. not installed)
/// is logged but doesn't fail the test — the per-frame JPEGs
/// remain under `artifact_dir/<name>_frames/` as a fallback.
struct ScreencastRecording {
    page: Page,
    frames_dir: std::path::PathBuf,
    mp4_path: std::path::PathBuf,
    frame_count: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    task: Option<tokio::task::JoinHandle<()>>,
}

impl Drop for ScreencastRecording {
    fn drop(&mut self) {
        // Stop the screencast (best-effort — page may already be
        // gone if the test panicked) and abort the receiver task.
        // Both happen on a one-shot blocking thread because Drop
        // can't be async.
        let page = self.page.clone();
        let task = self.task.take();
        let frames_dir = self.frames_dir.clone();
        let mp4_path = self.mp4_path.clone();
        let frame_count = self.frame_count.clone();
        // Use a separate thread because Drop is sync, but we need
        // tokio to send the stop command + give time for the
        // frames stream to drain.
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .unwrap();
            rt.block_on(async {
                let _ = page.execute(StopScreencastParams::default()).await;
                // Let in-flight frame events drain into the
                // receiver before we abort it.
                tokio::time::sleep(Duration::from_millis(300)).await;
                if let Some(t) = task {
                    t.abort();
                    let _ = t.await;
                }
            });
            let count = frame_count.load(std::sync::atomic::Ordering::SeqCst);
            eprintln!(
                "screencast: captured {count} frame(s) at {}",
                frames_dir.display()
            );
            run_ffmpeg(&frames_dir, &mp4_path);
        })
        .join()
        .ok();
    }
}

/// Subscribe to `Page.screencastFrame` events, start the
/// screencast in JPEG mode, and spawn a task that writes each
/// frame to `<artifact_dir>/<name>_frames/frame_NNNN.jpg`
/// (zero-padded so ffmpeg's image2 demuxer can sequence them).
async fn start_screencast(page: &Page, name: &str) -> anyhow::Result<ScreencastRecording> {
    let frames_dir = artifact_dir()?.join(format!("{name}_frames"));
    let _ = std::fs::remove_dir_all(&frames_dir);
    std::fs::create_dir_all(&frames_dir)?;

    let mut events = page.event_listener::<EventScreencastFrame>().await?;
    let frame_count = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0));

    let receiver_page = page.clone();
    let receiver_dir = frames_dir.clone();
    let receiver_count = frame_count.clone();
    let task = tokio::spawn(async move {
        while let Some(ev) = events.next().await {
            let raw: &str = ev.data.as_ref();
            // Each frame is base64-encoded JPEG. Decode + write to
            // disk so ffmpeg's image2 demuxer can consume them.
            let Ok(jpeg) = B64.decode(raw) else { continue };
            let idx = receiver_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            let path = receiver_dir.join(format!("frame_{:04}.jpg", idx));
            if std::fs::write(&path, &jpeg).is_err() {
                continue;
            }
            // Ack so the next frame is scheduled — without this
            // Chromium throttles to ~1 frame and the video is
            // basically a still image.
            let ack = ScreencastFrameAckParams {
                session_id: ev.session_id,
            };
            let _ = receiver_page.execute(ack).await;
        }
    });

    page.execute(
        StartScreencastParams::builder()
            .format(StartScreencastFormat::Jpeg)
            .quality(70)
            .every_nth_frame(1)
            .build(),
    )
    .await?;

    let mp4_path = artifact_dir()?.join(format!("{name}.mp4"));
    Ok(ScreencastRecording {
        page: page.clone(),
        frames_dir,
        mp4_path,
        frame_count,
        task: Some(task),
    })
}

/// Assemble JPEG frames into an MP4 via ffmpeg. ffmpeg missing
/// or failing is logged but not fatal — the per-frame JPEGs
/// remain on disk as a fallback artifact.
fn run_ffmpeg(frames_dir: &Path, mp4_path: &Path) {
    let pattern = frames_dir.join("frame_%04d.jpg");
    // Chromium emits screencast frames only on visual change, so
    // a 2-second test typically produces a handful of frames.
    // Play them back at 2 fps so each rendered frame is visible
    // for ~500 ms rather than flashing past.
    //
    // The `pad` filter rounds the resolution up to the next even
    // pixel count — libx264 + yuv420p requires both dimensions
    // to be divisible by 2, and Chromium ships odd dimensions
    // (e.g. 800x441) for the captured viewport.
    let output = std::process::Command::new("ffmpeg")
        .args(["-y", "-framerate", "2", "-i"])
        .arg(&pattern)
        .args([
            "-vf",
            "pad=ceil(iw/2)*2:ceil(ih/2)*2",
            "-c:v",
            "libx264",
            "-pix_fmt",
            "yuv420p",
        ])
        .arg(mp4_path)
        .stderr(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .status();
    match output {
        Ok(s) if s.success() => {
            eprintln!("screencast: wrote {}", mp4_path.display());
        }
        Ok(s) => {
            eprintln!(
                "screencast: ffmpeg exited {s}; keeping raw frames at {}",
                frames_dir.display()
            );
        }
        Err(e) => {
            eprintln!(
                "screencast: ffmpeg not available ({e}); keeping raw frames at {}",
                frames_dir.display()
            );
        }
    }
}

/// Inject `user:password@` userinfo into the authority of an
/// `http://` URL. Doesn't touch the path or fragment. Cheap
/// hand-rolled splitter (avoids pulling in a URL crate just for
/// one test).
fn inject_userinfo(url: &str, user: &str, pw: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("http://{user}:{pw}@{rest}")
    } else if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{user}:{pw}@{rest}")
    } else {
        url.to_string()
    }
}
