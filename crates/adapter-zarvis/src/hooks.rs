//! Optional zarvis hook runner.
//!
//! Hooks are intentionally opt-in through env/config, not auto-loaded from
//! the project tree, because they execute local commands with the user's
//! permissions.

use agentd_protocol::adapter::EventEmitter;
use anyhow::{Context, Result};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;
use tokio::io::AsyncWriteExt as _;
use tokio::process::Command;

const DEFAULT_TIMEOUT_MS: u64 = 10_000;
const MAX_LOG_CHARS: usize = 2_000;

#[derive(Debug, Clone, Default)]
pub struct Hooks {
    hooks: HashMap<String, Vec<HookCommand>>,
}

#[derive(Debug, Clone, Deserialize)]
struct HooksConfig {
    #[serde(default)]
    hooks: HashMap<String, Vec<HookCommand>>,
}

#[derive(Debug, Clone, Deserialize)]
struct HookCommand {
    command: String,
    #[serde(default)]
    timeout_ms: Option<u64>,
}

impl Hooks {
    pub fn load(cwd: &Path, emit: &EventEmitter) -> Self {
        match Self::try_load(cwd) {
            Ok(hooks) => hooks,
            Err(e) => {
                emit.log(format!("zarvis hooks disabled: {e}"));
                Self::default()
            }
        }
    }

    fn try_load(cwd: &Path) -> Result<Self> {
        let raw = if let Ok(s) = std::env::var("AGENTD_ZARVIS_HOOKS_JSON") {
            if s.trim().is_empty() {
                return Ok(Self::default());
            }
            s
        } else if let Ok(path) = std::env::var("AGENTD_ZARVIS_HOOKS_CONFIG") {
            if path.trim().is_empty() {
                return Ok(Self::default());
            }
            let path = expand_config_path(cwd, &path);
            std::fs::read_to_string(&path)
                .with_context(|| format!("read hooks config {}", path.display()))?
        } else {
            return Ok(Self::default());
        };
        let config: HooksConfig = serde_json::from_str(&raw).context("parse hooks config JSON")?;
        Ok(Self {
            hooks: config.hooks,
        })
    }

    pub async fn run(&self, event: &str, cwd: &Path, emit: &EventEmitter, payload: Value) {
        let Some(commands) = self.hooks.get(event) else {
            return;
        };
        for hook in commands {
            if let Err(e) = hook.run(event, cwd, payload.clone()).await {
                emit.log(format!("zarvis hook `{event}` failed: {e}"));
            }
        }
    }

    pub async fn mutate(
        &self,
        event: &str,
        cwd: &Path,
        emit: &EventEmitter,
        payload: Value,
    ) -> Value {
        let Some(commands) = self.hooks.get(event) else {
            return payload;
        };
        let mut current = payload;
        for hook in commands {
            match hook.run_capture(event, cwd, current.clone()).await {
                Ok(stdout) => {
                    let stdout = stdout.trim();
                    if stdout.is_empty() {
                        continue;
                    }
                    match serde_json::from_str::<Value>(stdout) {
                        Ok(Value::Object(fields)) => {
                            if let Some(base) = current.as_object_mut() {
                                for (k, v) in fields {
                                    base.insert(k, v);
                                }
                            }
                        }
                        Ok(_) => emit.log(format!(
                            "zarvis hook `{event}` ignored: mutating hooks must return a JSON object"
                        )),
                        Err(e) => emit.log(format!(
                            "zarvis hook `{event}` ignored invalid JSON stdout: {e}"
                        )),
                    }
                }
                Err(e) => emit.log(format!("zarvis hook `{event}` failed: {e}")),
            }
        }
        current
    }
}

impl HookCommand {
    async fn run(&self, event: &str, cwd: &Path, payload: Value) -> Result<()> {
        self.run_capture(event, cwd, payload).await.map(|_| ())
    }

    async fn run_capture(&self, event: &str, cwd: &Path, payload: Value) -> Result<String> {
        let timeout = Duration::from_millis(self.timeout_ms.unwrap_or(DEFAULT_TIMEOUT_MS));
        let shell = std::env::var("AGENTD_SHELL_BIN")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| "/bin/sh".to_string());
        let mut child = Command::new(shell)
            .arg("-lc")
            .arg(&self.command)
            .current_dir(cwd)
            .env("AGENTD_ZARVIS_HOOK_EVENT", event)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .with_context(|| format!("spawn `{}`", self.command))?;
        if let Some(mut stdin) = child.stdin.take() {
            let body = serde_json::to_vec(&payload).context("serialize hook payload")?;
            tokio::spawn(async move {
                let _ = stdin.write_all(&body).await;
                let _ = stdin.write_all(b"\n").await;
            });
        }
        let output = tokio::time::timeout(timeout, child.wait_with_output())
            .await
            .with_context(|| format!("timed out after {}ms", timeout.as_millis()))?
            .context("wait for hook command")?;
        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            let stdout = String::from_utf8_lossy(&output.stdout);
            anyhow::bail!(
                "`{}` exited {} stdout={} stderr={}",
                self.command,
                output.status,
                compact_log(&stdout),
                compact_log(&stderr)
            );
        }
        Ok(String::from_utf8_lossy(&output.stdout).to_string())
    }
}

pub fn base_payload(session_id: &str, cwd: &Path, mode: &str) -> Value {
    json!({
        "session_id": session_id,
        "cwd": cwd,
        "mode": mode,
        "at": chrono::Utc::now(),
    })
}

pub fn merge_payload(mut base: Value, fields: Value) -> Value {
    if let (Some(base), Some(fields)) = (base.as_object_mut(), fields.as_object()) {
        for (k, v) in fields {
            base.insert(k.clone(), v.clone());
        }
    }
    base
}

fn expand_config_path(cwd: &Path, path: &str) -> PathBuf {
    let p = PathBuf::from(path);
    if p.is_absolute() {
        p
    } else {
        cwd.join(p)
    }
}

fn compact_log(s: &str) -> String {
    let trimmed = s.trim();
    if trimmed.chars().count() <= MAX_LOG_CHARS {
        return trimmed.to_string();
    }
    let mut out: String = trimmed.chars().take(MAX_LOG_CHARS).collect();
    out.push_str("...");
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Mutex, OnceLock};

    fn env_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    #[test]
    fn parses_inline_hook_config() {
        let _guard = env_lock().lock().unwrap();
        std::env::set_var(
            "AGENTD_ZARVIS_HOOKS_JSON",
            r#"{"hooks":{"pre_tool_use":[{"command":"echo ok","timeout_ms":123}]}}"#,
        );
        std::env::remove_var("AGENTD_ZARVIS_HOOKS_CONFIG");
        let hooks = Hooks::try_load(Path::new("/tmp")).expect("load hooks");
        std::env::remove_var("AGENTD_ZARVIS_HOOKS_JSON");
        assert_eq!(hooks.hooks["pre_tool_use"][0].command, "echo ok");
        assert_eq!(hooks.hooks["pre_tool_use"][0].timeout_ms, Some(123));
    }

    #[test]
    fn loads_relative_config_path_from_cwd() {
        let _guard = env_lock().lock().unwrap();
        let dir = tempfile::tempdir().expect("tempdir");
        std::fs::write(
            dir.path().join("hooks.json"),
            r#"{"hooks":{"session_stop":[{"command":"printf stop"}]}}"#,
        )
        .expect("write hooks config");
        std::env::remove_var("AGENTD_ZARVIS_HOOKS_JSON");
        std::env::set_var("AGENTD_ZARVIS_HOOKS_CONFIG", "hooks.json");
        let hooks = Hooks::try_load(dir.path()).expect("load hooks");
        std::env::remove_var("AGENTD_ZARVIS_HOOKS_CONFIG");
        assert_eq!(hooks.hooks["session_stop"][0].command, "printf stop");
    }

    #[tokio::test]
    async fn hook_receives_payload_on_stdin() {
        let dir = tempfile::tempdir().expect("tempdir");
        let out = dir.path().join("payload.json");
        let hook = HookCommand {
            command: format!("cat > {}", shell_quote(&out.display().to_string())),
            timeout_ms: Some(1_000),
        };
        hook.run("session_start", dir.path(), json!({"hello":"world"}))
            .await
            .expect("run hook");
        let got = std::fs::read_to_string(out).expect("payload written");
        assert!(got.contains(r#""hello":"world""#), "{got}");
    }

    #[tokio::test]
    async fn mutating_hook_can_return_replacement_fields() {
        let dir = tempfile::tempdir().expect("tempdir");
        let hook = HookCommand {
            command: r#"printf '{"prompt":"rewritten"}'"#.to_string(),
            timeout_ms: Some(1_000),
        };
        let got = hook
            .run_capture(
                "user_prompt_mutate",
                dir.path(),
                json!({"prompt":"original"}),
            )
            .await
            .expect("run hook");
        let fields: Value = serde_json::from_str(got.trim()).expect("JSON output");
        let merged = merge_payload(json!({"prompt":"original","session_id":"s1"}), fields);
        assert_eq!(merged["prompt"], "rewritten");
        assert_eq!(merged["session_id"], "s1");
    }

    fn shell_quote(s: &str) -> String {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}
