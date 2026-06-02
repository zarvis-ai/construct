//! Persistent process sessions — child processes kept alive across tool
//! calls so the model can drive interactive programs (REPLs, prompts,
//! anything that reads stdin). `shell` with `interactive: true` starts a
//! session; the `write_stdin` tool feeds its stdin and drains new output.
//! Together with `shell` and `edit_file` this is the codex-style minimal
//! tool surface (`shell` + `edit_file` + `write_stdin`).
//!
//! Output is collected in the background into a shared buffer; each drain
//! returns only the bytes produced since the previous drain. A session is
//! removed once its process exits, and any still-running children are
//! killed when the registry drops (`kill_on_drop`).

use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::process::{Child, ChildStdin, Command};
use tokio::sync::Mutex;

/// Safety cap on concurrent live sessions; the oldest is reaped (killed)
/// when a new session would exceed it.
const MAX_SESSIONS: usize = 16;

struct Session {
    id: String,
    child: Child,
    stdin: Option<ChildStdin>,
    /// stdout+stderr, interleaved in arrival order, appended by reader tasks.
    buf: Arc<Mutex<Vec<u8>>>,
    /// How many bytes of `buf` have already been returned to the caller.
    drained: usize,
}

/// Result of draining a session after a yield window.
pub struct Drain {
    /// Output produced since the previous drain (lossily decoded UTF-8).
    pub output: String,
    /// `Some(code)` if the process has exited — the session is then removed.
    pub exit_code: Option<i32>,
}

/// Per-session registry of live child processes. Shared (via `Arc`) across
/// every `ToolCtx` in a session so a process started by one `shell` call is
/// reachable from later `write_stdin` calls.
#[derive(Default)]
pub struct ProcRegistry {
    sessions: Mutex<Vec<Session>>,
    counter: AtomicU64,
}

fn spawn_reader(mut stream: impl AsyncReadExt + Unpin + Send + 'static, buf: Arc<Mutex<Vec<u8>>>) {
    tokio::spawn(async move {
        let mut chunk = [0u8; 4096];
        loop {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buf.lock().await.extend_from_slice(&chunk[..n]),
            }
        }
    });
}

impl ProcRegistry {
    /// Spawn `command` under `bash -lc` as a persistent session. Returns the
    /// new session id. Output accumulates in the background; read it with
    /// [`ProcRegistry::drain`].
    pub async fn spawn(&self, cwd: &std::path::Path, command: &str) -> std::io::Result<String> {
        let mut child = Command::new("bash")
            .args(["-lc", command])
            .current_dir(cwd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let stdin = child.stdin.take();
        let buf = Arc::new(Mutex::new(Vec::<u8>::new()));
        if let Some(out) = child.stdout.take() {
            spawn_reader(out, buf.clone());
        }
        if let Some(err) = child.stderr.take() {
            spawn_reader(err, buf.clone());
        }
        let id = format!("proc-{}", self.counter.fetch_add(1, Ordering::Relaxed) + 1);

        let mut sessions = self.sessions.lock().await;
        if sessions.len() >= MAX_SESSIONS {
            sessions.remove(0); // drop oldest → kill_on_drop reaps it
        }
        sessions.push(Session {
            id: id.clone(),
            child,
            stdin,
            buf,
            drained: 0,
        });
        Ok(id)
    }

    /// Wait `yield_for`, then return output produced since the last drain and
    /// whether the process has exited. On exit the session is removed.
    /// `None` if `id` is unknown.
    pub async fn drain(&self, id: &str, yield_for: Duration) -> Option<Drain> {
        self.position(id).await?; // fast pre-check before sleeping
        tokio::time::sleep(yield_for).await;
        let mut sessions = self.sessions.lock().await;
        let idx = sessions.iter().position(|s| s.id == id)?;
        // Clone the buffer handle (an `Arc`) and copy the drain cursor out so
        // the guard below doesn't keep `sessions` borrowed while we update it.
        let buf = sessions[idx].buf.clone();
        let from = sessions[idx].drained;
        let (output, new_len) = {
            let b = buf.lock().await;
            let start = from.min(b.len());
            (String::from_utf8_lossy(&b[start..]).to_string(), b.len())
        };
        sessions[idx].drained = new_len;
        let exit = match sessions[idx].child.try_wait() {
            Ok(Some(status)) => Some(status.code().unwrap_or(-1)),
            _ => None,
        };
        if exit.is_some() {
            sessions.remove(idx);
        }
        Some(Drain {
            output,
            exit_code: exit,
        })
    }

    /// Write `data` to `id`'s stdin (closing it afterwards when `eof`), then
    /// drain after `yield_for`. `None` if `id` is unknown.
    pub async fn write(
        &self,
        id: &str,
        data: &str,
        eof: bool,
        yield_for: Duration,
    ) -> Option<Drain> {
        {
            let mut sessions = self.sessions.lock().await;
            let idx = sessions.iter().position(|s| s.id == id)?;
            if let Some(stdin) = sessions[idx].stdin.as_mut() {
                let _ = stdin.write_all(data.as_bytes()).await;
                let _ = stdin.flush().await;
            }
            if eof {
                sessions[idx].stdin = None; // drop closes the pipe → EOF
            }
        }
        self.drain(id, yield_for).await
    }

    async fn position(&self, id: &str) -> Option<usize> {
        self.sessions.lock().await.iter().position(|s| s.id == id)
    }

    /// Number of live sessions (test helper).
    #[cfg(test)]
    pub async fn session_count(&self) -> usize {
        self.sessions.lock().await.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn interactive_session_echoes_stdin() {
        let reg = ProcRegistry::default();
        let cwd = std::env::temp_dir();
        // `cat` echoes whatever it reads on stdin until EOF.
        let id = reg.spawn(&cwd, "cat").await.expect("spawn");
        assert_eq!(reg.session_count().await, 1);

        let d = reg
            .write(&id, "hello\n", false, Duration::from_millis(300))
            .await
            .expect("session exists");
        assert!(d.output.contains("hello"), "got: {:?}", d.output);
        assert!(d.exit_code.is_none(), "cat should still be running");

        // Closing stdin makes `cat` exit.
        let d = reg
            .write(&id, "", true, Duration::from_millis(300))
            .await
            .expect("session exists");
        assert_eq!(d.exit_code, Some(0));
        assert_eq!(reg.session_count().await, 0, "exited session is removed");
    }

    #[tokio::test]
    async fn unknown_session_returns_none() {
        let reg = ProcRegistry::default();
        assert!(reg
            .write("proc-999", "x", false, Duration::from_millis(10))
            .await
            .is_none());
        assert!(reg.drain("proc-999", Duration::from_millis(10)).await.is_none());
    }

    #[tokio::test]
    async fn short_command_exits_on_first_drain() {
        let reg = ProcRegistry::default();
        let cwd = std::env::temp_dir();
        let id = reg.spawn(&cwd, "echo done").await.expect("spawn");
        let d = reg
            .drain(&id, Duration::from_millis(300))
            .await
            .expect("session exists");
        assert!(d.output.contains("done"));
        assert_eq!(d.exit_code, Some(0));
        assert_eq!(reg.session_count().await, 0);
    }
}
