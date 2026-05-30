//! Tool registry: a trait + each tool's risk classification + a
//! truncation helper that decides what to send back into the model
//! context vs what the user sees in the transcript.

use crate::provider::ToolSpec;
use agentd_client::Client;
use agentd_protocol::{adapter::EventEmitter, ToolRisk};
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub mod agentd;
pub mod browser;
pub mod fs;
pub mod shell;
pub mod subagent;

/// Per-tool result. `output` is the full tool output as the user should
/// see it in the transcript; the agent loop separately calls
/// [`truncate_for_model`] before stuffing it back into the context.
#[derive(Debug)]
pub struct ToolOutcome {
    pub ok: bool,
    pub output: String,
}

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> Value;
    fn risk(&self) -> ToolRisk;
    /// Pre-formatted, single-line human summary of the call args, used
    /// in the approval prompt. Keep short.
    fn args_summary(&self, input: &Value) -> String {
        let s = serde_json::to_string(input).unwrap_or_default();
        if s.len() > 200 {
            format!("{}…", &s[..200])
        } else {
            s
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome>;
}

/// Effective risk for a single tool call, after applying the daemon-defined
/// auto-approval policy. A tool whose intrinsic risk is [`ToolRisk::Risky`]
/// is downgraded to [`ToolRisk::Safe`] when its target path is covered by
/// [`agentd_protocol::adapter::policy::AutoApprovePolicy`] — that's how the
/// "agentd defines the policy once, harnesses honor it" abstraction lands on
/// zarvis. Other Risky calls keep their gate.
pub fn effective_risk(tool: &dyn Tool, input: &Value, cwd: &std::path::Path) -> ToolRisk {
    if matches!(tool.risk(), ToolRisk::Safe) {
        return ToolRisk::Safe;
    }
    if auto_approve_covers(tool.name(), input, cwd) {
        return ToolRisk::Safe;
    }
    tool.risk()
}

fn auto_approve_covers(tool_name: &str, input: &Value, cwd: &std::path::Path) -> bool {
    // Only path-targeted file writes are eligible. Shell, browser, subagent,
    // and agentd-control tools are Risky for reasons unrelated to a file
    // path and must keep their gate.
    if !matches!(tool_name, "write_file" | "edit_file") {
        return false;
    }
    let Some(rel) = input.get("path").and_then(|v| v.as_str()) else {
        return false;
    };
    let policy = agentd_protocol::adapter::policy::AutoApprovePolicy::from_env();
    if policy.is_empty() {
        return false;
    }
    let abs = fs::resolve(cwd, rel);
    policy.allows_path_write(&abs)
}

/// Context shared with every tool invocation. `cwd` is the session's
/// working directory; the daemon `Client` is opened lazily on first
/// agentd-control tool call.
pub struct ToolCtx {
    pub cwd: std::path::PathBuf,
    pub session_id: String,
    pub client: tokio::sync::OnceCell<Arc<Client>>,
    pub emit: Option<EventEmitter>,
}

pub struct ToolRegistry {
    tools: Vec<Box<dyn Tool>>,
}

impl ToolRegistry {
    pub fn with_defaults() -> Self {
        let tools: Vec<Box<dyn Tool>> = vec![
            Box::new(shell::Shell),
            Box::new(fs::ReadFile),
            Box::new(fs::WriteFile),
            Box::new(fs::EditFile),
            Box::new(fs::ListDir),
            Box::new(fs::FindFiles),
            // Chrome DevTools browser tools
            Box::new(browser::BrowserOpen),
            Box::new(browser::BrowserInspect),
            Box::new(browser::BrowserScreenshot),
            Box::new(browser::BrowserEval),
            // agentd-control tools
            Box::new(agentd::Context),
            Box::new(agentd::Whoami),
            Box::new(agentd::ListSessions),
            Box::new(agentd::GetSession),
            Box::new(agentd::GetTranscript),
            Box::new(agentd::GetOutput),
            Box::new(agentd::GetDiff),
            Box::new(agentd::ListHarnesses),
            Box::new(agentd::CreateSession),
            Box::new(agentd::SendInput),
            Box::new(agentd::SendKeys),
            Box::new(agentd::InterruptSession),
            Box::new(agentd::StopSession),
            Box::new(agentd::KillSession),
            Box::new(agentd::DeleteSession),
            Box::new(agentd::PinSession),
            Box::new(agentd::RenameSession),
            Box::new(agentd::SetSessionGroup),
            Box::new(agentd::MoveSession),
            // Recurring-prompt loops (daemon scheduler).
            Box::new(agentd::LoopCreate),
            Box::new(agentd::LoopList),
            Box::new(agentd::LoopUpdate),
            Box::new(agentd::LoopRemove),
            // Zarvis-owned subagents: hidden backing sessions exposed as
            // task-like child agents to this parent session.
            Box::new(subagent::Create),
            Box::new(subagent::List),
            Box::new(subagent::Peek),
            Box::new(subagent::Enqueue),
            Box::new(subagent::Cancel),
            Box::new(subagent::Delete),
        ];
        Self { tools }
    }

    pub fn specs(&self) -> Vec<ToolSpec> {
        self.tools
            .iter()
            .map(|t| ToolSpec {
                name: t.name().to_string(),
                description: t.description().to_string(),
                schema: t.schema(),
            })
            .collect()
    }

    pub fn get(&self, name: &str) -> Option<&dyn Tool> {
        self.tools
            .iter()
            .find(|t| t.name() == name)
            .map(|b| b.as_ref())
    }
}

/// Trim a tool's raw output to a model-context budget. Keeps the head
/// and tail with a `[N bytes elided]` marker between, so the model sees
/// both the start and the final result of long commands. Full output
/// always goes to the transcript regardless.
pub fn truncate_for_model(s: &str, limit: usize) -> String {
    if s.len() <= limit {
        return s.to_string();
    }
    let head = (limit * 3) / 4;
    let tail = limit - head;
    // Snap to UTF-8 boundaries so we don't slice a multi-byte char.
    let head_end = floor_char_boundary(s, head);
    let tail_start = ceil_char_boundary(s, s.len().saturating_sub(tail));
    let elided = s.len() - head_end - (s.len() - tail_start);
    format!(
        "{}\n[… {} bytes elided …]\n{}",
        &s[..head_end],
        elided,
        &s[tail_start..]
    )
}

fn floor_char_boundary(s: &str, mut i: usize) -> usize {
    if i >= s.len() {
        return s.len();
    }
    while i > 0 && !s.is_char_boundary(i) {
        i -= 1;
    }
    i
}

fn ceil_char_boundary(s: &str, mut i: usize) -> usize {
    while i < s.len() && !s.is_char_boundary(i) {
        i += 1;
    }
    i
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn truncate_small_passthrough() {
        let s = "hello world";
        assert_eq!(truncate_for_model(s, 100), s);
    }

    #[test]
    fn truncate_large_keeps_head_and_tail() {
        let s = "a".repeat(10_000);
        let out = truncate_for_model(&s, 100);
        assert!(out.contains("[… "));
        assert!(out.contains("elided"));
        assert!(out.len() < 250); // head + marker + tail
        assert!(out.starts_with("a"));
        assert!(out.ends_with("a"));
    }

    #[test]
    fn truncate_respects_utf8_boundaries() {
        // 2-byte chars
        let s = "é".repeat(1000);
        let out = truncate_for_model(&s, 100);
        // Should be valid UTF-8 (just by virtue of String existing).
        assert!(out.contains("[… "));
    }

    #[test]
    fn effective_risk_downgrades_widget_dir_writes() {
        use agentd_protocol::adapter::policy::ENV_AUTO_APPROVE_PATHS;
        use serde_json::json;

        let widgets = std::env::temp_dir().join("agentd-policy-test-widgets");
        // SAFETY: rust 1.86+ marks env mutation unsafe; tests are
        // single-threaded for this module so the unsafe block is acceptable.
        std::env::set_var(ENV_AUTO_APPROVE_PATHS, &widgets);

        let cwd = std::path::PathBuf::from("/some/proj");
        let write = fs::WriteFile;
        let edit = fs::EditFile;
        let shell = shell::Shell;

        // Write into the widget dir → downgraded to Safe (no approval gate).
        let widget_path = widgets.join("status.md");
        assert!(matches!(
            effective_risk(&write, &json!({ "path": widget_path.to_string_lossy() }), &cwd),
            ToolRisk::Safe
        ));
        assert!(matches!(
            effective_risk(
                &edit,
                &json!({ "path": widget_path.to_string_lossy(), "find": "a", "replace": "b" }),
                &cwd,
            ),
            ToolRisk::Safe
        ));

        // Write outside the widget dir → keeps its Risky gate.
        assert!(matches!(
            effective_risk(&write, &json!({ "path": "/some/proj/other.md" }), &cwd),
            ToolRisk::Risky
        ));

        // Shell stays Risky regardless — auto-approve is path-scoped, not a
        // blanket waiver for every Risky tool.
        assert!(matches!(
            effective_risk(&shell, &json!({ "cmd": "ls" }), &cwd),
            ToolRisk::Risky
        ));

        std::env::remove_var(ENV_AUTO_APPROVE_PATHS);
    }

    #[test]
    fn registry_includes_subagent_tools() {
        let registry = ToolRegistry::with_defaults();
        assert!(
            registry.get("agentd_context").is_some(),
            "missing tool agentd_context"
        );
        for name in [
            "agentd_subagent_create",
            "agentd_subagent_list",
            "agentd_subagent_peek",
            "agentd_subagent_enqueue",
            "agentd_subagent_cancel",
            "agentd_subagent_delete",
        ] {
            assert!(registry.get(name).is_some(), "missing tool {name}");
        }
    }
}
