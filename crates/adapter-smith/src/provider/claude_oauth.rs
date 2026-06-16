//! Claude Code OAuth provider.
//!
//! This provider delegates authentication and model access to the installed
//! `claude` CLI instead of reading Claude credentials directly. Smith still
//! owns the agent loop: Claude Code's built-in tools are disabled, and the
//! model is asked to return a structured Smith turn containing assistant text
//! and optional Smith tool calls.

use super::{
    Content, LlmProvider, Message, ProviderTurn, Role, StopReason, TextSink, ToolCall, ToolSpec,
    Usage,
};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::process::Stdio;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::Command;

pub struct ClaudeOauth {
    command: agentd_protocol::adapter::CommandOverride,
}

impl ClaudeOauth {
    pub fn from_env() -> Result<Self> {
        Ok(Self {
            command: agentd_protocol::adapter::resolve_command_override(
                "CONSTRUCT_CLAUDE_CMD",
                "CONSTRUCT_CLAUDE_BIN",
                "claude",
            ),
        })
    }
}

#[derive(Debug, Default)]
struct CliOutput {
    result: Option<String>,
    usage: Usage,
    error: Option<String>,
}

#[derive(Debug, Deserialize)]
struct CliResult {
    #[serde(default)]
    result: Option<String>,
    #[serde(default)]
    is_error: bool,
    #[serde(default)]
    subtype: Option<String>,
    #[serde(default)]
    total_cost_usd: Option<f64>,
    #[serde(default)]
    usage: Option<CliUsage>,
}

#[derive(Debug, Deserialize)]
struct CliUsage {
    #[serde(default)]
    input_tokens: u64,
    #[serde(default)]
    output_tokens: u64,
    #[serde(default)]
    cache_read_input_tokens: u64,
}

fn role_name(role: Role) -> &'static str {
    match role {
        Role::System => "system",
        Role::User => "user",
        Role::Assistant => "assistant",
        Role::Tool => "tool",
    }
}

fn content_for_prompt(content: &Content) -> Value {
    match content {
        Content::Text { text } => json!({ "kind": "text", "text": text }),
        Content::AssistantToolCalls { text, calls } => {
            let calls: Vec<Value> = calls
                .iter()
                .map(|c| {
                    json!({
                        "id": c.id,
                        "name": c.name,
                        "input": c.input,
                    })
                })
                .collect();
            json!({ "kind": "assistant_tool_calls", "text": text, "calls": calls })
        }
        Content::ToolResult {
            call_id,
            output,
            is_error,
        } => json!({
            "kind": "tool_result",
            "call_id": call_id,
            "output": output,
            "is_error": is_error,
        }),
        Content::Summary { text, .. } => {
            json!({ "kind": "summary", "text": format!("{}{}", super::SUMMARY_WIRE_PREFIX, text) })
        }
        Content::Reasoning(item) => json!({
            "kind": "reasoning",
            "summary": &item.summary,
        }),
    }
}

fn messages_for_prompt(messages: &[Message]) -> Vec<Value> {
    messages
        .iter()
        .map(|m| {
            json!({
                "role": role_name(m.role),
                "content": content_for_prompt(&m.content),
            })
        })
        .collect()
}

fn tools_for_prompt(tools: &[ToolSpec]) -> Vec<Value> {
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

fn output_schema() -> Value {
    json!({
        "type": "object",
        "additionalProperties": false,
        "properties": {
            "text": {
                "description": "Assistant text to show the user before any tool calls, or an empty string.",
                "type": "string"
            },
            "tool_calls": {
                "description": "Smith tool calls to execute before the next model step.",
                "type": "array",
                "items": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {
                        "id": { "type": "string" },
                        "name": { "type": "string" },
                        "input": { "type": "object" }
                    },
                    "required": ["name", "input"]
                }
            },
            "stop_reason": {
                "type": "string",
                "enum": ["end_turn", "tool_use", "max_tokens"]
            }
        },
        "required": ["text", "tool_calls", "stop_reason"]
    })
}

fn provider_system_prompt(system: &str) -> String {
    format!(
        "{system}\n\n\
Smith is using Claude Code OAuth as a model provider. Do not use Claude Code's \
built-in tools. Use only the structured response protocol: return a JSON object \
matching the supplied schema. When you need a tool, put one or more entries in \
`tool_calls` using exactly one of the available Smith tool names and an input \
object matching that tool's schema. When no tool is needed, return final \
assistant text in `text` with an empty `tool_calls` array. Do not wrap the JSON \
in Markdown."
    )
}

fn build_prompt(messages: &[Message], tools: &[ToolSpec]) -> Result<String> {
    let conversation = serde_json::to_string_pretty(&messages_for_prompt(messages))
        .context("serialize smith conversation")?;
    let tools =
        serde_json::to_string_pretty(&tools_for_prompt(tools)).context("serialize smith tools")?;
    Ok(format!(
        "Continue the Smith conversation encoded below.\n\n\
Conversation JSON:\n{conversation}\n\n\
Available Smith tools JSON:\n{tools}\n\n\
Return the next Smith turn as JSON matching the required schema."
    ))
}

async fn write_prompt(mut stdin: tokio::process::ChildStdin, prompt: String) {
    let msg = json!({
        "type": "user",
        "message": {
            "role": "user",
            "content": [{ "type": "text", "text": prompt }]
        }
    });
    if let Ok(line) = serde_json::to_string(&msg) {
        let _ = stdin.write_all(line.as_bytes()).await;
        let _ = stdin.write_all(b"\n").await;
        let _ = stdin.flush().await;
    }
    let _ = stdin.shutdown().await;
}

async fn collect_stderr<R>(reader: R) -> String
where
    R: tokio::io::AsyncRead + Unpin + Send + 'static,
{
    let mut out = String::new();
    let mut lines = BufReader::new(reader).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(&line);
    }
    out
}

fn process_cli_line(line: &str, out: &mut CliOutput) {
    let Ok(v) = serde_json::from_str::<Value>(line) else {
        return;
    };
    match v.get("type").and_then(|t| t.as_str()).unwrap_or("") {
        "result" => match serde_json::from_value::<CliResult>(v) {
            Ok(r) => {
                if r.is_error {
                    out.error = Some(r.result.unwrap_or_else(|| {
                        r.subtype
                            .unwrap_or_else(|| "claude returned an error".to_string())
                    }));
                    return;
                }
                out.result = r.result;
                if let Some(usd) = r.total_cost_usd {
                    out.usage.usd = usd;
                }
                if let Some(u) = r.usage {
                    out.usage.input_tokens = u.input_tokens;
                    out.usage.output_tokens = u.output_tokens;
                    out.usage.cached_tokens = u.cache_read_input_tokens;
                }
            }
            Err(e) => {
                out.error = Some(format!("failed to parse claude result: {e}"));
            }
        },
        "error" => {
            out.error = Some(
                v.get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("claude returned an error")
                    .to_string(),
            );
        }
        _ => {}
    }
}

fn parse_protocol_turn(raw: &str) -> Result<ProviderTurn> {
    let v: Value = match serde_json::from_str(raw) {
        Ok(v) => v,
        Err(first_err) => {
            let trimmed = raw.trim();
            let Some(start) = trimmed.find('{') else {
                return Err(anyhow!("parse claude-oauth structured turn: {first_err}"));
            };
            let Some(end) = trimmed.rfind('}').map(|i| i + 1) else {
                return Err(anyhow!("parse claude-oauth structured turn: {first_err}"));
            };
            serde_json::from_str(&trimmed[start..end])
                .context("parse claude-oauth structured turn")?
        }
    };

    let text = v
        .get("text")
        .and_then(|t| t.as_str())
        .map(|s| s.to_string())
        .filter(|s| !s.trim().is_empty());
    let mut tool_calls = Vec::new();
    if let Some(calls) = v.get("tool_calls").and_then(|c| c.as_array()) {
        for (idx, call) in calls.iter().enumerate() {
            let name = call
                .get("name")
                .and_then(|n| n.as_str())
                .ok_or_else(|| anyhow!("claude-oauth tool call {idx} missing name"))?
                .to_string();
            if name.trim().is_empty() {
                return Err(anyhow!("claude-oauth tool call {idx} has empty name"));
            }
            let input = call.get("input").cloned().unwrap_or_else(|| json!({}));
            let id = call
                .get("id")
                .and_then(|id| id.as_str())
                .map(|s| s.to_string())
                .filter(|s| !s.trim().is_empty())
                .unwrap_or_else(|| format!("claude_oauth_tool_{idx}"));
            tool_calls.push(ToolCall { id, name, input });
        }
    }
    let stop_reason = match v.get("stop_reason").and_then(|s| s.as_str()) {
        Some("max_tokens") => StopReason::MaxTokens,
        Some("tool_use") => StopReason::ToolUse,
        _ if !tool_calls.is_empty() => StopReason::ToolUse,
        _ => StopReason::EndTurn,
    };
    Ok(ProviderTurn {
        text,
        tool_calls,
        stop_reason,
        usage: Usage::default(),
        reasoning_items: Vec::new(),
    })
}

#[async_trait]
impl LlmProvider for ClaudeOauth {
    fn name(&self) -> &str {
        "claude-oauth"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let prompt = build_prompt(messages, tools)?;
        let schema = serde_json::to_string(&output_schema()).context("serialize output schema")?;
        let mut args = self.command.args.clone();
        args.extend([
            "-p".to_string(),
            "--input-format".to_string(),
            "stream-json".to_string(),
            "--output-format".to_string(),
            "stream-json".to_string(),
            "--verbose".to_string(),
            "--include-partial-messages".to_string(),
            "--safe-mode".to_string(),
            "--no-session-persistence".to_string(),
            "--tools".to_string(),
            String::new(),
            "--permission-mode".to_string(),
            "dontAsk".to_string(),
            "--model".to_string(),
            model.to_string(),
            "--system-prompt".to_string(),
            provider_system_prompt(system),
            "--json-schema".to_string(),
            schema,
        ]);

        let mut command = Command::new(&self.command.bin);
        command
            .args(args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true);
        // This provider is explicitly the Claude Code OAuth path. Do not let
        // direct API-key or third-party provider env from the daemon process
        // silently change the billing/auth route of the child CLI.
        for key in [
            "ANTHROPIC_API_KEY",
            "ANTHROPIC_BASE_URL",
            "CLAUDE_CODE_USE_BEDROCK",
            "CLAUDE_CODE_USE_VERTEX",
            "CLAUDE_CODE_USE_ANTHROPIC_AWS",
            "CLAUDE_CODE_USE_FOUNDRY",
        ] {
            command.env_remove(key);
        }
        let mut child = command.spawn().map_err(|e| {
            anyhow!(agentd_protocol::adapter::missing_bin_hint(
                &self.command.argv_preview(),
                &e
            ))
        })?;
        let stdin = child.stdin.take().expect("piped");
        let stdout = child.stdout.take().expect("piped");
        let stderr = child.stderr.take().expect("piped");
        let writer = tokio::spawn(write_prompt(stdin, prompt));
        let stderr_task = tokio::spawn(collect_stderr(stderr));

        let mut cli = CliOutput::default();
        let mut lines = BufReader::new(stdout).lines();
        while let Some(line) = lines
            .next_line()
            .await
            .context("read claude-oauth stdout")?
        {
            if line.trim().is_empty() {
                continue;
            }
            sink.progress();
            process_cli_line(&line, &mut cli);
        }
        let _ = writer.await;
        let status = child.wait().await.context("wait for claude-oauth")?;
        let stderr = stderr_task.await.unwrap_or_default();
        if !status.success() {
            return Err(anyhow!(
                "claude-oauth command failed with {status}: {}",
                stderr.trim()
            ));
        }
        if let Some(err) = cli.error {
            return Err(anyhow!("claude-oauth: {err}"));
        }
        let raw = cli
            .result
            .ok_or_else(|| anyhow!("claude-oauth stream ended without a result"))?;
        let mut turn = parse_protocol_turn(&raw)?;
        if let Some(text) = turn.text.as_deref() {
            sink.delta(text);
        }
        turn.usage = cli.usage;
        Ok(turn)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_structured_text_turn() {
        let turn =
            parse_protocol_turn(r#"{"text":"done","tool_calls":[],"stop_reason":"end_turn"}"#)
                .unwrap();
        assert_eq!(turn.text.as_deref(), Some("done"));
        assert!(turn.tool_calls.is_empty());
        assert_eq!(turn.stop_reason, StopReason::EndTurn);
    }

    #[test]
    fn parses_structured_tool_turn_with_generated_id() {
        let turn = parse_protocol_turn(
            r#"{"text":"I will inspect.","tool_calls":[{"name":"shell","input":{"cmd":"pwd"}}],"stop_reason":"tool_use"}"#,
        )
        .unwrap();
        assert_eq!(turn.text.as_deref(), Some("I will inspect."));
        assert_eq!(turn.tool_calls.len(), 1);
        assert_eq!(turn.tool_calls[0].id, "claude_oauth_tool_0");
        assert_eq!(turn.tool_calls[0].name, "shell");
        assert_eq!(turn.tool_calls[0].input["cmd"], "pwd");
        assert_eq!(turn.stop_reason, StopReason::ToolUse);
    }

    #[test]
    fn extracts_json_from_markdown_fallback() {
        let turn = parse_protocol_turn(
            "```json\n{\"text\":\"ok\",\"tool_calls\":[],\"stop_reason\":\"end_turn\"}\n```",
        )
        .unwrap();
        assert_eq!(turn.text.as_deref(), Some("ok"));
    }

    #[test]
    fn cli_result_line_records_usage() {
        let mut out = CliOutput::default();
        process_cli_line(
            r#"{"type":"result","subtype":"success","result":"{\"text\":\"ok\",\"tool_calls\":[],\"stop_reason\":\"end_turn\"}","total_cost_usd":0.01,"usage":{"input_tokens":10,"output_tokens":2,"cache_read_input_tokens":4}}"#,
            &mut out,
        );
        assert!(out.error.is_none());
        assert!(out.result.unwrap().contains("\"text\":\"ok\""));
        assert_eq!(out.usage.input_tokens, 10);
        assert_eq!(out.usage.output_tokens, 2);
        assert_eq!(out.usage.cached_tokens, 4);
        assert_eq!(out.usage.usd, 0.01);
    }
}
