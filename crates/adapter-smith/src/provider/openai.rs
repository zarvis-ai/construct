//! OpenAI `/v1/chat/completions` with SSE streaming + tool calling.
//!
//! We target chat.completions (not the newer Responses API) because it's
//! the most stable shape across vendors that mimic OpenAI's surface
//! (Together, Groq, DeepSeek, etc.) — if a user points
//! `OPENAI_BASE_URL` at one of those, the same code path works.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};

pub struct OpenAi {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl OpenAi {
    pub fn from_env() -> Result<Self> {
        let api_key =
            std::env::var("OPENAI_API_KEY").map_err(|_| anyhow!("OPENAI_API_KEY not set"))?;
        Self::with_config(std::env::var("OPENAI_BASE_URL").ok(), api_key)
    }

    /// Build with an explicit base URL (None → public OpenAI) and key.
    /// Used by named `[smith.models.*]` profiles so several OpenAI-compatible
    /// endpoints can coexist in one session, independent of `OPENAI_BASE_URL`.
    pub fn with_config(base_url: Option<String>, api_key: String) -> Result<Self> {
        let base_url = base_url
            .unwrap_or_else(|| "https://api.openai.com/v1".to_string())
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

fn role_str(r: Role) -> &'static str {
    match r {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn messages_to_openai(system: &str, messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::with_capacity(messages.len() + 1);
    if !system.is_empty() {
        out.push(json!({ "role": "system", "content": system }));
    }
    for m in messages {
        match &m.content {
            Content::Text { text: text } => {
                out.push(json!({ "role": role_str(m.role), "content": text }));
            }
            Content::AssistantToolCalls { text, calls } => {
                let tool_calls: Vec<Value> = calls
                    .iter()
                    .map(|c| {
                        json!({
                            "id": c.id,
                            "type": "function",
                            "function": {
                                "name": c.name,
                                "arguments": serde_json::to_string(&c.input).unwrap_or_else(|_| "{}".into()),
                            }
                        })
                    })
                    .collect();
                let mut entry = json!({
                    "role": "assistant",
                    "tool_calls": tool_calls,
                });
                if let Some(t) = text {
                    entry["content"] = json!(t);
                } else {
                    entry["content"] = Value::Null;
                }
                out.push(entry);
            }
            Content::ToolResult {
                call_id,
                output,
                is_error: _,
            } => {
                out.push(json!({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": output,
                }));
            }
            Content::Summary { text, .. } => {
                let body = format!("{}{}", super::SUMMARY_WIRE_PREFIX, text);
                out.push(json!({ "role": "user", "content": body }));
            }
            // codex-oauth-only; nothing to send to the chat-completions API.
            Content::Reasoning(_) => {}
        }
    }
    out
}

fn tools_to_openai(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "function": {
                    "name": t.name,
                    "description": t.description,
                    "parameters": t.schema,
                }
            })
        })
        .collect()
}

#[async_trait]
impl LlmProvider for OpenAi {
    fn name(&self) -> &str {
        "openai"
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
            "messages": messages_to_openai(system, messages),
            "stream": true,
            "stream_options": { "include_usage": true },
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools_to_openai(tools));
        }

        let url = format!("{}/chat/completions", self.base_url);
        let resp = self
            .client
            .post(&url)
            .bearer_auth(&self.api_key)
            .json(&body)
            .send()
            .await
            .context("openai POST /chat/completions")?;
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
            return Err(anyhow!("openai {code}: {body}"));
        }

        // Stream SSE events. Each `data: ...` line is a JSON chunk;
        // `data: [DONE]` terminates.
        let mut stream = resp.bytes_stream().eventsource();

        let mut assistant_text = String::new();
        let mut emitted_so_far = 0usize;
        // tool-call accumulators keyed by `index`
        let mut tool_calls: Vec<ToolCallAcc> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = Usage::default();

        while let Some(ev) = stream.next().await {
            let ev = ev.context("openai SSE stream")?;
            sink.progress();
            if ev.data == "[DONE]" {
                break;
            }
            let chunk: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(u) = chunk.get("usage") {
                usage.input_tokens = u
                    .get("prompt_tokens")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(usage.input_tokens);
                usage.output_tokens = u
                    .get("completion_tokens")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(usage.output_tokens);
                usage.cached_tokens = u
                    .pointer("/prompt_tokens_details/cached_tokens")
                    .and_then(|n| n.as_u64())
                    .unwrap_or(usage.cached_tokens);
            }
            let choice = match chunk
                .get("choices")
                .and_then(|c| c.as_array())
                .and_then(|a| a.first())
            {
                Some(c) => c,
                None => continue,
            };
            if let Some(reason) = choice.get("finish_reason").and_then(|v| v.as_str()) {
                stop_reason = match reason {
                    "tool_calls" => StopReason::ToolUse,
                    "length" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
            }
            let delta = match choice.get("delta") {
                Some(d) => d,
                None => continue,
            };
            if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
                if !text.is_empty() {
                    sink.delta(text);
                    assistant_text.push_str(text);
                    emitted_so_far = assistant_text.len();
                }
            }
            if let Some(calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
                for c in calls {
                    let idx = c.get("index").and_then(|n| n.as_u64()).unwrap_or(0) as usize;
                    while tool_calls.len() <= idx {
                        tool_calls.push(ToolCallAcc::default());
                    }
                    let acc = &mut tool_calls[idx];
                    if let Some(id) = c.get("id").and_then(|v| v.as_str()) {
                        acc.id.push_str(id);
                    }
                    if let Some(name) = c.pointer("/function/name").and_then(|v| v.as_str()) {
                        acc.name.push_str(name);
                    }
                    if let Some(args) = c.pointer("/function/arguments").and_then(|v| v.as_str()) {
                        acc.args.push_str(args);
                    }
                }
            }
        }

        let calls: Vec<ToolCall> = tool_calls
            .into_iter()
            .filter(|a| !a.name.is_empty())
            .map(|a| {
                let input = if a.args.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str::<Value>(&a.args).unwrap_or_else(|_| json!({}))
                };
                ToolCall {
                    id: if a.id.is_empty() {
                        format!("tool_{}", short_hash(&a.name))
                    } else {
                        a.id
                    },
                    name: a.name,
                    input,
                }
            })
            .collect();

        Ok(ProviderTurn {
            text: if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
            tool_calls: calls,
            stop_reason,
            usage,
            reasoning_items: Vec::new(),
        })
    }
}

#[derive(Default)]
struct ToolCallAcc {
    id: String,
    name: String,
    args: String,
}

fn short_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())[..8].to_string()
}
