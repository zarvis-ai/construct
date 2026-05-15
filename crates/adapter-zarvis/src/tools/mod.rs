//! Tool registry: a trait + each tool's risk classification + a
//! truncation helper that decides what to send back into the model
//! context vs what the user sees in the transcript.

use crate::provider::ToolSpec;
use agentd_client::Client;
use agentd_protocol::ToolRisk;
use anyhow::Result;
use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;

pub mod agentd;
pub mod fs;
pub mod shell;

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

/// Context shared with every tool invocation. `cwd` is the session's
/// working directory; the daemon `Client` is opened lazily on first
/// agentd-control tool call.
pub struct ToolCtx {
    pub cwd: std::path::PathBuf,
    pub session_id: String,
    pub client: tokio::sync::OnceCell<Arc<Client>>,
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
            // agentd-control tools
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
            // Recurring-prompt loops (daemon scheduler).
            Box::new(agentd::LoopCreate),
            Box::new(agentd::LoopList),
            Box::new(agentd::LoopUpdate),
            Box::new(agentd::LoopRemove),
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
}
