//! Anthropic `/v1/messages` with SSE streaming + tool use.
//!
//! Uses the Messages API (current stable). Anthropic's tool-use loop:
//! the assistant emits `tool_use` content blocks; we respond with a
//! `tool_result` block on the next user message.
//!
//! The wire helpers ([`messages_to_anthropic`], [`tools_to_anthropic`],
//! [`read_message_stream`]) are shared with the subscription-OAuth
//! `claude-oauth` provider, which hits the same endpoint and differs only
//! in how it authenticates the request and shapes the system prompt.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

pub struct Anthropic {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Anthropic {
    pub fn from_env() -> Result<Self> {
        let api_key =
            std::env::var("ANTHROPIC_API_KEY").map_err(|_| anyhow!("ANTHROPIC_API_KEY not set"))?;
        Self::with_config(std::env::var("ANTHROPIC_BASE_URL").ok(), api_key)
    }

    /// Build with an explicit base URL (None → public Anthropic) and key.
    /// Used by named `[smith.models.*]` profiles.
    pub fn with_config(base_url: Option<String>, api_key: String) -> Result<Self> {
        let base_url = base_url
            .unwrap_or_else(|| "https://api.anthropic.com/v1".to_string())
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: reqwest::Client::builder()
                .build()
                .context("build reqwest client")?,
            base_url,
            api_key,
        })
    }
}

pub(crate) fn messages_to_anthropic(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    // Anthropic merges consecutive same-role messages on the wire.
    // We don't, but we do skip the system role (passed top-level).
    for m in messages {
        match (m.role, &m.content) {
            (Role::System, _) => {} // attached as `system` field on the request
            (_, Content::Text { text }) => {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "assistant",
                    Role::Tool => "user", // tool_result blocks live in user messages
                    Role::System => unreachable!(),
                };
                out.push(json!({ "role": role, "content": text }));
            }
            (_, Content::AssistantToolCalls { text, calls }) => {
                let mut blocks: Vec<Value> = Vec::with_capacity(calls.len() + 1);
                if let Some(t) = text {
                    if !t.is_empty() {
                        blocks.push(json!({ "type": "text", "text": t }));
                    }
                }
                for c in calls {
                    blocks.push(json!({
                        "type": "tool_use",
                        "id": c.id,
                        "name": c.name,
                        "input": c.input,
                    }));
                }
                out.push(json!({ "role": "assistant", "content": blocks }));
            }
            (
                _,
                Content::ToolResult {
                    call_id,
                    output,
                    is_error,
                },
            ) => {
                let block = json!({
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": output,
                    "is_error": *is_error,
                });
                out.push(json!({ "role": "user", "content": [block] }));
            }
            (_, Content::Summary { text, .. }) => {
                let body = format!("{}{}", super::SUMMARY_WIRE_PREFIX, text);
                out.push(json!({ "role": "user", "content": body }));
            }
            // codex-oauth-only; nothing to send to the Anthropic API.
            (_, Content::Reasoning(_)) => {}
        }
    }
    out
}

pub(crate) fn tools_to_anthropic(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "input_schema": t.schema,
            })
        })
        .collect()
}

/// Shared handler for an Anthropic Messages API streaming response: checks
/// the HTTP status (mapping context-overflow 400s to [`super::ContextOverflow`]
/// so the agent loop's learn-and-retry path can fire), then parses the typed
/// SSE event stream into a [`ProviderTurn`]. Both the API-key `anthropic`
/// provider and the subscription-OAuth `claude-oauth` provider feed their
/// already-sent `reqwest::Response` through here.
pub(crate) async fn read_message_stream(
    resp: reqwest::Response,
    sink: &mut dyn TextSink,
) -> Result<ProviderTurn> {
    if !resp.status().is_success() {
        let code = resp.status();
        let body = resp.text().await.unwrap_or_default();
        if code.as_u16() == 400 {
            if let Some(extracted) = super::parse_overflow(&body) {
                return Err(anyhow::Error::new(super::ContextOverflow {
                    extracted,
                    raw: body,
                }));
            }
        }
        return Err(anyhow!("anthropic {code}: {body}"));
    }

    let mut stream = resp.bytes_stream().eventsource();

    // Anthropic's stream uses typed events:
    //   message_start, content_block_start (text or tool_use),
    //   content_block_delta (text_delta or input_json_delta),
    //   content_block_stop, message_delta (stop_reason + usage),
    //   message_stop.
    let mut assistant_text = String::new();
    let mut blocks: Vec<BlockAcc> = Vec::new();
    let mut stop_reason = StopReason::EndTurn;
    let mut usage = Usage::default();

    while let Some(ev) = stream.next().await {
        let ev = ev.context("anthropic SSE stream")?;
        sink.progress();
        if ev.data.is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => continue,
        };
        let ty = v.get("type").and_then(|s| s.as_str()).unwrap_or("");
        match ty {
            "message_start" => {
                if let Some(u) = v.pointer("/message/usage") {
                    usage.input_tokens =
                        u.get("input_tokens").and_then(|n| n.as_u64()).unwrap_or(0);
                    usage.cached_tokens = u
                        .get("cache_read_input_tokens")
                        .and_then(|n| n.as_u64())
                        .unwrap_or(0);
                }
            }
            "content_block_start" => {
                let idx = v.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
                while blocks.len() <= idx {
                    blocks.push(BlockAcc::default());
                }
                let block = v.get("content_block").cloned().unwrap_or(Value::Null);
                let bty = block.get("type").and_then(|s| s.as_str()).unwrap_or("");
                match bty {
                    "tool_use" => {
                        blocks[idx].kind = BlockKind::ToolUse;
                        blocks[idx].id = block
                            .get("id")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        blocks[idx].name = block
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                    }
                    "thinking" => {
                        blocks[idx].kind = BlockKind::Thinking;
                    }
                    _ => {
                        blocks[idx].kind = BlockKind::Text;
                    }
                }
            }
            "content_block_delta" => {
                let idx = v.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
                if idx >= blocks.len() {
                    continue;
                }
                let delta = v.get("delta").cloned().unwrap_or(Value::Null);
                let dty = delta.get("type").and_then(|s| s.as_str()).unwrap_or("");
                match dty {
                    "text_delta" => {
                        if let Some(t) = delta.get("text").and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                sink.delta(t);
                                blocks[idx].text.push_str(t);
                                assistant_text.push_str(t);
                            }
                        }
                    }
                    "input_json_delta" => {
                        if let Some(j) = delta.get("partial_json").and_then(|s| s.as_str()) {
                            blocks[idx].input_json.push_str(j);
                        }
                    }
                    // Extended-thinking content: stream into the
                    // sink's reasoning channel so the TUI renders
                    // it dim/italic and the headless transcript
                    // gets a separate `SessionEvent::Reasoning`.
                    // The accompanying `signature_delta` event
                    // (Anthropic-internal signature for the
                    // thinking block; not user-visible) is
                    // ignored by the catch-all below.
                    "thinking_delta" => {
                        if let Some(t) = delta.get("thinking").and_then(|s| s.as_str()) {
                            if !t.is_empty() {
                                sink.reasoning_delta(t);
                                blocks[idx].text.push_str(t);
                            }
                        }
                    }
                    _ => {}
                }
            }
            "message_delta" => {
                if let Some(reason) = v.pointer("/delta/stop_reason").and_then(|s| s.as_str()) {
                    stop_reason = match reason {
                        "tool_use" => StopReason::ToolUse,
                        "max_tokens" => StopReason::MaxTokens,
                        _ => StopReason::EndTurn,
                    };
                }
                if let Some(u) = v.pointer("/usage") {
                    if let Some(n) = u.get("output_tokens").and_then(|n| n.as_u64()) {
                        usage.output_tokens = n;
                    }
                }
            }
            _ => {}
        }
    }

    let mut tool_calls: Vec<ToolCall> = Vec::new();
    for b in blocks {
        if matches!(b.kind, BlockKind::ToolUse) {
            let input = if b.input_json.is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&b.input_json).unwrap_or_else(|_| json!({}))
            };
            tool_calls.push(ToolCall {
                id: b.id,
                name: b.name,
                input,
            });
        }
    }

    Ok(ProviderTurn {
        text: if assistant_text.is_empty() {
            None
        } else {
            Some(assistant_text)
        },
        tool_calls,
        stop_reason,
        usage,
        reasoning_items: Vec::new(),
    })
}

#[async_trait]
impl LlmProvider for Anthropic {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let mut body = json!({
            "model": model,
            "max_tokens": 8192,
            "stream": true,
            "messages": messages_to_anthropic(messages),
        });
        if !system.is_empty() {
            body["system"] = json!(system);
        }
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools_to_anthropic(tools));
        }

        let url = format!("{}/messages", self.base_url);
        let resp = self
            .client
            .post(&url)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", "2023-06-01")
            .json(&body)
            .send()
            .await
            .context("anthropic POST /messages")?;
        read_message_stream(resp, sink).await
    }
}

#[derive(Default)]
struct BlockAcc {
    kind: BlockKind,
    text: String,
    id: String,
    name: String,
    input_json: String,
}

#[derive(Default)]
enum BlockKind {
    #[default]
    Text,
    ToolUse,
    /// Extended-thinking content block from Anthropic models that
    /// support reasoning (e.g. claude-3.7-sonnet thinking mode).
    /// Streamed via `thinking_delta` content-block deltas; we route
    /// these to `TextSink::reasoning_delta` instead of `delta`.
    Thinking,
}
