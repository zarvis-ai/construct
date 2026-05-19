//! Ollama `/api/chat` — NDJSON streaming, optional tool-calling on
//! capable models. Default host `http://localhost:11434`; override with
//! `OLLAMA_HOST`.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use futures::StreamExt;
use serde_json::{json, Value};

pub struct Ollama {
    client: reqwest::Client,
    base_url: String,
}

impl Ollama {
    pub fn from_env() -> Result<Self> {
        let base_url = std::env::var("OLLAMA_HOST")
            .unwrap_or_else(|_| "http://localhost:11434".to_string())
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: reqwest::Client::builder()
                .build()
                .context("build reqwest client")?,
            base_url,
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

fn messages_to_ollama(system: &str, messages: &[Message]) -> Vec<Value> {
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
                let tc: Vec<Value> = calls
                    .iter()
                    .map(|c| {
                        json!({
                            "function": {
                                "name": c.name,
                                "arguments": c.input,
                            }
                        })
                    })
                    .collect();
                let mut entry = json!({
                    "role": "assistant",
                    "content": text.clone().unwrap_or_default(),
                });
                if !tc.is_empty() {
                    entry["tool_calls"] = Value::Array(tc);
                }
                out.push(entry);
            }
            Content::ToolResult {
                call_id: _,
                output,
                is_error: _,
            } => {
                out.push(json!({
                    "role": "tool",
                    "content": output,
                }));
            }
            Content::Summary { text, .. } => {
                let body = format!("{}{}", super::SUMMARY_WIRE_PREFIX, text);
                out.push(json!({ "role": "user", "content": body }));
            }
        }
    }
    out
}

fn tools_to_ollama(tools: &[ToolSpec]) -> Vec<Value> {
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
impl LlmProvider for Ollama {
    fn name(&self) -> &str {
        "ollama"
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
            "stream": true,
            "messages": messages_to_ollama(system, messages),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(tools_to_ollama(tools));
        }

        let url = format!("{}/api/chat", self.base_url);
        let resp = self
            .client
            .post(&url)
            .json(&body)
            .send()
            .await
            .context("ollama POST /api/chat")?;
        if !resp.status().is_success() {
            let code = resp.status();
            let body = resp.text().await.unwrap_or_default();
            // Ollama returns 4xx for context overflow but body shape
            // varies by model. Run the parser; it only matches when
            // the body is actually overflow-shaped.
            if code.is_client_error() {
                if let Some(extracted) = super::parse_overflow(&body) {
                    return Err(anyhow::Error::new(super::ContextOverflow {
                        extracted,
                        raw: body,
                    }));
                }
            }
            return Err(anyhow!("ollama {code}: {body}"));
        }

        // NDJSON: one JSON object per line. `done: true` terminates.
        let mut stream = resp.bytes_stream();
        let mut buf: Vec<u8> = Vec::new();

        let mut assistant_text = String::new();
        let mut emitted_so_far = 0usize;
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = Usage::default();

        while let Some(chunk) = stream.next().await {
            let chunk = chunk.context("ollama NDJSON stream")?;
            buf.extend_from_slice(&chunk);
            // Process complete lines.
            while let Some(nl) = buf.iter().position(|b| *b == b'\n') {
                let line: Vec<u8> = buf.drain(..=nl).collect();
                let trimmed = match std::str::from_utf8(&line) {
                    Ok(s) => s.trim(),
                    Err(_) => continue,
                };
                if trimmed.is_empty() {
                    continue;
                }
                let v: Value = match serde_json::from_str(trimmed) {
                    Ok(v) => v,
                    Err(_) => continue,
                };
                if let Some(msg) = v.get("message") {
                    if let Some(t) = msg.get("content").and_then(|s| s.as_str()) {
                        if !t.is_empty() {
                            sink.delta(t);
                            assistant_text.push_str(t);
                            emitted_so_far = assistant_text.len();
                        }
                    }
                    if let Some(calls) = msg.get("tool_calls").and_then(|a| a.as_array()) {
                        for c in calls {
                            let name = c
                                .pointer("/function/name")
                                .and_then(|s| s.as_str())
                                .unwrap_or("")
                                .to_string();
                            let args = c
                                .pointer("/function/arguments")
                                .cloned()
                                .unwrap_or_else(|| json!({}));
                            if !name.is_empty() {
                                tool_calls.push(ToolCall {
                                    id: format!("tool_{}", tool_calls.len()),
                                    name,
                                    input: args,
                                });
                            }
                        }
                    }
                }
                if v.get("done").and_then(|b| b.as_bool()).unwrap_or(false) {
                    if let Some(n) = v.get("prompt_eval_count").and_then(|n| n.as_u64()) {
                        usage.input_tokens = n;
                    }
                    if let Some(n) = v.get("eval_count").and_then(|n| n.as_u64()) {
                        usage.output_tokens = n;
                    }
                    if !tool_calls.is_empty() {
                        stop_reason = StopReason::ToolUse;
                    }
                }
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
        })
    }
}
