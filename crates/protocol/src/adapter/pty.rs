//! PTY-backed adapter runtime.
//!
//! Helper that spawns a child under a real PTY, pumps bytes between
//! `portable-pty` and the adapter's [`AdapterContext`], and emits
//! [`SessionEvent::Pty`] / lifecycle events on the adapter's behalf.
//!
//! Available behind the `pty` feature of `agentd-protocol`.

use super::{AdapterContext, AdapterInboxMsg};
use crate::{PtySize, SessionEvent, SessionState};
use portable_pty::{native_pty_system, CommandBuilder};
use std::io::{Read, Write};
use std::path::PathBuf;
use tokio::sync::mpsc;

const READ_BUF: usize = 8 * 1024;

/// What to spawn under the PTY and how.
pub struct PtySpec {
    pub bin: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub size: PtySize,
    /// Free-form label that's emitted in the initial Status event's `detail`.
    pub status_detail: Option<String>,
}

/// Drive a PTY-backed session. Emits `Status(Running)` → byte stream
/// (`Pty` events) → `Done`. Honors `PtyInput`, `PtyResize`, `Interrupt`,
/// `Stop`, and line-oriented `Input` (appended with `\n`).
///
/// Returns the child's exit code (or `-1` if not available).
pub async fn run_session(spec: PtySpec, ctx: AdapterContext) -> i32 {
    let AdapterContext {
        session_id: _,
        emit,
        mut inbox,
    } = ctx;

    let pty_system = native_pty_system();
    let pair = match pty_system.openpty(portable_pty::PtySize {
        cols: spec.size.cols,
        rows: spec.size.rows,
        pixel_width: 0,
        pixel_height: 0,
    }) {
        Ok(p) => p,
        Err(e) => {
            emit.emit(SessionEvent::Error {
                message: format!("openpty: {e}"),
            });
            emit.emit(SessionEvent::Done { exit_code: 127 });
            return 127;
        }
    };

    let mut cmd = CommandBuilder::new(&spec.bin);
    for a in &spec.args {
        cmd.arg(a);
    }
    cmd.cwd(&spec.cwd);
    cmd.env(
        "TERM",
        std::env::var("TERM").unwrap_or_else(|_| "xterm-256color".into()),
    );
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    let child = match pair.slave.spawn_command(cmd) {
        Ok(c) => c,
        Err(e) => {
            let io_err = std::io::Error::other(e.to_string());
            emit.emit(SessionEvent::Error {
                message: super::missing_bin_hint(
                    &spec.status_detail.as_deref().unwrap_or(&spec.bin),
                    &io_err,
                ),
            });
            emit.emit(SessionEvent::Done { exit_code: 127 });
            return 127;
        }
    };

    let mut killer = child.clone_killer();
    let master = pair.master;
    let slave = pair.slave;

    let reader = match master.try_clone_reader() {
        Ok(r) => r,
        Err(e) => {
            emit.emit(SessionEvent::Error {
                message: format!("pty reader: {e}"),
            });
            emit.emit(SessionEvent::Done { exit_code: 1 });
            return 1;
        }
    };
    let writer = match master.take_writer() {
        Ok(w) => w,
        Err(e) => {
            emit.emit(SessionEvent::Error {
                message: format!("pty writer: {e}"),
            });
            emit.emit(SessionEvent::Done { exit_code: 1 });
            return 1;
        }
    };

    let (read_tx, mut read_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut reader = reader;
        let mut buf = vec![0u8; READ_BUF];
        loop {
            match reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if read_tx.send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let (write_tx, mut write_rx) = mpsc::unbounded_channel::<Vec<u8>>();
    std::thread::spawn(move || {
        let mut writer = writer;
        while let Some(bytes) = write_rx.blocking_recv() {
            if writer.write_all(&bytes).is_err() {
                break;
            }
            let _ = writer.flush();
        }
    });

    let mut wait_handle = tokio::task::spawn_blocking(move || {
        let _slave_alive = slave;
        let mut child = child;
        child.wait()
    });

    emit.emit(SessionEvent::Status {
        state: SessionState::Running,
        detail: spec.status_detail.clone(),
    });

    let mut read_closed = false;
    let mut inbox_closed = false;
    let exit_code: i32;
    loop {
        tokio::select! {
            biased;
            bytes = read_rx.recv(), if !read_closed => {
                match bytes {
                    Some(b) => emit.emit_pty(&b),
                    None => { read_closed = true; }
                }
            }
            msg = inbox.recv(), if !inbox_closed => {
                match msg {
                    None => { inbox_closed = true; }
                    Some(AdapterInboxMsg::PtyInput(b)) => {
                        let _ = write_tx.send(b);
                    }
                    Some(AdapterInboxMsg::PtyResize { cols, rows }) => {
                        let _ = master.resize(portable_pty::PtySize {
                            cols, rows,
                            pixel_width: 0, pixel_height: 0,
                        });
                    }
                    Some(AdapterInboxMsg::Input(text)) => {
                        let mut b = text.into_bytes();
                        if !b.ends_with(b"\n") { b.push(b'\n'); }
                        let _ = write_tx.send(b);
                    }
                    Some(AdapterInboxMsg::Interrupt) => {
                        // ETX → child's SIGINT path.
                        let _ = write_tx.send(vec![0x03]);
                    }
                    Some(AdapterInboxMsg::Stop) => {
                        let _ = killer.kill();
                    }
                    // PTY-mode adapters don't gate tool calls — ignore.
                    Some(AdapterInboxMsg::ToolDecision { .. })
                    | Some(AdapterInboxMsg::SetApprovalMode(_))
                    | Some(AdapterInboxMsg::ToolAction { .. }) => {}
                }
            }
            res = &mut wait_handle => {
                exit_code = match res {
                    Ok(Ok(status)) => {
                        if status.success() { 0 } else { status.exit_code() as i32 }
                    }
                    _ => -1,
                };
                while let Ok(b) = read_rx.try_recv() {
                    emit.emit_pty(&b);
                }
                break;
            }
        }
    }
    emit.emit(SessionEvent::Done { exit_code });
    exit_code
}
