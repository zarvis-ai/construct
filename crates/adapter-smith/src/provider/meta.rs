//! Meta Model API `/v1/responses` provider.
//!
//! Meta exposes Muse Spark through an OpenAI-compatible Responses API, not
//! Chat Completions. This adapter keeps the billing/auth path explicit as
//! `meta:<model>` and translates Smith's normalized conversation and tools
//! into Responses input items.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;

const DEFAULT_BASE_URL: &str = "https://api.meta.ai/v1";

pub struct Meta {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Meta {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("META_API_KEY")
            .or_else(|_| std::env::var("MODEL_API_KEY"))
            .map_err(|_| anyhow!("META_API_KEY (or MODEL_API_KEY) not set"))?;
        Self::with_config(std::env::var("META_BASE_URL").ok(), api_key)
    }

    pub fn with_config(base_url: Option<String>, api_key: String) -> Result<Self> {
        Ok(Self {
            client: reqwest::Client::builder()
                .build()
                .context("build reqwest client")?,
            base_url: base_url
                .unwrap_or_else(|| DEFAULT_BASE_URL.to_string())
                .trim_end_matches('/')
                .to_string(),
            api_key,
        })
    }
}

fn message_to_input_items(message: &Message) -> Vec<Value> {
    match &message.content {
        Content::Text { text } => {
            let (role, kind) = match message.role {
                Role::System => ("system", "input_text"),
                Role::User | Role::Tool => ("user", "input_text"),
                Role::Assistant => ("assistant", "output_text"),
            };
            vec![json!({
                "type": "message",
                "role": role,
                "content": [{ "type": kind, "text": text }],
            })]
        }
        Content::AssistantToolCalls { text, calls } => {
            let mut items = Vec::with_capacity(calls.len() + 1);
            if let Some(text) = text.as_deref().filter(|text| !text.is_empty()) {
                items.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": text }],
                }));
            }
            items.extend(calls.iter().map(|call| {
                json!({
                    "type": "function_call",
                    "call_id": call.id,
                    "name": call.name,
                    "arguments": serde_json::to_string(&call.input)
                        .unwrap_or_else(|_| "{}".to_string()),
                })
            }));
            items
        }
        Content::ToolResult {
            call_id, output, ..
        } => vec![json!({
            "type": "function_call_output",
            "call_id": call_id,
            "output": output,
        })],
        Content::Summary { text, .. } => vec![json!({
            "type": "message",
            "role": "user",
            "content": [{
                "type": "input_text",
                "text": format!("{}{}", super::SUMMARY_WIRE_PREFIX, text),
            }],
        })],
        // Meta returns reasoning items, but unlike the Codex OAuth backend it
        // does not require encrypted reasoning state to be replayed.
        Content::Reasoning(_) => Vec::new(),
    }
}

fn build_body(model: &str, system: &str, messages: &[Message], tools: &[ToolSpec]) -> Value {
    let input: Vec<Value> = messages.iter().flat_map(message_to_input_items).collect();
    let mut body = json!({
        "model": model,
        "input": input,
        "stream": true,
        // Smith owns and replays conversation history locally; do not ask the
        // provider to retain an additional server-side copy.
        "store": false,
    });
    if !system.is_empty() {
        body["instructions"] = json!(system);
    }
    if !tools.is_empty() {
        body["tools"] = Value::Array(
            tools
                .iter()
                .map(|tool| {
                    json!({
                        "type": "function",
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.schema,
                    })
                })
                .collect(),
        );
        body["parallel_tool_calls"] = Value::Bool(true);
    }
    body
}

#[derive(Default)]
struct FunctionCall {
    call_id: String,
    name: String,
    arguments: String,
}

#[async_trait]
impl LlmProvider for Meta {
    fn name(&self) -> &str {
        "meta"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let response = self
            .client
            .post(format!("{}/responses", self.base_url))
            .bearer_auth(&self.api_key)
            .header("Accept", "text/event-stream")
            .json(&build_body(model, system, messages, tools))
            .send()
            .await
            .context("meta POST /v1/responses")?;
        let status = response.status();
        if !status.is_success() {
            let raw = response.text().await.unwrap_or_default();
            if status.as_u16() == 400 {
                if let Some(extracted) = super::parse_overflow(&raw) {
                    return Err(anyhow::Error::new(super::ContextOverflow {
                        extracted,
                        raw,
                    }));
                }
            }
            return Err(anyhow!("meta {status}: {raw}"));
        }

        let mut stream = response.bytes_stream().eventsource();
        let mut text = String::new();
        let mut calls: HashMap<String, FunctionCall> = HashMap::new();
        let mut call_order = Vec::new();
        let mut usage = Usage::default();
        let mut stop_reason = StopReason::EndTurn;
        let mut terminal_event_seen = false;

        while let Some(event) = stream.next().await {
            let event = event.context("meta SSE stream")?;
            sink.progress();
            if event.data == "[DONE]" {
                break;
            }
            let chunk: Value = match serde_json::from_str(&event.data) {
                Ok(value) => value,
                Err(_) => continue,
            };
            let kind = if event.event.is_empty() {
                chunk
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
            } else {
                event.event.as_str()
            };
            match kind {
                "response.output_text.delta" => {
                    if let Some(delta) = chunk.get("delta").and_then(Value::as_str) {
                        sink.delta(delta);
                        text.push_str(delta);
                    }
                }
                "response.output_item.added" => {
                    let Some(item) = chunk.get("item") else {
                        continue;
                    };
                    if item.get("type").and_then(Value::as_str) != Some("function_call") {
                        continue;
                    }
                    let item_id = item.get("id").and_then(Value::as_str).unwrap_or_default();
                    if item_id.is_empty() {
                        continue;
                    }
                    call_order.push(item_id.to_string());
                    calls.insert(
                        item_id.to_string(),
                        FunctionCall {
                            call_id: item
                                .get("call_id")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            name: item
                                .get("name")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                            arguments: item
                                .get("arguments")
                                .and_then(Value::as_str)
                                .unwrap_or_default()
                                .to_string(),
                        },
                    );
                }
                "response.function_call_arguments.delta" => {
                    if let Some(call) = chunk
                        .get("item_id")
                        .and_then(Value::as_str)
                        .and_then(|id| calls.get_mut(id))
                    {
                        call.arguments.push_str(
                            chunk
                                .get("delta")
                                .and_then(Value::as_str)
                                .unwrap_or_default(),
                        );
                    }
                }
                "response.function_call_arguments.done" => {
                    if let Some(call) = chunk
                        .get("item_id")
                        .and_then(Value::as_str)
                        .and_then(|id| calls.get_mut(id))
                    {
                        if let Some(arguments) = chunk.get("arguments").and_then(Value::as_str) {
                            call.arguments = arguments.to_string();
                        }
                    }
                }
                "response.completed" | "response.incomplete" | "response.failed" => {
                    terminal_event_seen = true;
                    if let Some(provider_usage) = chunk.pointer("/response/usage") {
                        usage.input_tokens = provider_usage
                            .get("input_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        usage.output_tokens = provider_usage
                            .get("output_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                        usage.cached_tokens = provider_usage
                            .pointer("/input_tokens_details/cached_tokens")
                            .and_then(Value::as_u64)
                            .unwrap_or_default();
                    }
                    if kind == "response.failed" {
                        return Err(anyhow!(
                            "meta response failed: {}",
                            chunk
                                .pointer("/response/error/message")
                                .and_then(Value::as_str)
                                .unwrap_or("unknown error")
                        ));
                    }
                    if chunk
                        .pointer("/response/incomplete_details/reason")
                        .and_then(Value::as_str)
                        == Some("max_output_tokens")
                    {
                        stop_reason = StopReason::MaxTokens;
                    }
                    break;
                }
                _ => {}
            }
        }
        if !terminal_event_seen {
            return Err(anyhow!("meta stream ended before response.completed"));
        }

        let tool_calls: Vec<ToolCall> = call_order
            .iter()
            .filter_map(|id| calls.remove(id))
            .filter(|call| !call.name.is_empty())
            .map(|call| ToolCall {
                id: call.call_id,
                name: call.name,
                input: serde_json::from_str(&call.arguments).unwrap_or_else(|_| json!({})),
            })
            .collect();
        if !tool_calls.is_empty() {
            stop_reason = StopReason::ToolUse;
        }
        Ok(ProviderTurn {
            text: (!text.is_empty()).then_some(text),
            tool_calls,
            stop_reason,
            usage,
            reasoning_items: Vec::new(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_uses_responses_message_and_tool_shapes() {
        let messages = vec![
            Message {
                role: Role::User,
                content: Content::Text { text: "add".into() },
            },
            Message {
                role: Role::Assistant,
                content: Content::AssistantToolCalls {
                    text: None,
                    calls: vec![ToolCall {
                        id: "call_1".into(),
                        name: "add".into(),
                        input: json!({"a": 2, "b": 3}),
                    }],
                },
            },
            Message {
                role: Role::Tool,
                content: Content::ToolResult {
                    call_id: "call_1".into(),
                    output: "5".into(),
                    is_error: false,
                },
            },
        ];
        let body = build_body(
            "muse-spark-1.1",
            "system",
            &messages,
            &[ToolSpec {
                name: "add".into(),
                description: "Add".into(),
                schema: json!({"type": "object"}),
            }],
        );
        assert_eq!(body["instructions"], "system");
        assert_eq!(body["store"], false);
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][2]["type"], "function_call_output");
        assert_eq!(body["tools"][0]["name"], "add");
    }
}
