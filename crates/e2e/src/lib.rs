//! End-to-end test harness for the construct daemon.
//!
//! Spawns the **real** daemon (`construct daemon run`) out of the workspace's
//! `target/debug/` against a fresh tempdir for every test (so
//! the test never touches the developer's actual `$CONSTRUCT_*_DIR`
//! state), waits for the IPC socket to come up, and returns a
//! connected `construct_client::Client` plus the path to the
//! socket. Drop kills the daemon and cleans the tempdir.
//!
//! There's a sibling [`Tui`] helper that spawns the `construct` TUI
//! inside a pseudo-terminal so tests can scrape rendered output
//! (via `vt100`) and send keystrokes back. Together they let a
//! single test exercise the full stack: TUI → IPC → daemon →
//! remote WS listener.
//!
//! ## Prerequisite
//!
//! The harness assumes the daemon + cli binaries have already
//! been built — it does **not** invoke `cargo build` itself.
//! CI runs `cargo build --workspace --all-targets --locked`
//! before `cargo test`; for local runs, do the same once and
//! then `cargo test -p agentd-e2e` works.

use anyhow::{anyhow, Context, Result};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tempfile::TempDir;
use tokio::process::{Child, Command};

use construct_client::Client;

/// One isolated daemon instance + a connected IPC client.
/// `Drop` kills the daemon (via tokio's `kill_on_drop`) and
/// cleans up the tempdir (via `TempDir`).
pub struct Daemon {
    /// Holds the tempdir alive for the daemon's lifetime. Public
    /// so tests can read paths under it (e.g. `runtime/remote.json`
    /// to inspect what the daemon persisted).
    pub dir: TempDir,
    /// IPC socket path the daemon is bound to. The `Client` is
    /// already connected to this; the path is exposed mainly for
    /// passing to the TUI helper.
    pub socket: PathBuf,
    /// Path the daemon binary was launched from. For `spawn()`
    /// this is the workspace `target/debug/construct` (run as
    /// `construct daemon run`). For `spawn_relocatable()` it's a
    /// private copy under the tempdir that the test can swap to
    /// exercise the "exec picks up an upgraded binary" path.
    pub binary_path: PathBuf,
    /// Pre-connected IPC client. Tests start using it directly —
    /// no separate "connect" step.
    pub client: Arc<Client>,
    child: Child,
}

impl Daemon {
    /// Spawn a fresh daemon (`construct daemon run`) against a tempdir and wait
    /// for its IPC socket to come up. Always sets
    /// `CONSTRUCT_REMOTE_NO_TUNNEL=1` because the e2e tests should
    /// never spawn a real cloudflared subprocess (would publish a
    /// real tunnel URL and the CI runner can't reach a `*.try
    /// cloudflare.com` host anyway).
    ///
    /// Boot timeout is 15s — slow enough to absorb the
    /// orchestrator-spawn timeout on hosts where the adapter
    /// binaries can't be located.
    pub async fn spawn() -> Result<Self> {
        Self::spawn_inner(false).await
    }

    /// Like `spawn`, but copies the `construct` binary into the
    /// tempdir and launches that copy (as `construct daemon run`)
    /// instead of the workspace binary. The copy lives at
    /// `Daemon::binary_path`, so a test can atomically swap it
    /// (write-then-rename) and then
    /// `daemon.restart` to verify the daemon exec()s the new
    /// on-disk bytes. Running from a private copy also keeps the
    /// swap from disturbing other tests that share the workspace
    /// binary.
    pub async fn spawn_relocatable() -> Result<Self> {
        Self::spawn_inner(true).await
    }

    async fn spawn_inner(relocatable: bool) -> Result<Self> {
        // Root the tempdir under a SHORT base. macOS caps Unix
        // socket paths (`sun_path`) at 104 bytes, and the daemon's
        // per-adapter connect-back socket lives at
        // `<runtime>/adapters/s<32-hex>.sock`. The default macOS
        // temp dir (`/var/folders/<...long...>/T/`) blows past 104
        // once the tempdir suffix + `run/adapters/` + socket name
        // are appended, so the adapter's `bind()` fails silently
        // and `session.create` times out. `/tmp` keeps the whole
        // path well under the limit. (Linux's default `/tmp` is
        // already short, so this is a no-op there — which is why
        // CI never hit it.)
        let base: PathBuf = if cfg!(unix) {
            PathBuf::from("/tmp")
        } else {
            std::env::temp_dir()
        };
        std::fs::create_dir_all(&base).ok();
        let dir = tempfile::Builder::new()
            .prefix("ae")
            .tempdir_in(&base)
            .context("create tempdir")?;
        let runtime_dir = dir.path().join("run");
        let state_dir = dir.path().join("state");
        let data_dir = dir.path().join("data");
        let config_dir = dir.path().join("config");
        for d in [&runtime_dir, &state_dir, &data_dir, &config_dir] {
            std::fs::create_dir_all(d)?;
        }
        // Disable the orchestrator session in e2e. Without this,
        // CI runners (which have the smith adapter binary built
        // and discoverable) auto-spawn an "operator" session whose
        // panel grabs initial keyboard focus — keys then route
        // to the orchestrator's editor instead of the global
        // keymap, and chords like `Ctrl-x x` (palette) silently
        // type into the editor. Local dev environments where
        // the smith adapter isn't on PATH skip this naturally,
        // which is why the test passed locally but failed on CI.
        std::fs::write(
            config_dir.join("config.toml"),
            "[orchestrator]\nenabled = false\n",
        )
        .context("write e2e config.toml")?;
        let socket = runtime_dir.join("construct.sock");

        // For the relocatable case, copy the workspace binary into
        // the tempdir's `bin/`. Adapter binaries are resolved by
        // the daemon next to its own exe (`locate_sibling_binary`),
        // so copy those too — otherwise `session.create` in a
        // relocated daemon couldn't find them. The orchestrator is
        // disabled, but a test might still create a shell session.
        let binary_path = if relocatable {
            let bin_dir = dir.path().join("bin");
            std::fs::create_dir_all(&bin_dir)?;
            let src = construct_bin_path()?;
            let dst = bin_dir.join("construct");
            std::fs::copy(&src, &dst)
                .with_context(|| format!("copy {} -> {}", src.display(), dst.display()))?;
            copy_executable_perms(&dst)?;
            // No sibling adapter binaries needed — all adapters and the MCP
            // server are built into the single `construct` binary.
            dst
        } else {
            construct_bin_path()?
        };

        let mut cmd = Command::new(&binary_path);
        cmd.env("CONSTRUCT_RUNTIME_DIR", &runtime_dir)
            .env("CONSTRUCT_STATE_DIR", &state_dir)
            .env("CONSTRUCT_DATA_DIR", &data_dir)
            .env("CONSTRUCT_CONFIG_DIR", &config_dir)
            // Skip cloudflared in every e2e test — its absence
            // from CI runners is not a test failure.
            .env("CONSTRUCT_REMOTE_NO_TUNNEL", "1")
            .args(["daemon", "run", "--socket"])
            .arg(&socket)
            // Silence the daemon's stderr / stdout in tests by
            // default — flip these to `inherit()` while debugging.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", binary_path.display()))?;

        let deadline = Instant::now() + Duration::from_secs(15);
        while !socket.exists() {
            if Instant::now() > deadline {
                anyhow::bail!("daemon did not bind {} within 15s", socket.display());
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
        let client = Client::connect(&socket)
            .await
            .context("connect IPC client")?;
        // Sanity-ping so we know the daemon is actually serving,
        // not just that the socket file appeared.
        client
            .ping()
            .await
            .context("ping daemon after socket appeared")?;

        Ok(Daemon {
            dir,
            socket,
            binary_path,
            client,
            child,
        })
    }

    /// OS PID of the daemon process. Stable across `daemon.restart`
    /// because the daemon `exec()`s itself in place rather than
    /// forking — that invariant is exactly what the restart test
    /// asserts.
    pub fn pid(&self) -> Option<u32> {
        self.child.id()
    }

    /// After a `daemon.restart`, the old IPC connection is dead
    /// (the socket closed during `exec()`). Poll a fresh client +
    /// ping until the new daemon is serving again, then return the
    /// reconnected client. Errors on timeout.
    pub async fn wait_until_back(&self, timeout: Duration) -> Result<Arc<Client>> {
        let deadline = Instant::now() + timeout;
        loop {
            if let Ok(client) = Client::connect(&self.socket).await {
                if client.ping().await.is_ok() {
                    return Ok(client);
                }
            }
            if Instant::now() > deadline {
                anyhow::bail!("daemon did not come back within {timeout:?}");
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

/// A live `construct` TUI bound to a particular daemon socket, with
/// the underlying PTY parsed by `vt100` so tests can scrape the
/// rendered screen contents.
///
/// `Drop` force-kills the underlying child so a panicking
/// `wait_for` in a test doesn't leak the TUI process — without
/// this the tokio runtime can't tear down (the PTY reader task
/// stays parked) and `cargo test` looks like it hangs.
pub struct Tui {
    #[allow(dead_code)]
    pty: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    /// `Option` so `wait_exit` can take ownership of the child
    /// without preventing `Drop` from running on the rest of
    /// the struct.
    child: Option<Box<dyn portable_pty::Child + Send + Sync>>,
    parser: Arc<Mutex<vt100::Parser>>,
    /// Background blocking-task that drains the PTY into the
    /// `vt100::Parser`. Joined on Drop via the JoinHandle but
    /// we never await it explicitly — it exits when the PTY
    /// reader hits EOF.
    _reader_task: tokio::task::JoinHandle<()>,
}

impl Drop for Tui {
    fn drop(&mut self) {
        // Best-effort kill. `portable_pty::Child::kill` is
        // synchronous + idempotent — if the child has already
        // exited this is a no-op. Without this, a panicking
        // `wait_for` from a test leaves the construct process alive
        // and the tokio runtime hangs waiting on the PTY reader.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

impl Tui {
    /// Spawn `construct tui --socket <socket>` in a 30x100 PTY. The
    /// dimensions are arbitrary but match what the TUI tests
    /// expect for layout assertions.
    ///
    /// Old non-recording entrypoint; new tests should call
    /// `spawn_with_recording` so artifact uploads include a
    /// playable `.cast` of the test run.
    pub fn spawn(socket: &Path) -> Result<Self> {
        Self::spawn_inner(socket, None)
    }

    /// Like `spawn` but additionally writes an asciinema v2
    /// `.cast` recording of the entire PTY session to
    /// `artifact_dir()/<name>.cast`. CI converts these to GIFs
    /// via `agg` and uploads them so reviewers can replay the
    /// test interactively from the workflow run page.
    pub fn spawn_with_recording(socket: &Path, name: &str) -> Result<Self> {
        let path = artifact_dir()?.join(format!("{name}.cast"));
        Self::spawn_inner(socket, Some(path))
    }

    fn spawn_inner(socket: &Path, cast_path: Option<PathBuf>) -> Result<Self> {
        let client = construct_bin_path()?;
        let pty_system = portable_pty::native_pty_system();
        let size = portable_pty::PtySize {
            rows: 30,
            cols: 100,
            pixel_width: 0,
            pixel_height: 0,
        };
        let pair = pty_system
            .openpty(size)
            .map_err(|e| anyhow!("openpty: {e}"))?;

        let mut cmd = portable_pty::CommandBuilder::new(&client);
        cmd.args(["tui", "--socket"]);
        cmd.arg(socket);
        // The TUI looks at TERM for color handling. xterm-256color
        // is a safe default that vt100 understands.
        cmd.env("TERM", "xterm-256color");
        // CARGO_*-style env from the test runner can confuse the
        // child if it inspects them; not stripping them is fine
        // but explicitly setting RUST_BACKTRACE keeps test panic
        // output readable.
        cmd.env("RUST_BACKTRACE", "1");

        let child = pair
            .slave
            .spawn_command(cmd)
            .map_err(|e| anyhow!("spawn construct tui: {e}"))?;
        // Slave handle is no longer needed after spawn; dropping
        // it ensures EOF propagates to the child if the parent
        // closes.
        drop(pair.slave);

        let writer = pair
            .master
            .take_writer()
            .map_err(|e| anyhow!("take_writer: {e}"))?;
        let mut reader = pair
            .master
            .try_clone_reader()
            .map_err(|e| anyhow!("clone_reader: {e}"))?;

        let parser = Arc::new(Mutex::new(vt100::Parser::new(
            size.rows, size.cols,
            // Scrollback budget: 1000 lines is plenty for any
            // assertion the tests do today and small enough not
            // to hold huge memory.
            1000,
        )));
        // Open the asciinema cast file (if a path was supplied)
        // and write the v2 header. Each subsequent read from the
        // PTY appends a `[time, "o", "<bytes>"]` line. Writes are
        // best-effort: an IO error in the recording path
        // shouldn't fail the test, since the cast is a nice-to-
        // have artifact not a correctness check.
        let cast_writer = cast_path.and_then(|p| {
            if let Some(parent) = p.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let mut file = match std::fs::File::create(&p) {
                Ok(f) => f,
                Err(e) => {
                    eprintln!("warning: could not open cast file {}: {e}", p.display());
                    return None;
                }
            };
            let header = serde_json::json!({
                "version": 2,
                "width": size.cols,
                "height": size.rows,
                "timestamp": std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_secs())
                    .unwrap_or(0),
                "env": { "TERM": "xterm-256color" },
            });
            if writeln!(file, "{header}").is_err() {
                return None;
            }
            Some(file)
        });
        let parser_for_task = parser.clone();
        let reader_task = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            let start = std::time::Instant::now();
            let mut cast = cast_writer;
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut p = parser_for_task.lock().unwrap();
                        p.process(&buf[..n]);
                        drop(p);
                        if let Some(w) = cast.as_mut() {
                            // Lossy UTF-8 is acceptable here —
                            // the cast is for human review of
                            // the test run, not a faithful
                            // byte-for-byte log. Most PTY
                            // output is UTF-8 anyway.
                            let chunk = String::from_utf8_lossy(&buf[..n]);
                            let event =
                                serde_json::json!([start.elapsed().as_secs_f64(), "o", chunk,]);
                            let _ = writeln!(w, "{event}");
                        }
                    }
                }
            }
        });

        Ok(Tui {
            pty: pair.master,
            writer,
            child: Some(child),
            parser,
            _reader_task: reader_task,
        })
    }

    /// Snapshot of the visible screen contents as plain text.
    /// One newline per row, no trailing newline. Useful for
    /// `assert!(screen.contains(...))` style checks.
    pub fn screen(&self) -> String {
        self.parser.lock().unwrap().screen().contents()
    }

    /// Current cursor position as `(row, col)` from the parsed
    /// screen. Lets a benchmark measure cursor-movement latency
    /// (e.g. left-arrow across a line) deterministically rather
    /// than via screen-stability heuristics.
    pub fn cursor(&self) -> (u16, u16) {
        self.parser.lock().unwrap().screen().cursor_position()
    }

    /// Write raw bytes to the PTY. Strings are sent as-is — for
    /// keypresses, use the escape sequences a real terminal
    /// would send (e.g. `"\x1b"` for Esc, `"\r"` for Enter,
    /// `"\x03"` for Ctrl-C).
    pub fn send(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }

    /// Poll the parsed screen for the substring `needle`,
    /// returning as soon as it appears. On timeout, includes the
    /// full current screen in the error so the failure is
    /// inspectable from CI logs without rerunning the test.
    pub async fn wait_for(&self, needle: &str, timeout: Duration) -> Result<()> {
        let deadline = Instant::now() + timeout;
        loop {
            if self.screen().contains(needle) {
                return Ok(());
            }
            if Instant::now() > deadline {
                anyhow::bail!(
                    "timed out after {:?} waiting for {needle:?} in TUI screen.\n\
                     ----- last screen -----\n{}\n-----------------------",
                    timeout,
                    self.screen()
                );
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    /// Block until the TUI process exits, with a timeout. Returns
    /// the wait status. Useful at end-of-test to confirm a clean
    /// shutdown rather than a hang.
    pub async fn wait_exit(mut self, timeout: Duration) -> Result<portable_pty::ExitStatus> {
        let mut child = self
            .child
            .take()
            .ok_or_else(|| anyhow!("wait_exit called twice"))?;
        // `portable_pty::Child::wait` is blocking and takes `&mut self`,
        // so we ferry it across a thread + oneshot. Grab a killer handle
        // FIRST: on timeout we must kill the child, otherwise the wait
        // thread blocks forever holding the process alive, the PTY reader
        // never hits EOF, and the tokio runtime can't shut down — turning
        // a "TUI didn't quit" failure into a `cargo test` hang (Drop can't
        // help here: `self.child` is already taken).
        let mut killer = child.clone_killer();
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let _ = tx.send(child.wait());
        });
        match tokio::time::timeout(timeout, rx).await {
            Ok(Ok(Ok(status))) => Ok(status),
            Ok(Ok(Err(e))) => Err(anyhow!("child.wait: {e}")),
            Ok(Err(_)) => Err(anyhow!("TUI wait channel closed")),
            Err(_) => {
                let _ = killer.kill();
                Err(anyhow!("TUI did not exit within {:?}", timeout))
            }
        }
    }
}

/// Directory where e2e tests deposit artifacts (cast files,
/// screencast frames, screenshots, daemon logs). Idempotently
/// created on every call. CI's `actions/upload-artifact` step
/// picks up the whole directory so reviewers can download from
/// the workflow run page.
pub fn artifact_dir() -> Result<PathBuf> {
    let dir = target_dir().join("e2e-artifacts");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("create artifact_dir {}", dir.display()))?;
    Ok(dir)
}

/// Mark a freshly-copied file executable. `std::fs::copy`
/// preserves the source's mode on Unix, but be explicit so the
/// copied daemon is runnable regardless of the source's perms.
fn copy_executable_perms(path: &Path) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(path)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(path, perms)?;
    }
    let _ = path;
    Ok(())
}

/// Locate the `construct` binary in the workspace `target/`
/// directory. Honors `CARGO_TARGET_DIR` first, then falls back
/// to walking up two levels from `CARGO_MANIFEST_DIR`
/// (`crates/e2e` → workspace root).
fn construct_bin_path() -> Result<PathBuf> {
    bin_path("construct")
}

fn bin_path(name: &str) -> Result<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    // Match the test binary's own profile so `cargo test --release`
    // spawns release daemons/TUIs (which is what perf benchmarks
    // want — debug renders are an order of magnitude slower and
    // not representative of the real experience).
    let profile = if cfg!(debug_assertions) {
        "debug"
    } else {
        "release"
    };
    let p = target_dir().join(profile).join(&exe);
    if !p.exists() {
        anyhow::bail!(
            "expected {} — run `cargo build --workspace{}` before \
             `cargo test -p agentd-e2e{}`",
            p.display(),
            if profile == "release" {
                " --release"
            } else {
                ""
            },
            if profile == "release" {
                " --release"
            } else {
                ""
            },
        );
    }
    Ok(p)
}

fn target_dir() -> PathBuf {
    if let Ok(p) = std::env::var("CARGO_TARGET_DIR") {
        return PathBuf::from(p);
    }
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .and_then(|p| p.parent())
        .map(|p| p.join("target"))
        .expect("crates/e2e parent path resolution")
}
