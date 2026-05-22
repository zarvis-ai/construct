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
