//! End-to-end: drive the web client's **Program view** in a real headless
//! Chromium (spec 0059). Exercises the full surface — enter/render, templates,
//! edit + save, optimistic-version 3-way merge on conflict, run + run-selection
//! shimmer, smart-clip (@) autocomplete, find, and live `program/state` adopt
//! vs. keep-dirty — plus a real daemon round-trip that proves the JS block-id
//! parser agrees with the daemon byte-for-byte.
//!
//! Skipped (not failed) when Chrome / Chromium isn't installed, matching
//! `web_smoke.rs`.

use std::time::{Duration, Instant};

use agentd_e2e::{artifact_dir, Daemon};
use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::{Page, ScreenshotParams};
use futures::StreamExt;

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn web_program_view_full_parity() {
    let d = Daemon::spawn().await.expect("daemon");
    let r = d
        .client
        .remote_start(/* local_only */ true, /* password */ None)
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
            eprintln!(
                "skipping program_view: could not launch Chromium ({e}). \
                 Install Google Chrome to run this test locally."
            );
            return;
        }
    };
    let _handler_task = tokio::spawn(async move { while handler.next().await.is_some() {} });

    let page = browser.new_page("about:blank").await.expect("new page");
    let url = inject_userinfo(&r.url, "remote", &r.password);
    page.goto(&url).await.expect("goto");
    wait_conn_open(&page).await;

    // Inject the shared mock helpers once (used by every mocked-ws block below).
    page.evaluate(SETUP_JS).await.expect("inject test helpers");

    // --- 1. Real daemon round-trip: JS block ids must equal the daemon's
    //        legacy content ids, and program.get/update over the web WS must
    //        actually work. Stable daemon refs travel in block.id. ------------
    let parity: serde_json::Value = page
        .evaluate(
            r###"
            (async () => {
              const md = "# Plan\n- step one @{session:s1 clip_id=clip_3}\n- step two\n\nA paragraph\nwrapped across lines\n";
              const created = await rpc("session.create", { harness: "shell", cwd: "/tmp", prompt: "" });
              const sid = created.session_id;
              await rpc("program.update", { session_id: sid, markdown: md });
              const got = await rpc("program.get", { session_id: sid });
              return {
                daemonIds: (got.blocks || []).map((b) => b.id),
                daemonContentIds: (got.blocks || []).map((b) => b.content_id),
                jsIds: programBlockSpans(got.program.markdown).map((b) => b.id),
                markdownRoundTripped: got.program.markdown === md,
              };
            })()
            "###,
        )
        .await
        .expect("evaluate block-id parity")
        .into_value()
        .expect("json");
    assert_eq!(
        parity["daemonContentIds"], parity["jsIds"],
        "JS block ids must match the daemon's legacy content ids: {parity:?}"
    );
    assert!(
        parity["daemonIds"].as_array().map(|a| a.len()).unwrap_or(0) >= 4,
        "expected several blocks: {parity:?}"
    );
    assert_eq!(parity["markdownRoundTripped"], true, "{parity:?}");

    // --- 2. Enter Program mode renders the document + toggle state. ----------
    let enter: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-prog", markdown: "# Title\n- task a\n- task b\n", version: 7, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              setSession("s-prog", "shell");
              await switchCurrentViewMode("program");
              return {
                wrapVisible: !programWrapEl.hidden,
                transcriptHidden: transcriptEl.hidden,
                value: programSerialize(),
                version: programVersionEl.textContent,
                programPressed: viewModeProgramBtn.getAttribute("aria-pressed"),
                mode: state.mode,
                mounted: state.program.mountedId,
              };
            })
            "###,
        )
        .await
        .expect("evaluate enter program")
        .into_value()
        .expect("json");
    assert_eq!(enter["wrapVisible"], true, "{enter:?}");
    assert_eq!(enter["transcriptHidden"], true, "{enter:?}");
    assert_eq!(enter["value"], "# Title\n- task a\n- task b\n", "{enter:?}");
    assert_eq!(enter["version"], "v7", "{enter:?}");
    assert_eq!(enter["programPressed"], "true", "{enter:?}");
    assert_eq!(enter["mode"], "program", "{enter:?}");
    assert_eq!(enter["mounted"], "s-prog", "{enter:?}");

    // --- 3. Empty program shows templates; clicking one seeds the doc. -------
    let templates: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-empty", markdown: "", version: 0, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [
                { id: "blank", name: "Blank", markdown: "", built_in: true },
                { id: "tasks", name: "Tasks", description: "A todo board", markdown: "## Todo\n- first\n", built_in: true },
              ] }),
              "program.update": (p) => ({ program: { session_id: "s-empty", markdown: p.markdown, version: 1, template_id: p.template_id || null }, blocks: [], active_run: null }),
            }, async () => {
              window.__updates = [];
              const realSend = state.ws.send.bind(state.ws);
              state.ws.send = (raw) => { const m = JSON.parse(raw); if (m.method === "program.update") window.__updates.push(m.params); realSend(raw); };
              setSession("s-empty", "shell");
              await switchCurrentViewMode("program");
              await new Promise((r) => setTimeout(r, 40));
              const emptyVisible = !programEmptyEl.hidden;
              const tmplButtons = Array.from(programEmptyEl.querySelectorAll("[data-tmpl]")).map((b) => b.dataset.tmpl);
              programEmptyEl.querySelector('[data-tmpl="tasks"]').click();
              await new Promise((r) => setTimeout(r, 40));
              return { emptyVisible, tmplButtons, updates: window.__updates, valueAfter: programSerialize(), emptyHiddenAfter: programEmptyEl.hidden };
            })
            "###,
        )
        .await
        .expect("evaluate templates")
        .into_value()
        .expect("json");
    assert_eq!(templates["emptyVisible"], true, "{templates:?}");
    assert_eq!(
        templates["tmplButtons"],
        serde_json::json!(["tasks"]),
        "{templates:?}"
    );
    assert_eq!(
        templates["updates"][0]["template_id"], "tasks",
        "{templates:?}"
    );
    assert_eq!(
        templates["updates"][0]["markdown"], "## Todo\n- first\n",
        "{templates:?}"
    );
    assert_eq!(
        templates["valueAfter"], "## Todo\n- first\n",
        "{templates:?}"
    );
    assert_eq!(templates["emptyHiddenAfter"], true, "{templates:?}");

    // --- 4. Edit + clean save sends program.update with the base version. ----
    let save: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-save", markdown: "old\n", version: 3, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
              "program.update": (p) => { window.__updates.push(p); return { program: { session_id: "s-save", markdown: p.markdown, version: 4, template_id: null }, blocks: [], active_run: null }; },
            }, async () => {
              window.__updates = [];
              setSession("s-save", "shell");
              await switchCurrentViewMode("program");
              programTestSet("old\nnew line\n");
              const dirtyBefore = programSaveBtn.getAttribute("data-dirty");
              await programSave();
              return { dirtyBefore, dirtyAfter: programSaveBtn.getAttribute("data-dirty"), updates: window.__updates, versionAfter: programVersionEl.textContent, msg: programMsgEl.textContent };
            })
            "###,
        )
        .await
        .expect("evaluate save")
        .into_value()
        .expect("json");
    assert_eq!(save["dirtyBefore"], "true", "{save:?}");
    assert_eq!(save["dirtyAfter"], "false", "{save:?}");
    assert_eq!(save["updates"][0]["base_version"], 3, "{save:?}");
    assert_eq!(
        save["updates"][0]["markdown"], "old\nnew line\n",
        "{save:?}"
    );
    assert_eq!(save["versionAfter"], "v4", "{save:?}");
    assert!(
        save["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("saved v4"),
        "{save:?}"
    );

    // --- 5. Save conflict → clean 3-way merge of non-overlapping edits. ------
    let merge: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              window.__updates = [];
              let firstUpdate = true;
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-merge", markdown: "L1\nL2\nL3\n", version: 5, template_id: null }, active_run: null, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.update"] = (p) => {
                window.__updates.push(p);
                if (firstUpdate) { firstUpdate = false; throw new Error("program conflict: current version is 6, attempted base version is 5"); }
                return { program: { session_id: "s-merge", markdown: p.markdown, version: 7, template_id: null }, blocks: [], active_run: null };
              };
              setSession("s-merge", "shell");
              await switchCurrentViewMode("program");
              // local change to line 1; concurrent agent change to line 3.
              programTestSet("OURS\nL2\nL3\n");
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-merge", markdown: "L1\nL2\nTHEIRS\n", version: 6, template_id: null }, active_run: null, blocks: [], revisions: [] });
              await programSave();
              return { updates: window.__updates, finalValue: programSerialize(), msg: programMsgEl.textContent };
            })
            "###,
        )
        .await
        .expect("evaluate merge")
        .into_value()
        .expect("json");
    assert_eq!(
        merge["updates"][1]["markdown"], "OURS\nL2\nTHEIRS\n",
        "{merge:?}"
    );
    assert_eq!(merge["updates"][1]["base_version"], 6, "{merge:?}");
    assert_eq!(merge["finalValue"], "OURS\nL2\nTHEIRS\n", "{merge:?}");
    assert!(
        merge["msg"].as_str().unwrap_or_default().contains("merged"),
        "{merge:?}"
    );

    // --- 6. Run dispatches execute and shimmers the program's blocks. --------
    let run: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const md = "# Heading\n- alpha\n- beta\n";
              window.__execs = [];
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-run", markdown: md, version: 2, template_id: null }, active_run: null, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.execute"] = (p) => {
                window.__execs.push(p);
                const ids = programBlockSpans(md).map((b) => b.id);
                const now = Date.now();
                return { program: { session_id: "s-run", markdown: md, version: 2, template_id: null }, blocks: [], active_run: { run_id: "r1", started_at_ms: now, expires_at_ms: now + 60000, pending_block_ids: ids, pending_block_tooltips: {}, seen_running: false, first_output_seen: false, agent_managed: false } };
              };
              setSession("s-run", "shell");
              await switchCurrentViewMode("program");
              programTestClearSel();
              await programRun();
              const r = state.program.runById.get("s-run");
              return { execs: window.__execs, runPending: r ? r.pendingIds.size : 0, shimmerActive: !!programInputEl.querySelector(".program-line.is-running"), stage: programRunStageEl.textContent, msg: programMsgEl.textContent };
            })
            "###,
        )
        .await
        .expect("evaluate run")
        .into_value()
        .expect("json");
    assert_eq!(
        run["execs"][0]["selection"],
        serde_json::Value::Null,
        "whole-program run sends no selection: {run:?}"
    );
    assert!(run["runPending"].as_u64().unwrap_or(0) >= 3, "{run:?}");
    assert_eq!(
        run["shimmerActive"], true,
        "running lines should get the shimmer class: {run:?}"
    );
    assert_eq!(run["stage"], "delivered", "{run:?}");
    assert!(
        run["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("run sent (program"),
        "{run:?}"
    );

    // --- 6a. Run gives immediate optimistic affordance before execute returns.
    let immediate_run: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const md = "# Heading\n- alpha\n- beta\n";
              window.__execs = [];
              let resolveExecute;
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-run-immediate", markdown: md, version: 2, template_id: null }, active_run: null, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.execute"] = (p) => {
                window.__execs.push(p);
                return new Promise((resolve) => {
                  resolveExecute = () => {
                    const ids = programBlockSpans(md).map((b) => b.id);
                    const now = Date.now();
                    resolve({ program: { session_id: "s-run-immediate", markdown: md, version: 2, template_id: null }, blocks: [], active_run: { run_id: "r-immediate", started_at_ms: now, expires_at_ms: now + 60000, pending_block_ids: ids, pending_block_tooltips: {}, seen_running: false, first_output_seen: false, agent_managed: false } });
                  };
                });
              };
              setSession("s-run-immediate", "shell");
              await switchCurrentViewMode("program");
              programTestClearSel();
              const runPromise = programRun();
              await new Promise((resolve) => requestAnimationFrame(resolve));
              const before = {
                execCount: window.__execs.length,
                runPending: state.program.runById.get("s-run-immediate")?.pendingIds.size || 0,
                shimmerActive: !!programInputEl.querySelector(".program-line.is-running"),
                button: programRunBtn.dataset.running,
                stage: programRunStageEl.textContent,
                msg: programMsgEl.textContent,
              };
              resolveExecute();
              await runPromise;
              const after = {
                runPending: state.program.runById.get("s-run-immediate")?.pendingIds.size || 0,
                shimmerActive: !!programInputEl.querySelector(".program-line.is-running"),
                button: programRunBtn.dataset.running,
                stage: programRunStageEl.textContent,
                msg: programMsgEl.textContent,
              };
              return { before, after };
            })
            "###,
        )
        .await
        .expect("evaluate immediate optimistic run")
        .into_value()
        .expect("json");
    assert_eq!(immediate_run["before"]["execCount"], 1, "{immediate_run:?}");
    assert!(
        immediate_run["before"]["runPending"].as_u64().unwrap_or(0) >= 3,
        "{immediate_run:?}"
    );
    assert_eq!(
        immediate_run["before"]["shimmerActive"], true,
        "shimmer should be active before execute resolves: {immediate_run:?}"
    );
    assert_eq!(
        immediate_run["before"]["button"], "true",
        "Run button should pulse before execute resolves: {immediate_run:?}"
    );
    assert_eq!(
        immediate_run["before"]["stage"], "pressed",
        "{immediate_run:?}"
    );
    assert_eq!(
        immediate_run["after"]["stage"], "delivered",
        "{immediate_run:?}"
    );
    assert!(
        immediate_run["before"]["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("running program"),
        "{immediate_run:?}"
    );
    assert!(
        immediate_run["after"]["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("run sent (program"),
        "{immediate_run:?}"
    );

    // --- 6b. Mid-flight re-Run preserves narrowed shimmer and pulses Run. ----
    let rerun: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const oldMd = "# Heading\n- settled\n- pending\n";
              const newMd = "# Heading\n- changed settled\n- pending\n";
              const pendingId = programBlockSpans(oldMd).find((b) => oldMd.split("\n").slice(b.start_line, b.end_line).join("\n").includes("pending")).id;
              const now = Date.now();
              const priorRun = { run_id: "r-old", started_at_ms: now - 2000, expires_at_ms: now + 60000, pending_block_ids: [pendingId], pending_block_tooltips: {}, seen_running: true, first_output_seen: true, agent_managed: true };
              window.__execs = [];
              window.__updates = [];
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-rerun", markdown: oldMd, version: 2, template_id: null }, active_run: priorRun, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.update"] = (p) => {
                window.__updates.push(p);
                return { program: { session_id: "s-rerun", markdown: p.markdown, version: 3, template_id: null }, blocks: [], active_run: priorRun };
              };
              window.__mockProgramHandlers["program.execute"] = (p) => {
                window.__execs.push(p);
                const ids = programBlockSpans(newMd).map((b) => b.id).filter((_, i) => p.shimmer && p.shimmer[i]);
                const t = Date.now();
                return { program: { session_id: "s-rerun", markdown: newMd, version: 3, template_id: null }, blocks: [], active_run: { run_id: "r-new", started_at_ms: t, expires_at_ms: t + 60000, pending_block_ids: ids, pending_block_tooltips: {}, seen_running: false, first_output_seen: false, agent_managed: false } };
              };
              setSession("s-rerun", "shell");
              await switchCurrentViewMode("program");
              programTestClearSel();
              programTestSet(newMd);
              await programRun();
              const run = state.program.runById.get("s-rerun");
              const pendingTexts = programBlockSpans(newMd)
                .filter((b) => run && run.pendingIds.has(b.id))
                .map((b) => newMd.split("\n").slice(b.start_line, b.end_line).join("\n"));
              const beforeTool = programRunBtn.dataset.running;
              handleNotification("session/event", { session_id: "s-rerun", event: { type: "tool_use", tool: "shell", args: {} } });
              return {
                updates: window.__updates,
                execs: window.__execs,
                pendingTexts,
                beforeTool,
                afterTool: programRunBtn.dataset.running,
              };
            })
            "###,
        )
        .await
        .expect("evaluate rerun")
        .into_value()
        .expect("json");
    assert_eq!(
        rerun["execs"][0]["shimmer"],
        serde_json::json!([false, true, true]),
        "re-run should shimmer only changed + still-pending blocks: {rerun:?}"
    );
    assert_eq!(
        rerun["pendingTexts"],
        serde_json::json!(["- changed settled", "- pending"]),
        "optimistic run should keep old pending and add the user edit: {rerun:?}"
    );
    assert_eq!(
        rerun["beforeTool"], "true",
        "Run button should pulse until tool output: {rerun:?}"
    );
    assert_eq!(
        rerun["afterTool"], "false",
        "tool_use should clear Run button pulse: {rerun:?}"
    );

    // --- 6c. A double programRun() (double-click / Run button + Ctrl+Enter)
    //         must dispatch exactly one execute turn (spec 0042 consequence:
    //         Run overlap/idempotency guard). ------------------------------
    let double_run: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const md = "# Heading\n- alpha\n- beta\n";
              window.__execs = [];
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-double-run", markdown: md, version: 2, template_id: null }, active_run: null, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.execute"] = (p) => {
                window.__execs.push(p);
                const ids = programBlockSpans(md).map((b) => b.id);
                const now = Date.now();
                return { program: { session_id: "s-double-run", markdown: md, version: 2, template_id: null }, blocks: [], active_run: { run_id: "r-double", started_at_ms: now, expires_at_ms: now + 60000, pending_block_ids: ids, pending_block_tooltips: {}, seen_running: false, first_output_seen: false, agent_managed: false } };
              };
              setSession("s-double-run", "shell");
              await switchCurrentViewMode("program");
              programTestClearSel();
              await programRun();
              await programRun();
              return { execs: window.__execs, msg: programMsgEl.textContent };
            })
            "###,
        )
        .await
        .expect("evaluate double run")
        .into_value()
        .expect("json");
    assert_eq!(
        double_run["execs"].as_array().map(|a| a.len()),
        Some(1),
        "a double programRun() must send exactly one program.execute: {double_run:?}"
    );
    assert!(
        double_run["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("already dispatched"),
        "{double_run:?}"
    );

    // --- 7. Run with a selection scopes execute to the selected text. --------
    let run_sel: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const md = "# Heading\n- alpha\n- beta\n- gamma\n";
              window.__execs = [];
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-runsel", markdown: md, version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.execute"] = (p) => { window.__execs.push(p); const now = Date.now(); return { program: { session_id: "s-runsel", markdown: md, version: 1, template_id: null }, blocks: [], active_run: { run_id: "r2", started_at_ms: now, expires_at_ms: now + 60000, pending_block_ids: [], pending_block_tooltips: {}, seen_running: false, first_output_seen: false, agent_managed: false } }; };
              setSession("s-runsel", "shell");
              await switchCurrentViewMode("program");
              programTestSelectLines(1, 2); // "- alpha" + "- beta"
              await programRun();
              return { execs: window.__execs, msg: programMsgEl.textContent };
            })
            "###,
        )
        .await
        .expect("evaluate run selection")
        .into_value()
        .expect("json");
    assert_eq!(
        run_sel["execs"][0]["selection"], "- alpha\n- beta",
        "{run_sel:?}"
    );
    assert!(
        run_sel["msg"]
            .as_str()
            .unwrap_or_default()
            .contains("selection"),
        "{run_sel:?}"
    );

    // --- 7a. Drag-style selection exposes the inline Run affordance. --------
    let run_menu: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const md = "# Heading\n- alpha\n- beta\n- gamma\n";
              window.__execs = [];
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-runmenu", markdown: md, version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.execute"] = (p) => { window.__execs.push(p); const now = Date.now(); return { program: { session_id: "s-runmenu", markdown: md, version: 1, template_id: null }, blocks: [], active_run: { run_id: "r-menu", started_at_ms: now, expires_at_ms: now + 60000, pending_block_ids: [], pending_block_tooltips: {}, seen_running: false, first_output_seen: false, agent_managed: false } }; };
              setSession("s-runmenu", "shell");
              await switchCurrentViewMode("program");
              programInputEl.focus();
              programTestSelectLines(1, 2); // "- alpha" + "- beta"
              const line = programInputEl.querySelectorAll(":scope > div")[2];
              const rect = line.getBoundingClientRect();
              programInputEl.dispatchEvent(new PointerEvent("pointerup", {
                bubbles: true,
                pointerType: "mouse",
                clientX: rect.right,
                clientY: rect.bottom,
              }));
              await new Promise((resolve) => requestAnimationFrame(resolve));
              const shown = !programSelectionMenuEl.hidden;
              const label = programSelectionRunBtn.textContent.trim();
              programSelectionRunBtn.dispatchEvent(new MouseEvent("mousedown", { bubbles: true, cancelable: true }));
              for (let i = 0; i < 20 && window.__execs.length === 0; i++) {
                await new Promise((resolve) => requestAnimationFrame(resolve));
              }
              const sel = window.getSelection();
              return {
                shown,
                label,
                execs: window.__execs,
                hiddenAfter: programSelectionMenuEl.hidden,
                selectionCollapsed: !sel || sel.rangeCount === 0 || sel.isCollapsed,
              };
            })
            "###,
        )
        .await
        .expect("evaluate selection run menu")
        .into_value()
        .expect("json");
    assert_eq!(run_menu["shown"], true, "{run_menu:?}");
    assert!(
        run_menu["label"]
            .as_str()
            .unwrap_or_default()
            .contains("Run"),
        "{run_menu:?}"
    );
    assert_eq!(
        run_menu["execs"][0]["selection"], "- alpha\n- beta",
        "{run_menu:?}"
    );
    assert_eq!(run_menu["hiddenAfter"], true, "{run_menu:?}");
    assert_eq!(run_menu["selectionCollapsed"], true, "{run_menu:?}");

    // --- 7b. A partial-line selection (a strict SUBSTRING of a single line,
    // not the whole line) sends the real enclosing block's id as
    // `selection_block_ids`, not a phantom hash of the substring alone (the
    // bug this fix addresses: such a phantom matches nothing in the document
    // and the block never shimmers). The mock `program.execute` handler below
    // stands in for the fixed daemon by echoing that id back as pending.
    let partial: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const md = "Some long text here\n";
              window.__execs = [];
              window.__mockProgramHandlers["program.get"] = () => ({ program: { session_id: "s-partial", markdown: md, version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] });
              window.__mockProgramHandlers["program.execute"] = (p) => {
                window.__execs.push(p);
                const now = Date.now();
                const realId = (p.selection_block_ids && p.selection_block_ids[0]) || "phantom-no-ids-sent";
                return {
                  program: { session_id: "s-partial", markdown: md, version: 1, template_id: null },
                  blocks: [],
                  active_run: { run_id: "r-partial", started_at_ms: now, expires_at_ms: now + 60000, pending_block_ids: [], pending_block_refs: [realId], pending_block_tooltips: {}, seen_running: false, first_output_seen: false, agent_managed: false },
                };
              };
              setSession("s-partial", "shell");
              await switchCurrentViewMode("program");
              // Select "long text" out of "Some long text here" — a strict
              // substring of the single line/block, not the whole line.
              programTestSelectRange(0, 5, 14);
              await programRun();
              const line = programInputEl.querySelectorAll(":scope > div")[0];
              return {
                execs: window.__execs,
                realBlockId: programBlockSpans(md)[0].id,
                shimmering: line.classList.contains("is-running"),
              };
            })
            "###,
        )
        .await
        .expect("evaluate partial-line selection run")
        .into_value()
        .expect("json");
    assert_eq!(partial["execs"][0]["selection"], "long text", "{partial:?}");
    assert_eq!(
        partial["execs"][0]["selection_block_ids"][0], partial["realBlockId"],
        "the real enclosing block's id, not a phantom hash of the substring: {partial:?}"
    );
    assert_eq!(
        partial["shimmering"], true,
        "the block should shimmer once the (simulated fixed) daemon echoes the real id back: {partial:?}"
    );

    // --- 8. Smart-clip (@) autocomplete inserts a session clip. --------------
    let clip: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-clip", markdown: "ping ", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              state.sessions = [{ id: "sAAA111", title: "Builder", harness: "claude", state: "running", kind: "user" }, { id: "s-clip", harness: "shell", kind: "user" }];
              state.harnesses = [{ name: "codex", available: true }, { name: "claude", available: true }];
              state.currentId = "s-clip";
              await switchCurrentViewMode("program");
              programTestSet("ping @");
              programTestCaretEnd();
              programUpdateClipMenu();
              const menuOpen = !programClipMenuEl.hidden;
              const itemCount = programClipMenuEl.querySelectorAll("[data-sel]").length;
              // Arrow-down selects the 2nd item; the keyup must NOT reset it to 0.
              const sel0 = state.program.clip.selected;
              programInputEl.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true }));
              const selAfterDown = state.program.clip.selected;
              programInputEl.dispatchEvent(new KeyboardEvent("keyup", { key: "ArrowDown", bubbles: true }));
              const selAfterKeyup = state.program.clip.selected;
              programAcceptClip(0);
              return {
                menuOpen, itemCount, value: programSerialize(), menuClosedAfter: programClipMenuEl.hidden,
                sel0, selAfterDown, selAfterKeyup,
                // The chip is an atomic widget: its visible text is the friendly
                // label; the raw @{…} lives only in data-raw, never as text.
                chipHasLabel: programInputEl.textContent.includes("Builder"),
                chipShowsRaw: programInputEl.textContent.includes("@{session:sAAA111"),
                chipIsAtomic: !!programInputEl.querySelector('.program-clip[contenteditable="false"]'),
              };
            })
            "###,
        )
        .await
        .expect("evaluate clip")
        .into_value()
        .expect("json");
    assert_eq!(clip["menuOpen"], true, "{clip:?}");
    assert!(
        clip["itemCount"].as_u64().unwrap_or(0) >= 2,
        "session + harness candidates: {clip:?}"
    );
    assert_eq!(clip["value"], "ping @{session:sAAA111}", "{clip:?}");
    assert_eq!(clip["menuClosedAfter"], true, "{clip:?}");
    // #2: arrow-down navigation persists (keyup must not snap back to item 0).
    assert_eq!(clip["sel0"], 0, "{clip:?}");
    assert_eq!(clip["selAfterDown"], 1, "{clip:?}");
    assert_eq!(
        clip["selAfterKeyup"], 1,
        "arrow-down selection must persist through keyup: {clip:?}"
    );
    // #1: the clip renders a friendly, content-fit label, never the raw @{…}; it
    // is an atomic contenteditable=false widget (one cursor stop, deletes whole).
    assert_eq!(
        clip["chipHasLabel"], true,
        "clip should render its friendly label: {clip:?}"
    );
    assert_eq!(
        clip["chipShowsRaw"], false,
        "clip text must not be the raw @{{…}} syntax: {clip:?}"
    );
    assert_eq!(
        clip["chipIsAtomic"], true,
        "clip must be an atomic contenteditable=false widget: {clip:?}"
    );

    // --- 9. Find highlights matches and reports a count. ---------------------
    let find: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-find", markdown: "todo one\ntodo two\ndone three\ntodo four\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              setSession("s-find", "shell");
              await switchCurrentViewMode("program");
              programOpenFind();
              programFindInputEl.value = "todo";
              programRecomputeFind();
              const count = programFindCountEl.textContent;
              programFindMove(1);
              return { findVisible: !programFindEl.hidden, count, countAfterNext: programFindCountEl.textContent, matchCount: state.program.find.matches.length };
            })
            "###,
        )
        .await
        .expect("evaluate find")
        .into_value()
        .expect("json");
    assert_eq!(find["findVisible"], true, "{find:?}");
    assert_eq!(find["matchCount"], 3, "{find:?}");
    assert_eq!(find["count"], "1/3", "{find:?}");
    assert_eq!(find["countAfterNext"], "2/3", "{find:?}");

    // --- 9b. Ctrl+F is Emacs cursor-forward, not the browser/app Find. -------
    let ctrl_f: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-ctrlf", markdown: "abcdef\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              setSession("s-ctrlf", "shell");
              await switchCurrentViewMode("program");
              const before = programRangeForOffset(2);
              const sel = window.getSelection();
              sel.removeAllRanges();
              sel.addRange(before);
              const ev = new KeyboardEvent("keydown", { key: "f", ctrlKey: true, bubbles: true, cancelable: true });
              const defaultPrevented = !programInputEl.dispatchEvent(ev);
              const offsets = programSelectionOffsets();
              return {
                defaultPrevented,
                head: offsets ? offsets.head : null,
                findVisible: !programFindEl.hidden,
              };
            })
            "###,
        )
        .await
        .expect("evaluate ctrl+f cursor forward")
        .into_value()
        .expect("json");
    assert_eq!(
        ctrl_f["defaultPrevented"], true,
        "Ctrl+F must be prevented so the browser never opens its Find bar: {ctrl_f:?}"
    );
    assert_eq!(
        ctrl_f["head"], 3,
        "Ctrl+F should move the caret forward one character: {ctrl_f:?}"
    );
    assert_eq!(
        ctrl_f["findVisible"], false,
        "Ctrl+F must not open the Program Find bar either: {ctrl_f:?}"
    );

    // --- 10. Live program/state: adopt when clean, keep when dirty. ----------
    let live: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-live", markdown: "v1 body\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              setSession("s-live", "shell");
              await switchCurrentViewMode("program");
              handleProgramState({ program: { session_id: "s-live", markdown: "agent edit v2\n", version: 2, template_id: null }, active_run: null });
              const adopted = programSerialize();
              const adoptedVersion = programVersionEl.textContent;
              programTestSet("agent edit v2\nmy unsaved line\n");
              handleProgramState({ program: { session_id: "s-live", markdown: "agent edit v3 different\n", version: 3, template_id: null }, active_run: null });
              return { adopted, adoptedVersion, keptDirty: programSerialize(), dirty: programSaveBtn.getAttribute("data-dirty") };
            })
            "###,
        )
        .await
        .expect("evaluate live")
        .into_value()
        .expect("json");
    assert_eq!(
        live["adopted"], "agent edit v2\n",
        "clean buffer adopts agent edit: {live:?}"
    );
    assert_eq!(live["adoptedVersion"], "v2", "{live:?}");
    assert_eq!(
        live["keptDirty"], "agent edit v2\nmy unsaved line\n",
        "dirty buffer keeps local edits: {live:?}"
    );
    assert_eq!(live["dirty"], "true", "{live:?}");

    let run_state: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-run-state", markdown: "- pending\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              const md = "- pending\n";
              setSession("s-run-state", "shell");
              await switchCurrentViewMode("program");
              const id = programBlockSpans(md)[0].id;
              programStartOptimisticRun("s-run-state", md, false, null, "");
              handleProgramState({ program: { session_id: "s-run-state", markdown: md, version: 2, template_id: null }, active_run: null, blocks: [] });
              const kept = state.program.runById.get("s-run-state");
              const settleAfterKept = state.program.settleById.get("s-run-state")?.size || 0;
              const now = Date.now();
              handleProgramState({
                program: { session_id: "s-run-state", markdown: md, version: 2, template_id: null },
                active_run: { run_id: "r1", started_at_ms: now - 1000, expires_at_ms: now + 60000, pending_block_ids: [id], pending_block_tooltips: {}, seen_running: true, first_output_seen: false, agent_managed: false },
                blocks: [],
              });
              const confirmed = state.program.runById.get("s-run-state");
              handleProgramState({ program: { session_id: "s-run-state", markdown: md, version: 2, template_id: null }, active_run: null, blocks: [] });
              const survivedStaleClear = state.program.runById.has("s-run-state");
              const grace = typeof PROGRAM_RUN_ADOPT_CLEAR_GRACE_MS === "number" ? PROGRAM_RUN_ADOPT_CLEAR_GRACE_MS : 1500;
              const adopted = state.program.runById.get("s-run-state");
              if (adopted) adopted.daemonAdoptedPerf = performance.now() - grace - 1;
              handleProgramState({ program: { session_id: "s-run-state", markdown: md, version: 2, template_id: null }, active_run: null, blocks: [] });
              return {
                keptPending: kept ? kept.pendingIds.size : 0,
                settleAfterKept,
                confirmedPending: confirmed ? confirmed.pendingIds.size : 0,
                survivedStaleClear,
                cleared: !state.program.runById.has("s-run-state"),
              };
            })
            "###,
        )
        .await
        .expect("evaluate program run state")
        .into_value()
        .expect("json");
    assert_eq!(
        run_state["keptPending"], 1,
        "empty state must not clear an optimistic web run: {run_state:?}"
    );
    assert_eq!(
        run_state["settleAfterKept"], 0,
        "skipped optimistic clear must not record settle flourishes: {run_state:?}"
    );
    assert_eq!(
        run_state["confirmedPending"], 1,
        "daemon progress should be adopted before later clear: {run_state:?}"
    );
    assert_eq!(
        run_state["survivedStaleClear"], true,
        "stale empty state inside the adopt grace window must not clear daemon-confirmed web runs: {run_state:?}"
    );
    assert_eq!(
        run_state["cleared"], true,
        "empty state must still clear daemon-confirmed web runs: {run_state:?}"
    );

    // --- 11. Live collaboration: local edit RPC + remote cursor overlay. ----
    let collab: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-collab", markdown: "abc\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [], collaborators: [] }),
              "program.list_templates": () => ({ templates: [] }),
              "program.edit": (p) => {
                window.__programEdits.push(p);
                return { program: { session_id: "s-collab", markdown: "abcX\n", version: 2, template_id: null }, active_run: null, blocks: [] };
              },
              "program.cursor": (p) => {
                window.__programCursors.push(p);
                return { cursor: { session_id: p.session_id, client_id: "web-self", label: "Web", kind: "web", cursor: p.cursor, color_index: 1, updated_at_ms: Date.now(), active: !p.clear } };
              },
            }, async () => {
              window.__programEdits = [];
              window.__programCursors = [];
              setSession("s-collab", "shell");
              await switchCurrentViewMode("program");
              programTestCaretEnd();
              document.execCommand("insertText", false, "X");
              await new Promise((r) => setTimeout(r, 120));
              handleProgramCursor({ cursor: { session_id: "s-collab", client_id: "peer-1", label: "TUI", kind: "tui", cursor: 1, color_index: 2, updated_at_ms: Date.now(), active: true } });
              // A peer that stopped publishing over a minute ago must not
              // render, even though the daemon never sent an explicit
              // tombstone for it (e.g. the peer's connection is just idle).
              handleProgramCursor({ cursor: { session_id: "s-collab", client_id: "peer-2", label: "Stale", kind: "tui", cursor: 2, color_index: 3, updated_at_ms: Date.now() - 61000, active: true } });
              return {
                text: programSerialize(),
                editCount: window.__programEdits.length,
                firstEdit: window.__programEdits[0] || null,
                cursorCount: window.__programCursors.length,
                remoteCursorCount: programCursorLayerEl.querySelectorAll(".program-remote-cursor").length,
                remoteLabel: programCursorLayerEl.querySelector(".program-remote-cursor")?.dataset.label || "",
              };
            })
            "###,
        )
        .await
        .expect("evaluate collab")
        .into_value()
        .expect("json");
    assert_eq!(
        collab["text"], "abcX\n",
        "typed text should stay local immediately: {collab:?}"
    );
    assert_eq!(
        collab["editCount"], 1,
        "one live program.edit should be sent: {collab:?}"
    );
    assert_eq!(collab["firstEdit"]["session_id"], "s-collab", "{collab:?}");
    assert_eq!(
        collab["cursorCount"].as_i64().unwrap_or_default() >= 1,
        true,
        "cursor presence should publish: {collab:?}"
    );
    assert_eq!(
        collab["remoteCursorCount"], 1,
        "remote cursor overlay should render only the live peer, not the stale one: {collab:?}"
    );
    assert_eq!(collab["remoteLabel"], "TUI", "{collab:?}");

    // --- 12. Live collaboration: own rebased cursor notification moves caret.
    let own_cursor: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-own-cursor", markdown: "123456789\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [], collaborators: [] }),
              "program.list_templates": () => ({ templates: [] }),
              "program.cursor": (p) => {
                return { cursor: { session_id: p.session_id, client_id: "web-self", label: "Web", kind: "web", cursor: p.cursor, color_index: 1, updated_at_ms: Date.now(), active: !p.clear } };
              },
            }, async () => {
              setSession("s-own-cursor", "shell");
              await switchCurrentViewMode("program");
              state.program.ownClientId = "web-self";
              const before = programRangeForOffset(6);
              const sel = window.getSelection();
              sel.removeAllRanges();
              sel.addRange(before);
              handleProgramState({ program: { session_id: "s-own-cursor", markdown: "12356789\n", version: 2, template_id: null }, active_run: null, blocks: [] });
              handleProgramCursor({ cursor: { session_id: "s-own-cursor", client_id: "web-self", label: "Web 1", kind: "web", cursor: 5, color_index: 1, updated_at_ms: Date.now(), active: true } });
              const offsets = programSelectionOffsets();
              return {
                text: programSerialize(),
                head: offsets ? offsets.head : null,
                remoteCursorCount: programCursorLayerEl.querySelectorAll(".program-remote-cursor").length,
              };
            })
            "###,
        )
        .await
        .expect("evaluate own cursor")
        .into_value()
        .expect("json");
    assert_eq!(own_cursor["text"], "12356789\n", "{own_cursor:?}");
    assert_eq!(
        own_cursor["head"], 5,
        "own caret should adopt daemon-rebased cursor offset: {own_cursor:?}"
    );
    assert_eq!(
        own_cursor["remoteCursorCount"], 0,
        "own cursor should not render as a remote overlay: {own_cursor:?}"
    );

    // --- 12b. Live program/state adopt rebases the caret through the content
    //          diff instead of merely clamping it (spec 0065). An insertion
    //          lands before the caret, with no own-cursor echo to correct it
    //          afterward — the adopt path itself must shift the caret.
    let adopt_caret: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-adopt-caret", markdown: "alpha beta\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              setSession("s-adopt-caret", "shell");
              await switchCurrentViewMode("program");
              const before = programRangeForOffset(10);
              const sel = window.getSelection();
              sel.removeAllRanges();
              sel.addRange(before);
              handleProgramState({ program: { session_id: "s-adopt-caret", markdown: "alpha INSERTED beta\n", version: 2, template_id: null }, active_run: null, blocks: [] });
              const offsets = programSelectionOffsets();
              return { text: programSerialize(), head: offsets ? offsets.head : null };
            })
            "###,
        )
        .await
        .expect("evaluate adopt caret")
        .into_value()
        .expect("json");
    assert_eq!(
        adopt_caret["text"], "alpha INSERTED beta\n",
        "{adopt_caret:?}"
    );
    assert_eq!(
        adopt_caret["head"], 19,
        "caret should shift by the 9-char insertion, not clamp in place: {adopt_caret:?}"
    );

    // --- 13. Session-status chip badges: initial missing state, live update
    //         on session/state, and flip to missing on session/deleted. Also
    //         covers the running-activity pulse (`.is-active`), which mirrors
    //         the session list's own gate (state "running" + recent PTY
    //         bytes) rather than repainting per spinner frame. ---------------
    let badges: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-badges", markdown: "@{session:sBadge1} @{session:sGone}\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [] }),
              "program.list_templates": () => ({ templates: [] }),
            }, async () => {
              state.sessions = [
                { id: "s-badges", harness: "shell", kind: "user" },
                { id: "sBadge1", title: "Worker", harness: "claude", state: "running", last_pty_at_ms: Date.now(), kind: "user" },
              ];
              state.currentId = "s-badges";
              await switchCurrentViewMode("program");
              const chipInfo = () => Array.from(programInputEl.querySelectorAll(".program-clip[data-raw]"))
                .map((c) => ({ status: c.dataset.status, title: c.title, active: c.classList.contains("is-active") }));
              const before = chipInfo();
              handleNotification("session/state", { session: { id: "sBadge1", title: "Worker", harness: "claude", state: "errored", kind: "user" } });
              const afterError = chipInfo();
              handleNotification("session/deleted", { session_id: "sBadge1" });
              const afterDelete = chipInfo();
              return { before, afterError, afterDelete };
            })
            "###,
        )
        .await
        .expect("evaluate badges")
        .into_value()
        .expect("json");
    assert_eq!(
        badges["before"][0]["status"], "running",
        "a resolved session clip should badge its live status: {badges:?}"
    );
    assert_eq!(
        badges["before"][0]["active"], true,
        "a running session with recent PTY bytes should pulse its chip, mirroring \
         the session list's own activity indicator: {badges:?}"
    );
    assert_eq!(
        badges["before"][1]["status"], "missing",
        "a clip whose target isn't in state.sessions should badge missing: {badges:?}"
    );
    assert_eq!(
        badges["before"][1]["title"], "session deleted",
        "{badges:?}"
    );
    assert_eq!(
        badges["afterError"][0]["status"], "errored",
        "a live session/state push should repaint the mounted chip in place: {badges:?}"
    );
    assert_eq!(
        badges["afterError"][0]["title"], "exited with error",
        "{badges:?}"
    );
    assert_eq!(
        badges["afterError"][0]["active"], false,
        "an errored session should stop pulsing its chip: {badges:?}"
    );
    assert_eq!(
        badges["afterDelete"][0]["status"], "missing",
        "session/deleted should flip the now-dead chip to missing: {badges:?}"
    );
    assert_eq!(
        badges["afterDelete"][0]["title"], "session deleted",
        "{badges:?}"
    );

    // --- 14. Live collaboration: agent presence cursor + reveal highlight
    //     (spec 0065 agent presence). An agent-authored edit publishes a
    //     `kind: "agent"` cursor carrying the edited span in
    //     `selection_anchor`/`selection_head`; the web client renders it
    //     styled distinctly and briefly tints that span.
    let agent_presence: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({ program: { session_id: "s-agent-presence", markdown: "123456789\n", version: 1, template_id: null }, active_run: null, blocks: [], revisions: [], collaborators: [] }),
              "program.list_templates": () => ({ templates: [] }),
              "program.cursor": (p) => {
                return { cursor: { session_id: p.session_id, client_id: "web-self", label: "Web", kind: "web", cursor: p.cursor, color_index: 1, updated_at_ms: Date.now(), active: !p.clear } };
              },
            }, async () => {
              setSession("s-agent-presence", "shell");
              await switchCurrentViewMode("program");
              handleProgramCursor({ cursor: { session_id: "s-agent-presence", client_id: "agent-1", label: "claude", kind: "agent", cursor: 6, selection_anchor: 3, selection_head: 6, color_index: 2, updated_at_ms: Date.now(), active: true } });
              const agentEl = programCursorLayerEl.querySelector('.program-remote-cursor[data-kind="agent"]');
              return {
                agentCursorCount: programCursorLayerEl.querySelectorAll('.program-remote-cursor[data-kind="agent"]').length,
                agentLabel: agentEl ? agentEl.dataset.label : "",
                revealCount: programCursorLayerEl.querySelectorAll(".program-agent-reveal").length,
              };
            })
            "###,
        )
        .await
        .expect("evaluate agent presence")
        .into_value()
        .expect("json");
    assert_eq!(
        agent_presence["agentCursorCount"], 1,
        "agent-authored cursor should render as a distinctly-kinded remote cursor: {agent_presence:?}"
    );
    assert_eq!(agent_presence["agentLabel"], "claude", "{agent_presence:?}");
    assert!(
        agent_presence["revealCount"].as_i64().unwrap_or_default() >= 1,
        "the edited span should get a brief reveal highlight: {agent_presence:?}"
    );

    // --- 14b. Same reveal, but for a cursor arriving in the `program.get`
    //     mount snapshot rather than a live push. There is no local receipt
    //     for a cursor this client never watched arrive, so this path must
    //     keep gating on the daemon's own `updated_at_ms`: a cursor from
    //     seconds ago (within the daemon's one-minute presence TTL, so it's
    //     still in the snapshot) must NOT flash a reveal on open, while one
    //     from just now must.
    let agent_presence_snapshot: serde_json::Value = page
        .evaluate(
            r###"
            withMockProgram({
              "program.get": () => ({
                program: { session_id: "s-agent-presence-snapshot", markdown: "123456789\n", version: 1, template_id: null },
                active_run: null, blocks: [], revisions: [],
                collaborators: [
                  { session_id: "s-agent-presence-snapshot", client_id: "agent-fresh", label: "claude", kind: "agent", cursor: 6, selection_anchor: 3, selection_head: 6, color_index: 2, updated_at_ms: Date.now(), active: true },
                  { session_id: "s-agent-presence-snapshot", client_id: "agent-stale", label: "claude", kind: "agent", cursor: 6, selection_anchor: 3, selection_head: 6, color_index: 3, updated_at_ms: Date.now() - 30000, active: true },
                ],
              }),
              "program.list_templates": () => ({ templates: [] }),
              "program.cursor": (p) => {
                return { cursor: { session_id: p.session_id, client_id: "web-self", label: "Web", kind: "web", cursor: p.cursor, color_index: 1, updated_at_ms: Date.now(), active: !p.clear } };
              },
            }, async () => {
              setSession("s-agent-presence-snapshot", "shell");
              await switchCurrentViewMode("program");
              return {
                revealCount: programCursorLayerEl.querySelectorAll(".program-agent-reveal").length,
              };
            })
            "###,
        )
        .await
        .expect("evaluate agent presence snapshot")
        .into_value()
        .expect("json");
    assert_eq!(
        agent_presence_snapshot["revealCount"], 1,
        "only the fresh snapshot cursor should flash a reveal on mount, not the 30s-old one: \
         {agent_presence_snapshot:?}"
    );

    // --- Visual artifacts: drive the REAL session's program (it has a smart
    //     clip from step 1) and leave it mounted so the screenshots show the
    //     genuine rendered surface, chip, and run shimmer. ------------------
    page.evaluate(
        r###"
        (async () => {
          const list = await rpc("session.list");
          const sid = (list.find((s) => s.harness === "shell") || list[0]).id;
          state.sessions = list;
          state.currentId = sid;
          await switchCurrentViewMode("program");
          return true;
        })()
        "###,
    )
    .await
    .ok();
    screenshot(&page, "program_view_rendered.png").await;
    page.evaluate(
        r###"
        (() => {
          const sid = state.program.mountedId;
          programStartOptimisticRun(sid, programSerialize(), false, null);
          programApplyShimmer();
          return true;
        })()
        "###,
    )
    .await
    .ok();
    screenshot(&page, "program_view_shimmer.png").await;
    page.evaluate(
        r###"
        (() => {
          programStopShimmer();
          state.program.runById.clear();
          programTestSet(programSerialize() + "\nrun @");
          programTestCaretEnd();
          programInputEl.focus();
          programUpdateClipMenu();
          return true;
        })()
        "###,
    )
    .await
    .ok();
    screenshot(&page, "program_view_clip_menu.png").await;

    page.evaluate("enterChatMode(); true").await.ok();
}

/// Instant-dispatch fast path (spec 0066): a selection-Run over a single list
/// item naming exactly one `@{harness:<name>}` clip is executed by the daemon
/// mechanically — no browser needed, this drives the real daemon IPC surface
/// directly like the other e2e helpers in this crate.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_instant_dispatch_fast_path() {
    let d = Daemon::spawn().await.expect("daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();

    let owner = d
        .client
        .create(shell_session_params(&cwd, "owner"))
        .await
        .expect("create owner session");

    let md = "# Todo\n\n- Print hello @{harness:shell}\n";
    let updated = d
        .client
        .program_update(agentd_protocol::ProgramUpdateParams {
            session_id: owner.clone(),
            markdown: md.to_string(),
            base_version: None,
            actor: agentd_protocol::ProgramUpdateActor::Human,
            template_id: None,
            note: None,
            shimmer: None,
            shimmer_tooltips: None,
        })
        .await
        .expect("program.update");

    let item_text = "- Print hello @{harness:shell}";
    let result = d
        .client
        .program_execute(agentd_protocol::ProgramExecuteParams {
            session_id: owner.clone(),
            selection: Some(item_text.to_string()),
            base_version: Some(updated.program.version),
            shimmer: None,
            selection_block_ids: None,
            comment: None,
        })
        .await
        .expect("program.execute");

    // No LLM round trip: the fast path never delivers a prompt to the owner.
    assert_eq!(result.prompt, "", "fast path must not deliver a prompt");

    // Exactly one subagent was spawned, parented to the owner and backed by
    // the named harness.
    let sessions = d.client.list().await.expect("list");
    let subagents: Vec<_> = sessions
        .iter()
        .filter(|s| {
            s.kind == agentd_protocol::SessionKind::Subagent
                && s.parent_session_id.as_deref() == Some(owner.as_str())
        })
        .collect();
    assert_eq!(
        subagents.len(),
        1,
        "expected exactly one dispatched subagent: {sessions:?}"
    );
    let subagent = subagents[0];
    assert_eq!(subagent.harness, "shell");

    // The program was annotated with the new subagent's session clip,
    // alongside (not replacing) the original harness clip.
    let expected_clip = format!("@{{session:{}}}", subagent.id);
    assert!(
        result.program.markdown.contains(&expected_clip),
        "program should carry the new subagent's session clip: {}",
        result.program.markdown
    );
    assert!(
        result.program.markdown.contains("@{harness:shell}"),
        "the original harness clip should stay: {}",
        result.program.markdown
    );

    // The dispatched item shimmers with the "Dispatched" tooltip, and the
    // active_run projection reflects the started run for shimmer rendering.
    let dispatched_block = result
        .blocks
        .iter()
        .find(|b| b.text.contains("Print hello"))
        .expect("dispatched block present in projection");
    assert!(dispatched_block.shimmer, "dispatched item should shimmer");
    assert_eq!(
        dispatched_block.tooltip.as_deref(),
        Some("Dispatched"),
        "dispatched item should carry the 'Dispatched' tooltip"
    );
    let active_run = result.active_run.expect("active run started");
    assert!(
        active_run.agent_managed,
        "a fast-pathed run is actively managed via its shimmer declaration"
    );

    // Re-reading the program from a clean call agrees with the execute
    // response — the run state is daemon-owned shared state, not a
    // client-local optimistic artifact.
    let refetched = d.client.program_get(&owner).await.expect("program.get");
    assert!(refetched
        .active_run
        .is_some_and(|run| run.agent_managed && !run.pending_block_refs.is_empty()));
}

/// A selection where only *some* items name a harness clip falls through to
/// the normal (LLM-mediated) execute path in its entirety — no subagent is
/// created for the matching item either (spec 0066: mixed selections are
/// all-or-nothing, never partially fast-pathed).
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_instant_dispatch_mixed_selection_falls_through() {
    let d = Daemon::spawn().await.expect("daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();

    let owner = d
        .client
        .create(shell_session_params(&cwd, "owner"))
        .await
        .expect("create owner session");

    let md = "- Fix the bug @{harness:shell}\n- Investigate the timeout\n";
    d.client
        .program_update(agentd_protocol::ProgramUpdateParams {
            session_id: owner.clone(),
            markdown: md.to_string(),
            base_version: None,
            actor: agentd_protocol::ProgramUpdateActor::Human,
            template_id: None,
            note: None,
            shimmer: None,
            shimmer_tooltips: None,
        })
        .await
        .expect("program.update");

    let selection = "- Fix the bug @{harness:shell}\n- Investigate the timeout";
    let result = d
        .client
        .program_execute(agentd_protocol::ProgramExecuteParams {
            session_id: owner.clone(),
            selection: Some(selection.to_string()),
            base_version: None,
            shimmer: None,
            selection_block_ids: None,
            comment: None,
        })
        .await
        .expect("program.execute");

    // Normal path: a real prompt is delivered, and the document is untouched
    // by the execute call itself (the agent would edit it in its own turn).
    assert!(!result.prompt.is_empty(), "normal path delivers a prompt");
    assert_eq!(result.program.markdown, md);

    // No subagent was spawned even though one item named a harness clip.
    let sessions = d.client.list().await.expect("list");
    let subagents = sessions
        .iter()
        .filter(|s| s.kind == agentd_protocol::SessionKind::Subagent)
        .count();
    assert_eq!(
        subagents, 0,
        "a mixed selection must not partially fast-path: {sessions:?}"
    );
}

/// A nested (indented) list item's leading whitespace must survive into the
/// anchored edit the fast path applies (spec 0066) — using the whole
/// selection's *trimmed* text as the edit anchor would strip that
/// indentation from the first line and the anchor would never match the
/// stored document.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn program_instant_dispatch_preserves_nested_indentation() {
    let d = Daemon::spawn().await.expect("daemon");
    let cwd = d.dir.path().to_string_lossy().to_string();

    let owner = d
        .client
        .create(shell_session_params(&cwd, "owner"))
        .await
        .expect("create owner session");

    let md = "- Parent\n  - Fix nested bug @{harness:shell}\n";
    d.client
        .program_update(agentd_protocol::ProgramUpdateParams {
            session_id: owner.clone(),
            markdown: md.to_string(),
            base_version: None,
            actor: agentd_protocol::ProgramUpdateActor::Human,
            template_id: None,
            note: None,
            shimmer: None,
            shimmer_tooltips: None,
        })
        .await
        .expect("program.update");

    // Selected exactly as it appears in the document, indentation included.
    let selection = "  - Fix nested bug @{harness:shell}";
    let result = d
        .client
        .program_execute(agentd_protocol::ProgramExecuteParams {
            session_id: owner.clone(),
            selection: Some(selection.to_string()),
            base_version: None,
            shimmer: None,
            selection_block_ids: None,
            comment: None,
        })
        .await
        .expect("program.execute");

    assert_eq!(result.prompt, "", "fast path must not deliver a prompt");
    assert!(
        result
            .program
            .markdown
            .contains("  - Fix nested bug @{harness:shell} @{session:"),
        "the nested item's original indentation must be preserved: {}",
        result.program.markdown
    );
}

fn shell_session_params(cwd: &str, title: &str) -> agentd_protocol::CreateSessionParams {
    agentd_protocol::CreateSessionParams {
        harness: "shell".to_string(),
        cwd: cwd.to_string(),
        prompt: None,
        model: None,
        title: Some(title.to_string()),
        mode: None,
        pty_size: None,
        worktree: false,
        env: std::collections::HashMap::new(),
        args: Vec::new(),
        kind: Default::default(),
        parent_session_id: None,
        group_id: None,
        position_after_session_id: None,
        forked_from: None,
    }
}

async fn wait_conn_open(page: &Page) {
    let deadline = Instant::now() + Duration::from_secs(15);
    loop {
        let state: String = page
            .evaluate("document.getElementById('conn')?.dataset?.state || ''")
            .await
            .ok()
            .and_then(|r| r.into_value::<String>().ok())
            .unwrap_or_default();
        if state == "open" {
            return;
        }
        if Instant::now() > deadline {
            panic!("web client never reached conn state='open'");
        }
        tokio::time::sleep(Duration::from_millis(150)).await;
    }
}

async fn screenshot(page: &Page, name: &str) {
    let Ok(dir) = artifact_dir() else { return };
    let path = dir.join(name);
    if page
        .save_screenshot(ScreenshotParams::builder().full_page(true).build(), &path)
        .await
        .is_ok()
    {
        eprintln!("program_view screenshot: {}", path.display());
    }
}

fn inject_userinfo(url: &str, user: &str, pw: &str) -> String {
    if let Some(rest) = url.strip_prefix("http://") {
        format!("http://{user}:{pw}@{rest}")
    } else if let Some(rest) = url.strip_prefix("https://") {
        format!("https://{user}:{pw}@{rest}")
    } else {
        url.to_string()
    }
}

/// Installs `window.setSession(id, harness)` and `window.withMockProgram(handlers, fn)`:
/// the latter swaps in a fake `state.ws` answering the given program.* RPCs from
/// `window.__mockProgramHandlers` (tests may mutate it mid-run), runs `fn`, then
/// restores every touched global.
const SETUP_JS: &str = r###"
    window.setSession = function (id, harness) {
      state.sessions = [{ id, title: id, harness, state: "running", kind: "user", has_pty: false }];
      state.currentId = id;
    };
    // contenteditable test helpers (the editor is no longer a <textarea>).
    window.programTestSet = function (md) {
      state.program.applyingRemote = true;
      try { programRenderDoc(md); programOnInput(); }
      finally { state.program.applyingRemote = false; }
    };
    window.programTestClearSel = function () { const s = window.getSelection(); if (s) s.removeAllRanges(); };
    window.programTestCaretEnd = function () {
      const walker = document.createTreeWalker(programInputEl, NodeFilter.SHOW_TEXT);
      let last = null, n; while ((n = walker.nextNode())) last = n;
      const sel = window.getSelection(); sel.removeAllRanges();
      const r = document.createRange();
      if (last) r.setStart(last, last.data.length); else r.setStart(programInputEl, 0);
      r.collapse(true); sel.addRange(r);
    };
    window.programTestSelectLines = function (a, b) {
      const lines = programInputEl.querySelectorAll(":scope > div");
      const sel = window.getSelection(); sel.removeAllRanges();
      const r = document.createRange();
      r.setStart(lines[a], 0);
      r.setEnd(lines[b], lines[b].childNodes.length);
      sel.addRange(r);
    };
    // Selects characters [startCol, endCol) of a single line's first text
    // node — a strict SUBSTRING of the line, not the whole line, for testing
    // the partial-line selection Run fix.
    window.programTestSelectRange = function (lineIndex, startCol, endCol) {
      const line = programInputEl.querySelectorAll(":scope > div")[lineIndex];
      const textNode = line.firstChild;
      const sel = window.getSelection(); sel.removeAllRanges();
      const r = document.createRange();
      r.setStart(textNode, startCol);
      r.setEnd(textNode, endCol);
      sel.addRange(r);
    };
    window.withMockProgram = async function (handlers, fn) {
      const saved = {
        ws: state.ws, sessions: state.sessions, currentId: state.currentId,
        mode: state.mode, harnesses: state.harnesses,
        docById: state.program.docById, runById: state.program.runById,
        runButtonById: state.program.runButtonById,
        templates: state.program.templates, mountedId: state.program.mountedId,
      };
      state.program.docById = new Map();
      state.program.runById = new Map();
      state.program.runButtonById = new Map();
      state.program.templates = null;
      window.__mockProgramHandlers = handlers;
      state.ws = {
        readyState: 1,
        send(raw) {
          const msg = JSON.parse(raw);
          const pending = state.pending.get(msg.id);
          state.pending.delete(msg.id);
          const h = window.__mockProgramHandlers[msg.method];
          queueMicrotask(() => {
            if (!h) { pending.resolve({}); return; }
            try { pending.resolve(h(msg.params)); } catch (e) { pending.reject(e); }
          });
        },
      };
      try { return await fn(); }
      finally {
        try { leaveProgramView(); } catch (e) {}
        state.ws = saved.ws; state.sessions = saved.sessions; state.currentId = saved.currentId;
        state.mode = saved.mode; state.harnesses = saved.harnesses;
        state.program.docById = saved.docById; state.program.runById = saved.runById;
        state.program.runButtonById = saved.runButtonById;
        state.program.templates = saved.templates; state.program.mountedId = saved.mountedId;
      }
    };
    true
"###;
