//! Antigravity (Google's agy) subscription OAuth provider for smith.
//!
//! Reads the *exact* credential the `agy` CLI uses (macOS keychain service
//! "gemini", account "antigravity", or the Go keyring fallback) and calls
//! the internal Cloud Code / Code Assist backend that agy talks to
//! (`daily-cloudcode-pa.googleapis.com/...:streamGenerateContent`).
//!
//! This is the subscription-backed path, distinct from the public Gemini API
//! (`gemini:` / `GEMINI_API_KEY`). Model strings use the explicit prefix
//! `antigravity-oauth:...` (e.g. `antigravity-oauth:gemini-2.5-pro`, or short
//! aliases once we map them).
//!
//! Smith owns the agent loop, tools, approvals, and persistence — we only
//! borrow the OAuth token and the wire format from the agy client.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde_json::{json, Value};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

/// The internal Code Assist / Cloud Code endpoint agy uses for streaming.
const STREAM_URL: &str =
    "https://daily-cloudcode-pa.googleapis.com/v1internal:streamGenerateContent";

/// Keychain service/account the agy binary (and its Go keyring) writes to.
const KEYCHAIN_SERVICE: &str = "gemini";
const KEYCHAIN_ACCOUNT: &str = "antigravity";

pub struct AntigravityOauth {
    http: reqwest::Client,
}

impl AntigravityOauth {
    pub fn from_env() -> Result<Self> {
        // We build the client here; actual token is fetched per-turn so that
        // a long-lived smith session can survive a token refresh done by agy.
        let http = reqwest::Client::builder()
            .user_agent("agentd-smith/antigravity-oauth")
            .build()
            .context("build reqwest client")?;
        Ok(Self { http })
    }

    /// Read the credential exactly the way agy does (keychain item written by
    /// Go's keyring on macOS). Returns the access token.
    fn load_access_token(&self) -> Result<String> {
        // Try the macOS keychain first (what `agy` writes under the hood via
        // github.com/zalando/go-keyring on darwin).
        if let Ok(raw) = keychain_read() {
            if let Ok(tok) = extract_token_from_go_keyring(&raw) {
                return Ok(tok);
            }
        }

        // Fallback: some installations may have a plain file (mirrors how
        // older gemini/agy code sometimes persisted). We look in the same
        // places the agy binary would.
        if let Ok(tok) = try_file_fallback() {
            return Ok(tok);
        }

        Err(anyhow!(
            "could not find antigravity/agy OAuth credentials.\n\
             Expected macOS keychain item service=\"{}\" account=\"{}\".\n\
             Run `agy` once and log in with your Google account, then retry.",
            KEYCHAIN_SERVICE,
            KEYCHAIN_ACCOUNT
        ))
    }
}

fn keychain_read() -> Result<String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-a", KEYCHAIN_ACCOUNT, "-w"])
        .output()
        .context("run security find-generic-password")?;
    if !out.status.success() {
        anyhow::bail!("keychain item {}/{} not found or unreadable", KEYCHAIN_SERVICE, KEYCHAIN_ACCOUNT);
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// The Go keyring library (used by agy) base64-encodes a JSON blob that
/// contains { "token": { "access_token": "...", ... }, "auth_method": "consumer" }.
fn extract_token_from_go_keyring(raw: &str) -> Result<String> {
    let s = raw.trim();
    let b64 = s.strip_prefix("go-keyring-base64:").unwrap_or(s);
    let bytes = base64::Engine::decode(
        &base64::engine::general_purpose::STANDARD,
        b64,
    )
    .context("base64 decode keyring blob")?;
    let v: Value = serde_json::from_slice(&bytes).context("parse keyring JSON")?;
    let tok = v
        .pointer("/token/access_token")
        .and_then(|x| x.as_str())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("keyring blob missing token.access_token"))?;
    Ok(tok.to_string())
}

fn try_file_fallback() -> Result<String> {
    // Mirror locations agy/Cloud Code have used historically.
    let home = std::env::var("HOME").unwrap_or_default();
    let candidates = [
        format!("{}/.gemini/antigravity_oauth.json", home),
        format!("{}/.config/antigravity/credentials.json", home),
        format!("{}/.antigravity/auth.json", home),
    ];
    for p in candidates {
        if let Ok(s) = std::fs::read_to_string(&p) {
            if let Ok(v) = serde_json::from_str::<Value>(&s) {
                if let Some(t) = v.get("access_token").and_then(|x| x.as_str()).filter(|s| !s.is_empty()) {
                    return Ok(t.to_string());
                }
                if let Some(t) = v.pointer("/token/access_token").and_then(|x| x.as_str()) {
                    if !t.is_empty() { return Ok(t.to_string()); }
                }
            }
        }
    }
    Err(anyhow!("no file fallback found"))
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

#[async_trait]
impl LlmProvider for AntigravityOauth {
    fn name(&self) -> &str {
        "antigravity-oauth"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let access_token = self.load_access_token()?;

        // Build the request body in the shape the internal
        // streamGenerateContent endpoint expects. This is intentionally
        // close to the public Gemini shape (contents + tools) because the
        // internal backend re-uses a lot of the same protos, but we may need
        // to adjust fields once we have a live trace.
        let contents = messages_to_antigravity(messages);
        let mut body = json!({
            "model": model,
            "contents": contents,
        });
        if !system.trim().is_empty() {
            body["systemInstruction"] = json!({ "parts": [{ "text": system }] });
        }
        if !tools.is_empty() {
            body["tools"] = json!([{ "functionDeclarations": tools_to_antigravity(tools) }]);
        }

        let resp = self
            .http
            .post(STREAM_URL)
            .query(&[("alt", "sse")])
            .bearer_auth(&access_token)
            .json(&body)
            .send()
            .await
            .context("POST streamGenerateContent")?;

        if !resp.status().is_success() {
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            anyhow::bail!("antigravity backend error {status}: {text}");
        }

        let mut stream = resp.bytes_stream().eventsource();

        let mut assistant_text = String::new();
        let mut tool_calls: Vec<ToolCall> = Vec::new();
        let mut finish = StopReason::EndTurn;
        let mut usage = Usage::default();

        while let Some(ev) = stream.next().await {
            let ev = ev.context("antigravity SSE")?;
            sink.progress();
            if ev.data.is_empty() || ev.data == "[DONE]" {
                continue;
            }
            let v: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(_) => continue,
            };

            // The internal endpoint tends to wrap the same candidate/part
            // structure the public Gemini API uses.
            if let Some(parts) = v
                .pointer("/candidates/0/content/parts")
                .and_then(|p| p.as_array())
            {
                for part in parts {
                    if let Some(fc) = part.get("functionCall") {
                        let name = fc.get("name").and_then(|s| s.as_str()).unwrap_or("").to_string();
                        let input = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                        let id = fc
                            .get("id")
                            .and_then(|s| s.as_str())
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| format!("call_{}", tool_calls.len()));
                        tool_calls.push(ToolCall { id, name, input });
                    } else if let Some(t) = part.get("text").and_then(|s| s.as_str()) {
                        if !t.is_empty() {
                            sink.delta(t);
                            assistant_text.push_str(t);
                        }
                    }
                }
            }
            if let Some(reason) = v.pointer("/candidates/0/finishReason").and_then(|s| s.as_str()) {
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
            }
        }

        let stop_reason = if tool_calls.is_empty() { finish } else { StopReason::ToolUse };

        Ok(ProviderTurn {
            text: if assistant_text.is_empty() { None } else { Some(assistant_text) },
            tool_calls,
            stop_reason,
            usage,
            reasoning_items: Vec::new(),
        })
    }
}

fn messages_to_antigravity(messages: &[Message]) -> Vec<Value> {
    let mut out = Vec::new();
    for m in messages {
        match (m.role, &m.content) {
            (Role::System, _) => {}
            (_, Content::Text { text }) => {
                let role = match m.role {
                    Role::User => "user",
                    Role::Assistant => "model",
                    Role::Tool => "user",
                    Role::System => unreachable!(),
                };
                out.push(json!({ "role": role, "parts": [{ "text": text }] }));
            }
            (_, Content::AssistantToolCalls { text, calls }) => {
                let mut parts: Vec<Value> = Vec::new();
                if let Some(t) = text {
                    if !t.is_empty() {
                        parts.push(json!({ "text": t }));
                    }
                }
                for c in calls {
                    parts.push(json!({ "functionCall": { "name": c.name, "args": c.input, "id": c.id } }));
                }
                out.push(json!({ "role": "model", "parts": parts }));
            }
            (_, Content::ToolResult { call_id, output, is_error }) => {
                // The internal surface expects functionResponse inside a user turn.
                out.push(json!({
                    "role": "user",
                    "parts": [{
                        "functionResponse": {
                            "id": call_id,
                            "name": "", // filled by caller if needed; many impls ignore
                            "response": { "output": output, "error": is_error }
                        }
                    }]
                }));
            }
            _ => {}
        }
    }
    out
}

fn tools_to_antigravity(tools: &[ToolSpec]) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "name": t.name,
                "description": t.description,
                "parameters": t.schema
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_go_keyring_blob() {
        let blob = "go-keyring-base64:eyJ0b2tlbiI6eyJhY2Nlc3NfdG9rZW4iOiJ5YTI5LmZvbyJ9fQ==";
        let tok = extract_token_from_go_keyring(blob).unwrap();
        assert_eq!(tok, "ya29.foo");
    }
}
