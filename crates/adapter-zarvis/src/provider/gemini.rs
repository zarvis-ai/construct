//! Google Gemini `:streamGenerateContent` (v1beta) with SSE streaming +
//! function calling.
//!
//! Gemini's native API differs from OpenAI/Anthropic: a turn is a `contents`
//! entry with `parts`, roles are `user`/`model` (no `assistant`/`system`/
//! `tool`), tools are `functionDeclarations`, and a tool result is sent back
//! as a `functionResponse` part inside a `user` turn. Streamed `functionCall`
//! parts arrive complete (their `args` is already a JSON object), so — unlike
//! OpenAI/Anthropic — there is no partial-JSON to accumulate.
//!
//! Auth is the `x-goog-api-key` header. The system prompt rides on the
//! top-level `systemInstruction` field. Point `GEMINI_BASE_URL` at a
//! compatible gateway to override the endpoint.

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

pub struct Gemini {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl Gemini {
    pub fn from_env() -> Result<Self> {
        let api_key = std::env::var("GEMINI_API_KEY")
            .or_else(|_| std::env::var("GOOGLE_API_KEY"))
            .map_err(|_| anyhow!("GEMINI_API_KEY (or GOOGLE_API_KEY) not set"))?;
        let base_url = std::env::var("GEMINI_BASE_URL")
            .unwrap_or_else(|_| "https://generativelanguage.googleapis.com/v1beta".to_string())
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

/// Translate the conversation into Gemini `contents`. Gemini's
/// `functionResponse` needs the function *name*, but a `ToolResult` only
/// carries the `call_id` — so we thread a `call_id → name` map built from the
/// preceding `AssistantToolCalls` turns and look the name up when emitting the
/// response part.
fn messages_to_gemini(messages: &[Message]) -> Vec<Value> {
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    let mut call_names: HashMap<String, String> = HashMap::new();
    for m in messages {
        match (m.role, &m.content) {
            (Role::System, _) => {} // attached as top-level `systemInstruction`
            (_, Content::Text { text }) => {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "model",
                    Role::Tool => "user", // functionResponse parts live in user turns
                    Role::System => unreachable!(),
                };
                out.push(json!({ "role": role, "parts": [{ "text": text }] }));
            }
            (_, Content::AssistantToolCalls { text, calls }) => {
                let mut parts: Vec<Value> = Vec::with_capacity(calls.len() + 1);
                if let Some(t) = text {
                    if !t.is_empty() {
                        parts.push(json!({ "text": t }));
                    }
                }
                for c in calls {
                    call_names.insert(c.id.clone(), c.name.clone());
                    let mut fc = json!({ "name": c.name, "args": c.input });
                    if !c.id.is_empty() {
                        // Gemini 3 correlates calls↔responses by id; echo it
                        // back so multi-call turns map correctly.
                        fc["id"] = json!(c.id);
                    }
                    parts.push(json!({ "functionCall": fc }));
                }
                out.push(json!({ "role": "model", "parts": parts }));
            }
            (
                _,
                Content::ToolResult {
                    call_id,
                    output,
                    is_error,
                },
            ) => {
                let name = call_names
                    .get(call_id)
                    .cloned()
                    .unwrap_or_else(|| call_id.clone());
                // `response` must be a JSON object; wrap the tool's string
                // output and flag errors so the model can react.
                let response = if *is_error {
                    json!({ "error": output })
                } else {
                    json!({ "output": output })
                };
                let mut fr = json!({ "name": name, "response": response });
                if !call_id.is_empty() {
                    fr["id"] = json!(call_id);
                }
                out.push(json!({ "role": "user", "parts": [{ "functionResponse": fr }] }));
            }
            (_, Content::Summary { text, .. }) => {
                let body = format!("{}{}", super::SUMMARY_WIRE_PREFIX, text);
                out.push(json!({ "role": "user", "parts": [{ "text": body }] }));
            }
            // codex-oauth-only; nothing to send to the Gemini API.
            (_, Content::Reasoning(_)) => {}
        }
    }
    out
}

fn tools_to_gemini(tools: &[ToolSpec]) -> Value {
    let decls: Vec<Value> = tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.schema,
            })
        })
        .collect();
    json!([{ "functionDeclarations": decls }])
}

#[async_trait]
impl LlmProvider for Gemini {
    fn name(&self) -> &str {
        "gemini"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let mut body = json!({ "contents": messages_to_gemini(messages) });
        if !system.is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
        }
        if !tools.is_empty() {
            body["tools"] = tools_to_gemini(tools);
        }

        // The model may arrive bare (`gemini-2.5-pro`) or fully qualified
        // (`models/gemini-2.5-pro`); normalize so we never emit
        // `models/models/...`.
        let model_path = model.strip_prefix("models/").unwrap_or(model);
        let url = format!(
            "{}/models/{}:streamGenerateContent?alt=sse",
            self.base_url, model_path
        );
        let resp = self
            .client
            .post(&url)
            .header("x-goog-api-key", &self.api_key)
            .json(&body)
            .send()
            .await
            .context("gemini POST :streamGenerateContent")?;
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
            return Err(anyhow!("gemini {code}: {body}"));
        }

        let mut stream = resp.bytes_stream().eventsource();

        // Each SSE `data:` line is a full GenerateContentResponse chunk:
        //   { candidates: [{ content: { parts: [...] }, finishReason }],
        //     usageMetadata: {...} }
        // Text parts stream as deltas; functionCall parts arrive complete.
        let mut assistant_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish = StopReason::EndTurn;
        let mut usage = Usage::default();

        while let Some(ev) = stream.next().await {
            let ev = ev.context("gemini SSE stream")?;
            sink.progress();
            if ev.data.is_empty() {
                continue;
            }
            let v: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            if let Some(parts) = v
                .pointer("/candidates/0/content/parts")
                .and_then(|p| p.as_array())
            {
                for part in parts {
                    if let Some(fc) = part.get("functionCall") {
                        let name = fc
                            .get("name")
                            .and_then(|s| s.as_str())
                            .unwrap_or("")
                            .to_string();
                        let input = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                        // Gemini 3 supplies an id; older models don't, so
                        // synthesize a unique one to keep call↔result
                        // correlation working through the agent loop.
                        let id = fc
                            .get("id")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("call_{}", tool_calls.len()));
                        tool_calls.push(ToolCall { id, name, input });
                    } else if let Some(t) = part.get("text").and_then(|s| s.as_str()) {
                        if t.is_empty() {
                            continue;
                        }
                        // Gemini 2.5+ "thinking" parts carry `thought: true`;
                        // route them to the reasoning channel like Anthropic's
                        // thinking deltas.
                        if part
                            .get("thought")
                            .and_then(|b| b.as_bool())
                            .unwrap_or(false)
                        {
                            sink.reasoning_delta(t);
                        } else {
                            sink.delta(t);
                            assistant_text.push_str(t);
                        }
                    }
                }
            }
            if let Some(reason) = v
                .pointer("/candidates/0/finishReason")
                .and_then(|s| s.as_str())
            {
                // Gemini reports STOP even when the turn ends in a function
                // call, so tool-use is inferred from the collected calls
                // below rather than from finishReason.
                finish = match reason {
                    "MAX_TOKENS" => StopReason::MaxTokens,
                    _ => StopReason::EndTurn,
                };
            }
            if let Some(u) = v.get("usageMetadata") {
                if let Some(n) = u.get("promptTokenCount").and_then(|n| n.as_u64()) {
                    usage.input_tokens = n;
                }
                if let Some(n) = u.get("candidatesTokenCount").and_then(|n| n.as_u64()) {
                    usage.output_tokens = n;
                }
                if let Some(n) = u.get("cachedContentTokenCount").and_then(|n| n.as_u64()) {
                    usage.cached_tokens = n;
                }
            }
        }

        let stop_reason = if tool_calls.is_empty() {
            finish
        } else {
            StopReason::ToolUse
        };

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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_roles_and_tool_round_trip() {
        let messages = vec![
            Message {
                role: Role::User,
                content: Content::Text {
                    text: "hi".into(),
                },
            },
            Message {
                role: Role::Assistant,
                content: Content::AssistantToolCalls {
                    text: None,
                    calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "shell".into(),
                        input: json!({ "command": "ls" }),
                    }],
                },
            },
            Message {
                role: Role::Tool,
                content: Content::ToolResult {
                    call_id: "c1".into(),
                    output: "file.txt".into(),
                    is_error: false,
                },
            },
        ];
        let contents = messages_to_gemini(&messages);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "hi");
        // Assistant tool call → model turn with functionCall.
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["name"], "shell");
        assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "c1");
        // Tool result → user turn whose functionResponse carries the
        // function name resolved from the earlier call.
        assert_eq!(contents[2]["role"], "user");
        let fr = &contents[2]["parts"][0]["functionResponse"];
        assert_eq!(fr["name"], "shell");
        assert_eq!(fr["id"], "c1");
        assert_eq!(fr["response"]["output"], "file.txt");
    }

    #[test]
    fn tool_error_result_is_flagged() {
        let messages = vec![
            Message {
                role: Role::Assistant,
                content: Content::AssistantToolCalls {
                    text: None,
                    calls: vec![ToolCall {
                        id: "c1".into(),
                        name: "shell".into(),
                        input: json!({}),
                    }],
                },
            },
            Message {
                role: Role::Tool,
                content: Content::ToolResult {
                    call_id: "c1".into(),
                    output: "boom".into(),
                    is_error: true,
                },
            },
        ];
        let contents = messages_to_gemini(&messages);
        assert_eq!(
            contents[1]["parts"][0]["functionResponse"]["response"]["error"],
            "boom"
        );
    }

    #[test]
    fn tools_use_function_declarations() {
        let tools = vec![ToolSpec {
            name: "shell".into(),
            description: "run a command".into(),
            schema: json!({ "type": "object" }),
        }];
        let v = tools_to_gemini(&tools);
        assert_eq!(v[0]["functionDeclarations"][0]["name"], "shell");
        assert_eq!(
            v[0]["functionDeclarations"][0]["parameters"]["type"],
            "object"
        );
    }
}
