//! `shell` tool — run a bash command with a timeout, capture
//! stdout/stderr/exit. Runs in the session's cwd by default.

use super::{Tool, ToolCtx, ToolOutcome};
use agentd_protocol::ToolRisk;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::{json, Value};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncReadExt;
use tokio::process::Command;

pub struct Shell;

#[async_trait]
impl Tool for Shell {
    fn name(&self) -> &str {
        "shell"
    }
    fn description(&self) -> &str {
        "Run a shell command via `bash -lc`. Captures stdout, stderr, and exit code. \
         Honors a per-call timeout (default 30s); a one-off command that exceeds it is \
         killed. Use this for everything filesystem- and system-related: read files \
         (`cat`/`sed -n`), search (`grep`/`rg`), list (`ls`), run tests, git, etc. \
         Batch independent reads into one call (or issue them as parallel tool calls). \
         Set `interactive: true` to start a long-lived process (a REPL or a command \
         that prompts for input) instead of killing it at the timeout: the call returns \
         a `session_id` after a brief wait, and you then drive it with `write_stdin`."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command":     { "type": "string", "description": "Shell command line; passed to `bash -lc`." },
                "timeout_sec": { "type": "integer", "minimum": 1, "maximum": 600, "default": 30, "description": "Kill the (non-interactive) command after this many seconds." },
                "cwd":         { "type": "string", "description": "Working directory (defaults to the session's cwd)." },
                "interactive": { "type": "boolean", "default": false, "description": "Keep the process alive as a session for use with write_stdin, instead of killing it at the timeout." }
            },
            "required": ["command"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        let cmd = input
            .get("command")
            .and_then(|s| s.as_str())
            .unwrap_or("(missing command)");
        if cmd.len() > 200 {
            format!("{}…", &cmd[..200])
        } else {
            cmd.to_string()
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let cmd = input
            .get("command")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'command'"))?;
        let timeout_sec = input
            .get("timeout_sec")
            .and_then(|n| n.as_u64())
            .unwrap_or(30)
            .clamp(1, 600);
        let cwd = input
            .get("cwd")
            .and_then(|s| s.as_str())
            .map(std::path::PathBuf::from)
            .unwrap_or_else(|| ctx.cwd.clone());

        if !cwd.is_dir() {
            return Ok(ToolOutcome {
                ok: false,
                output: format!(
                    "shell: cwd '{}' does not exist or is not a directory (session worktree may have been removed)\n",
                    cwd.display()
                ),
            });
        }

        let interactive = input
            .get("interactive")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);
        if interactive {
            // Start a persistent session; return after a brief wait so the
            // model can drive it via write_stdin. The process is NOT killed
            // at timeout_sec — it lives until it exits or the session ends.
            let id = match ctx.procs.spawn(&cwd, cmd).await {
                Ok(id) => id,
                Err(e) => {
                    return Ok(ToolOutcome {
                        ok: false,
                        output: format!("shell: failed to start interactive session: {e}\n"),
                    })
                }
            };
            let drain = ctx.procs.drain(&id, INTERACTIVE_START_WAIT).await;
            return Ok(format_session(&id, drain, true));
        }

        let mut child = Command::new("bash")
            .args(["-lc", cmd])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()?;
        let mut stdout = child.stdout.take().expect("piped");
        let mut stderr = child.stderr.take().expect("piped");

        let timeout = Duration::from_secs(timeout_sec);
        let result = tokio::time::timeout(timeout, async {
            let mut so = Vec::new();
            let mut se = Vec::new();
            let _ = stdout.read_to_end(&mut so).await;
            let _ = stderr.read_to_end(&mut se).await;
            let status = child.wait().await?;
            Ok::<_, anyhow::Error>((status, so, se))
        })
        .await;

        match result {
            Ok(Ok((status, so, se))) => {
                let stdout_s = String::from_utf8_lossy(&so).to_string();
                let stderr_s = String::from_utf8_lossy(&se).to_string();
                let code = status.code().unwrap_or(-1);
                let body = format_output(&stdout_s, &stderr_s, code);
                Ok(ToolOutcome {
                    ok: status.success(),
                    output: body,
                })
            }
            Ok(Err(e)) => Ok(ToolOutcome {
                ok: false,
                output: format!("shell failed: {e}"),
            }),
            Err(_) => {
                // Timed out — child is killed by kill_on_drop when the future drops.
                Ok(ToolOutcome {
                    ok: false,
                    output: format!("shell timed out after {timeout_sec}s"),
                })
            }
        }
    }
}

fn format_output(stdout: &str, stderr: &str, exit_code: i32) -> String {
    let mut out = String::new();
    if !stdout.is_empty() {
        out.push_str("stdout:\n");
        out.push_str(stdout);
        if !stdout.ends_with('\n') {
            out.push('\n');
        }
    }
    if !stderr.is_empty() {
        out.push_str("stderr:\n");
        out.push_str(stderr);
        if !stderr.ends_with('\n') {
            out.push('\n');
        }
    }
    out.push_str(&format!("exit_code: {exit_code}\n"));
    out
}

/// How long `shell interactive: true` waits for the process to produce its
/// first output (e.g. a prompt) before returning the session handle.
const INTERACTIVE_START_WAIT: Duration = Duration::from_secs(3);

/// `write_stdin` tool — feed input to a process started by `shell` with
/// `interactive: true`, then return any new output.
pub struct WriteStdin;

#[async_trait]
impl Tool for WriteStdin {
    fn name(&self) -> &str {
        "write_stdin"
    }
    fn description(&self) -> &str {
        "Send input to a running interactive session started by `shell` with \
         interactive=true. Writes `data` to the process's stdin verbatim — include a \
         trailing newline to submit a line. Set `eof: true` to close stdin (signals \
         end-of-input, which many programs need to finish). Waits up to `timeout_sec` \
         for output and returns whatever the process emitted since the last call. The \
         session ends automatically when the process exits."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id":  { "type": "string", "description": "Session id returned by `shell` with interactive=true." },
                "data":        { "type": "string", "default": "", "description": "Bytes to write to stdin (include \\n to submit a line). Empty = just poll for new output." },
                "eof":         { "type": "boolean", "default": false, "description": "Close stdin after writing (sends EOF)." },
                "timeout_sec": { "type": "integer", "minimum": 1, "maximum": 600, "default": 5, "description": "How long to wait for output before returning." }
            },
            "required": ["session_id"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        let id = input.get("session_id").and_then(|s| s.as_str()).unwrap_or("?");
        let data = input.get("data").and_then(|s| s.as_str()).unwrap_or("");
        let eof = input.get("eof").and_then(|b| b.as_bool()).unwrap_or(false);
        let preview: String = data.chars().take(60).collect();
        format!("{id} <- {preview:?}{}", if eof { " +EOF" } else { "" })
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let id = input
            .get("session_id")
            .and_then(|s| s.as_str())
            .ok_or_else(|| anyhow::anyhow!("missing 'session_id'"))?;
        let data = input.get("data").and_then(|s| s.as_str()).unwrap_or("");
        let eof = input.get("eof").and_then(|b| b.as_bool()).unwrap_or(false);
        let timeout_sec = input
            .get("timeout_sec")
            .and_then(|n| n.as_u64())
            .unwrap_or(5)
            .clamp(1, 600);
        match ctx
            .procs
            .write(id, data, eof, Duration::from_secs(timeout_sec))
            .await
        {
            Some(drain) => Ok(format_session(id, Some(drain), false)),
            None => Ok(ToolOutcome {
                ok: false,
                output: format!("write_stdin: no live session '{id}' (it may have exited)\n"),
            }),
        }
    }
}

/// Render a process drain (or its absence) into a tool outcome — shared by
/// `shell interactive` (start) and `write_stdin` (continue).
fn format_session(id: &str, drain: Option<super::proc::Drain>, starting: bool) -> ToolOutcome {
    let Some(drain) = drain else {
        return ToolOutcome {
            ok: false,
            output: format!("session '{id}' not found\n"),
        };
    };
    let mut out = String::new();
    if starting {
        out.push_str(&format!("session_id: {id}\n"));
    }
    if !drain.output.is_empty() {
        out.push_str("output:\n");
        out.push_str(&drain.output);
        if !drain.output.ends_with('\n') {
            out.push('\n');
        }
    }
    match drain.exit_code {
        Some(code) => out.push_str(&format!("exit_code: {code} (session ended)\n")),
        None => out.push_str(&format!(
            "status: running (use write_stdin with session_id={id})\n"
        )),
    }
    ToolOutcome {
        ok: drain.exit_code.map(|c| c == 0).unwrap_or(true),
        output: out,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::{Tool, ToolCtx};

    fn ctx_with_cwd(cwd: std::path::PathBuf) -> ToolCtx {
        ToolCtx {
            cwd,
            session_id: "test".to_string(),
            client: tokio::sync::OnceCell::new(),
            emit: None,
            procs: std::sync::Arc::new(crate::tools::proc::ProcRegistry::default()),
        }
    }

    #[tokio::test]
    async fn returns_descriptive_error_when_cwd_missing() {
        let missing = std::env::temp_dir().join("agentd-zarvis-shell-test-missing-cwd-xyz123");
        assert!(!missing.exists(), "test precondition: path must not exist");

        let ctx = ctx_with_cwd(missing.clone());
        let outcome = Shell
            .run(json!({"command": "echo hi"}), &ctx)
            .await
            .expect("run returns Ok");

        assert!(!outcome.ok);
        assert!(
            outcome
                .output
                .contains("does not exist or is not a directory"),
            "unexpected output: {}",
            outcome.output
        );
        assert!(
            outcome.output.contains(&missing.display().to_string()),
            "output should name the missing path: {}",
            outcome.output
        );
    }

    #[tokio::test]
    async fn runs_command_when_cwd_exists() {
        let ctx = ctx_with_cwd(std::env::temp_dir());
        let outcome = Shell
            .run(
                json!({"command": "echo hello-from-shell-test", "timeout_sec": 10}),
                &ctx,
            )
            .await
            .expect("run returns Ok");

        assert!(outcome.ok, "command should succeed: {}", outcome.output);
        assert!(
            outcome.output.contains("hello-from-shell-test"),
            "stdout should contain the echoed string: {}",
            outcome.output
        );
        assert!(outcome.output.contains("exit_code: 0"));
    }

    #[tokio::test]
    async fn interactive_shell_and_write_stdin_roundtrip() {
        let ctx = ctx_with_cwd(std::env::temp_dir());
        let start = Shell
            .run(json!({"command": "cat", "interactive": true}), &ctx)
            .await
            .expect("run returns Ok");
        assert!(
            start.output.contains("session_id: proc-"),
            "expected a session id: {}",
            start.output
        );
        assert!(
            start.output.contains("status: running"),
            "cat should still be running: {}",
            start.output
        );
        let id = start
            .output
            .lines()
            .find_map(|l| l.strip_prefix("session_id: "))
            .expect("session id line")
            .to_string();

        let echoed = WriteStdin
            .run(
                json!({"session_id": id, "data": "ping\n", "timeout_sec": 1}),
                &ctx,
            )
            .await
            .expect("run returns Ok");
        assert!(
            echoed.output.contains("ping"),
            "cat should echo stdin: {}",
            echoed.output
        );

        let done = WriteStdin
            .run(json!({"session_id": id, "eof": true, "timeout_sec": 1}), &ctx)
            .await
            .expect("run returns Ok");
        assert!(
            done.output.contains("exit_code: 0"),
            "closing stdin should let cat exit: {}",
            done.output
        );
    }
}
