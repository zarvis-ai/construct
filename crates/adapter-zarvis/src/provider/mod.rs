//! Provider plumbing: a small trait + normalized message/tool types,
//! plus per-provider implementations. The agent loop is generic over
//! [`LlmProvider`] so adding a new provider is one impl file.

use anyhow::Result;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use serde_json::Value;

pub mod anthropic;
pub mod ollama;
pub mod openai;
pub mod routing;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    System,
    User,
    Assistant,
    Tool,
}

/// One message in the rolling conversation we send to the model. Mirrors
/// the shape that maps cleanly onto OpenAI / Anthropic / Ollama wire
/// formats — each provider impl translates it to its own JSON.
///
/// Serializable so the agent loop can append each message to
/// `zarvis.jsonl` and replay on daemon-restart resume.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Content,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Content {
    /// Plain text (system / user / assistant).
    Text { text: String },
    /// Assistant turn that's making tool calls. May also include final
    /// pre-tool prose (`text`) that comes before the calls.
    AssistantToolCalls {
        text: Option<String>,
        calls: Vec<ToolCall>,
    },
    /// Single tool result, paired with its originating call id.
    ToolResult {
        call_id: String,
        output: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone)]
pub struct ToolSpec {
    pub name: String,
    pub description: String,
    /// JSON-schema-shaped object describing the tool's input.
    pub schema: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub input: Value,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct Usage {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub usd: f64,
}

/// Why the provider stopped producing tokens.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StopReason {
    /// Final assistant text; the agent loop should park awaiting input.
    EndTurn,
    /// Assistant emitted tool calls; the agent loop should run them and
    /// feed results back.
    ToolUse,
    /// Hit max tokens / other provider-side limit. Treat like EndTurn so
    /// the user can intervene.
    MaxTokens,
}

/// The aggregated result of one provider call.
#[derive(Debug)]
pub struct ProviderTurn {
    pub text: Option<String>,
    pub tool_calls: Vec<ToolCall>,
    pub stop_reason: StopReason,
    pub usage: Usage,
}

/// Sink for the assistant's streaming text deltas. Headless mode wires
/// this to `SessionEvent::Message` events; interactive (PTY) mode wires
/// it to raw `SessionEvent::Pty` bytes so the user sees the response
/// flow in the terminal pane. Provider impls don't care which it is.
pub trait TextSink: Send {
    fn delta(&mut self, text: &str);
}

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;

    /// Run one turn against the model. Implementations stream the
    /// response and push deltas through `sink` so the user sees the
    /// assistant text flowing as it arrives.
    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn>;
}
