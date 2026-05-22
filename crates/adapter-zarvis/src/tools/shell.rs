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
         Honors a per-call timeout (default 30s). Use this for ad-hoc system queries \
         and one-off scripts; prefer dedicated tools (read_file, list_dir, find_files) \
         for their use cases."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "command":     { "type": "string", "description": "Shell command line; passed to `bash -lc`." },
                "timeout_sec": { "type": "integer", "minimum": 1, "maximum": 600, "default": 30 },
                "cwd":         { "type": "string", "description": "Working directory (defaults to the session's cwd)." }
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
}
