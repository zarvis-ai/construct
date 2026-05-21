//! End-to-end test harness for `agentd`.
//!
//! Spawns the **real** `agentd` binary out of the workspace's
//! `target/debug/` against a fresh tempdir for every test (so
//! the test never touches the developer's actual `$AGENTD_*_DIR`
//! state), waits for the IPC socket to come up, and returns a
//! connected `agentd_client::Client` plus the path to the
//! socket. Drop kills the daemon and cleans the tempdir.
//!
//! There's a sibling [`Tui`] helper that spawns the `agent` TUI
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

use agentd_client::Client;

/// One isolated `agentd` instance + a connected IPC client.
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
    /// Pre-connected IPC client. Tests start using it directly —
    /// no separate "connect" step.
    pub client: Arc<Client>,
    #[allow(dead_code)]
    child: Child,
}

impl Daemon {
    /// Spawn a fresh `agentd` against a tempdir and wait for its
    /// IPC socket to come up. Always sets
    /// `AGENTD_REMOTE_NO_TUNNEL=1` because the e2e tests should
    /// never spawn a real cloudflared subprocess (would publish a
    /// real tunnel URL and the CI runner can't reach a `*.try
    /// cloudflare.com` host anyway).
    ///
    /// Boot timeout is 15s — slow enough to absorb the
    /// orchestrator-spawn timeout on hosts where the adapter
    /// binaries can't be located.
    pub async fn spawn() -> Result<Self> {
        let dir = tempfile::tempdir().context("create tempdir")?;
        let runtime_dir = dir.path().join("run");
        let state_dir = dir.path().join("state");
        let data_dir = dir.path().join("data");
        let config_dir = dir.path().join("config");
        for d in [&runtime_dir, &state_dir, &data_dir, &config_dir] {
            std::fs::create_dir_all(d)?;
        }
        let socket = runtime_dir.join("agentd.sock");

        let bin = agentd_bin_path()?;
        let mut cmd = Command::new(&bin);
        cmd.env("AGENTD_RUNTIME_DIR", &runtime_dir)
            .env("AGENTD_STATE_DIR", &state_dir)
            .env("AGENTD_DATA_DIR", &data_dir)
            .env("AGENTD_CONFIG_DIR", &config_dir)
            // Skip cloudflared in every e2e test — its absence
            // from CI runners is not a test failure.
            .env("AGENTD_REMOTE_NO_TUNNEL", "1")
            .args(["run", "--socket"])
            .arg(&socket)
            // Silence the daemon's stderr / stdout in tests by
            // default — flip these to `inherit()` while debugging.
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true);

        let child = cmd
            .spawn()
            .with_context(|| format!("spawn {}", bin.display()))?;

        let deadline = Instant::now() + Duration::from_secs(15);
        while !socket.exists() {
            if Instant::now() > deadline {
                anyhow::bail!(
                    "daemon did not bind {} within 15s",
                    socket.display()
                );
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
            client,
            child,
        })
    }
}

/// A live `agent` TUI bound to a particular daemon socket, with
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
        // `wait_for` from a test leaves the agent process alive
        // and the tokio runtime hangs waiting on the PTY reader.
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
        }
    }
}

impl Tui {
    /// Spawn `agent tui --socket <socket>` in a 30x100 PTY. The
    /// dimensions are arbitrary but match what the TUI tests
    /// expect for layout assertions.
    pub fn spawn(socket: &Path) -> Result<Self> {
        let agent = agent_bin_path()?;
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

        let mut cmd = portable_pty::CommandBuilder::new(&agent);
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
            .map_err(|e| anyhow!("spawn agent tui: {e}"))?;
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
            size.rows,
            size.cols,
            // Scrollback budget: 1000 lines is plenty for any
            // assertion the tests do today and small enough not
            // to hold huge memory.
            1000,
        )));
        let parser_for_task = parser.clone();
        let reader_task = tokio::task::spawn_blocking(move || {
            let mut buf = [0u8; 4096];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let mut p = parser_for_task.lock().unwrap();
                        p.process(&buf[..n]);
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
        // `portable_pty::Child::wait` is blocking and takes
        // `&mut self`, so we ferry it across a spawn_blocking
        // and a oneshot.
        let (tx, rx) = tokio::sync::oneshot::channel();
        std::thread::spawn(move || {
            let status = child.wait();
            let _ = tx.send(status);
        });
        let status = tokio::time::timeout(timeout, rx)
            .await
            .map_err(|_| anyhow!("TUI did not exit within {:?}", timeout))?
            .map_err(|_| anyhow!("TUI wait channel closed"))?
            .map_err(|e| anyhow!("child.wait: {e}"))?;
        Ok(status)
    }
}

/// Locate the `agentd` binary in the workspace `target/`
/// directory. Honors `CARGO_TARGET_DIR` first, then falls back
/// to walking up two levels from `CARGO_MANIFEST_DIR`
/// (`crates/e2e` → workspace root).
fn agentd_bin_path() -> Result<PathBuf> {
    bin_path("agentd")
}

fn agent_bin_path() -> Result<PathBuf> {
    bin_path("agent")
}

fn bin_path(name: &str) -> Result<PathBuf> {
    let exe = if cfg!(windows) {
        format!("{name}.exe")
    } else {
        name.to_string()
    };
    let p = target_dir().join("debug").join(&exe);
    if !p.exists() {
        anyhow::bail!(
            "expected {} — run `cargo build --workspace` before `cargo test -p agentd-e2e`",
            p.display()
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
