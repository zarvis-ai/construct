//! Agentd-control tools: thin wrappers over `construct_client::Client`.
//! Lets a smith session drive the daemon (list/spawn/send-input to
//! other sessions) using natural-language tool calls — the same surface
//! the MCP server exposes.

use super::{Tool, ToolCtx, ToolOutcome};
use construct_client::Client;
use construct_protocol::{agent_context, paths::Paths, CreateSessionParams, PtySize, ToolRisk};
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use base64::Engine;
use serde_json::{json, Value};
use std::sync::Arc;

pub(crate) async fn client(ctx: &ToolCtx) -> Result<Arc<Client>> {
    ctx.client
        .get_or_try_init(|| async {
            let socket = Paths::discover().socket();
            let c = Client::connect(&socket).await?;
            Ok::<Arc<Client>, anyhow::Error>(c)
        })
        .await
        .cloned()
}

fn need_str(input: &Value, k: &str) -> Result<String> {
    input
        .get(k)
        .and_then(|s| s.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing `{k}`"))
}

// ---------- read ----------

pub struct Context;
#[async_trait]
impl Tool for Context {
    fn name(&self) -> &str {
        agent_context::TOOL_NAME
    }
    fn description(&self) -> &str {
        agent_context::TOOL_DESCRIPTION
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "refresh": { "type": "boolean", "description": "Resend static fields and unchanged content. Pass true when earlier agentd_context results may have been compacted out of your context." },
                "skip_memory": { "type": "boolean", "description": "Omit memory file contents you already hold verbatim (e.g. you just wrote them); paths and etags are still returned." },
                "include_reference": { "type": "boolean", "description": "Include the full memory/widget policy and Markdown extension reference." }
            }
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let request = agent_context::ContextRequest::from_args(&input);
        let mut state = ctx.context_serve.lock().unwrap_or_else(|p| p.into_inner());
        let response =
            agent_context::compact_response(agent_context::build_from_env(), &request, &mut state);
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&response)?,
        })
    }
}

pub struct Whoami;
#[async_trait]
impl Tool for Whoami {
    fn name(&self) -> &str {
        "agentd_whoami"
    }
    fn description(&self) -> &str {
        "Returns the session id of the construct session this agent is running inside, \
         or null if not inside one. Use this to avoid acting on yourself."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "session_id": ctx.session_id }).to_string(),
        })
    }
}

pub struct ListSessions;
#[async_trait]
impl Tool for ListSessions {
    fn name(&self) -> &str {
        "agentd_list_sessions"
    }
    fn description(&self) -> &str {
        "List every construct session (running and finished). Each entry includes the \
         session id, harness, state, cwd, pinned flag, approval mode, last_pty_at_ms \
         (use `now - last_pty_at_ms < 600ms` as a 'is the agent currently busy?' \
         signal), and group info when applicable."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let c = client(ctx).await?;
        let sessions = c.list().await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&sessions)?,
        })
    }
}

pub struct GetSession;
#[async_trait]
impl Tool for GetSession {
    fn name(&self) -> &str {
        "agentd_get_session"
    }
    fn description(&self) -> &str {
        "Fetch the full summary + structured transcript for one session."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "session_id": { "type": "string" } }, "required": ["session_id"] })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let c = client(ctx).await?;
        let det = c.get(&sid).await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&det)?,
        })
    }
}

pub struct GetTranscript;
#[async_trait]
impl Tool for GetTranscript {
    fn name(&self) -> &str {
        "agentd_get_transcript"
    }
    fn description(&self) -> &str {
        "Fetch a slice of the session's structured event log. `from` is a 1-based seq; \
         `limit` bounds the count."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "from":       { "type": "integer", "minimum": 0 },
                "limit":      { "type": "integer", "minimum": 1 }
            },
            "required": ["session_id"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let from = input.get("from").and_then(|n| n.as_u64()).unwrap_or(0);
        let limit = input
            .get("limit")
            .and_then(|n| n.as_u64())
            .map(|n| n as usize);
        let c = client(ctx).await?;
        let res = c.transcript(&sid, from, limit).await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&res)?,
        })
    }
}

pub struct GetOutput;
#[async_trait]
impl Tool for GetOutput {
    fn name(&self) -> &str {
        "agentd_get_output"
    }
    fn description(&self) -> &str {
        "Fetch the session's recent PTY scrollback as text (UTF-8 lossy). Use for \
         reading what's on the screen of a PTY-backed session."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "session_id": { "type": "string" } }, "required": ["session_id"] })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let c = client(ctx).await?;
        let snap = c.pty_replay(&sid).await?;
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(&snap.data)
            .unwrap_or_default();
        let text = String::from_utf8_lossy(&bytes).to_string();
        Ok(ToolOutcome {
            ok: true,
            output: text,
        })
    }
}

pub struct GetDiff;
#[async_trait]
impl Tool for GetDiff {
    fn name(&self) -> &str {
        "agentd_get_diff"
    }
    fn description(&self) -> &str {
        "`git diff HEAD` for the session's worktree (empty if not a git repo)."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": { "session_id": { "type": "string" } }, "required": ["session_id"] })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = need_str(&input, "session_id")?;
        let c = client(ctx).await?;
        let d = c.diff(&sid).await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&d)?,
        })
    }
}

pub struct ListHarnesses;
#[async_trait]
impl Tool for ListHarnesses {
    fn name(&self) -> &str {
        "agentd_list_harnesses"
    }
    fn description(&self) -> &str {
        "List available adapter harnesses (shell, claude, codex, smith, …)."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let c = client(ctx).await?;
        let h = c.harnesses().await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&h)?,
        })
    }
}

// ---------- write ----------

pub struct CreateSession;
#[async_trait]
impl Tool for CreateSession {
    fn name(&self) -> &str {
        "agentd_create_session"
    }
    fn description(&self) -> &str {
        "Spawn a new top-level/visible session in the fleet. Use this when the user asks \
         for a new session or independent session. If the user says subagent, use \
         agentd_subagent_create instead so the child is parented to the current \
         session. `harness` must match `agentd_list_harnesses`. `cwd` defaults to \
         the current session cwd. `worktree:true` starts in an isolated git worktree."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "harness":  { "type": "string" },
                "cwd":      { "type": "string" },
                "prompt":   { "type": "string" },
                "title":    { "type": "string" },
                "mode":     { "type": "string", "enum": ["interactive", "headless"] },
                "worktree": { "type": "boolean" }
            },
            "required": ["harness"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        let h = input.get("harness").and_then(|s| s.as_str()).unwrap_or("?");
        let p = input.get("prompt").and_then(|s| s.as_str()).unwrap_or("");
        if p.is_empty() {
            format!("harness={h}")
        } else {
            format!("harness={h} prompt={p}")
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let harness = need_str(&input, "harness")?;
        let cwd = input
            .get("cwd")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string());
        let params = CreateSessionParams {
            harness,
            cwd,
            prompt: input
                .get("prompt")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            model: None,
            title: input
                .get("title")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            mode: input
                .get("mode")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            pty_size: Some(PtySize {
                cols: 100,
                rows: 30,
            }),
            forked_from: None,
            worktree: input
                .get("worktree")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            env: Default::default(),
            args: Vec::new(),
            // Sessions created via the agentd-control tool are always
            // user sessions — the orchestrator is daemon-internal only.
            kind: construct_protocol::SessionKind::User,
            parent_session_id: None,
            group_id: input
                .get("group_id")
                .and_then(|s| s.as_str())
                .map(|s| s.to_string()),
            position_after_session_id: None,
        };
        let c = client(ctx).await?;
        let sid = c.create(params).await?;
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "session_id": sid }).to_string(),
        })
    }
}

macro_rules! simple_write_tool {
    ($struct_name:ident, $tool_name:literal, $desc:literal, $extra_props:expr, $required:expr, $call:expr, $summary_key:literal) => {
        pub struct $struct_name;
        #[async_trait]
        impl Tool for $struct_name {
            fn name(&self) -> &str { $tool_name }
            fn description(&self) -> &str { $desc }
            fn schema(&self) -> Value {
                let mut props = serde_json::Map::new();
                props.insert("session_id".to_string(), json!({ "type": "string" }));
                for (k, v) in $extra_props {
                    props.insert(k.to_string(), v);
                }
                json!({
                    "type": "object",
                    "properties": Value::Object(props),
                    "required": $required,
                })
            }
            fn risk(&self) -> ToolRisk { ToolRisk::Risky }
            fn args_summary(&self, input: &Value) -> String {
                let sid = input.get("session_id").and_then(|s| s.as_str()).unwrap_or("?");
                if $summary_key.is_empty() {
                    sid.to_string()
                } else {
                    let extra = input.get($summary_key).and_then(|s| s.as_str()).unwrap_or("");
                    if extra.is_empty() { sid.to_string() } else { format!("{sid} {}", super::truncate_for_model(extra, 120)) }
                }
            }
            async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
                let sid = need_str(&input, "session_id")?;
                let c = client(ctx).await?;
                ($call)(&c, &sid, &input).await?;
                Ok(ToolOutcome { ok: true, output: json!({ "ok": true }).to_string() })
            }
        }
    };
}

simple_write_tool!(
    SendInput,
    "agentd_send_input",
    "Send a line of text to a session as user input (line-oriented).",
    vec![("text", json!({ "type": "string" }))],
    json!(["session_id", "text"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let text = need_str(input, "text").unwrap_or_default();
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.send_input(&sid, text).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "text"
);

simple_write_tool!(
    SendKeys,
    "agentd_send_keys",
    "Send raw bytes (base64-encoded) to a PTY-backed session. Use for control chars / arrows.",
    vec![("bytes_b64", json!({ "type": "string" }))],
    json!(["session_id", "bytes_b64"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let b64 = need_str(input, "bytes_b64").unwrap_or_default();
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move {
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes())?;
            c.pty_input(&sid, bytes).await
        }) as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "bytes_b64"
);

simple_write_tool!(
    InterruptSession,
    "agentd_interrupt_session",
    "Send an interrupt (Ctrl-C semantics) to a session.",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.interrupt(&sid).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    StopSession,
    "agentd_stop_session",
    "Ask a session to wind down cleanly.",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.stop(&sid).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    KillSession,
    "agentd_kill_session",
    "Kill a session immediately (SIGKILL the adapter).",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.kill(&sid).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    DeleteSession,
    "agentd_delete_session",
    "Delete a session — kills it if alive, drops its transcript + worktree.",
    Vec::<(&str, Value)>::new(),
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, _input: &Value| {
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.delete(&sid).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    PinSession,
    "agentd_pin_session",
    "Toggle the pinned flag on a session (controls the TUI pin strip).",
    vec![("pinned", json!({ "type": "boolean" }))],
    json!(["session_id", "pinned"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let pinned = input
            .get("pinned")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.set_pinned(&sid, pinned).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    ""
);

simple_write_tool!(
    RenameSession,
    "agentd_rename_session",
    "Set the session's user-facing title (or clear it by omitting `title`).",
    vec![("title", json!({ "type": "string" }))],
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let title = input
            .get("title")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.set_title(&sid, title).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "title"
);

simple_write_tool!(
    SetSessionGroup,
    "agentd_set_session_group",
    "Move a session into a group, or ungroup it by omitting `group_id` (or passing null). \
     `position` is \"top\" or \"bottom\" of the target region (default \"bottom\").",
    vec![
        ("group_id", json!({ "type": ["string", "null"] })),
        (
            "position",
            json!({ "type": "string", "enum": ["top", "bottom"] })
        )
    ],
    json!(["session_id"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let group_id = match input.get("group_id") {
            Some(Value::Null) | None => None,
            Some(Value::String(s)) => Some(s.clone()),
            _ => None,
        };
        let position = match input
            .get("position")
            .and_then(|v| v.as_str())
            .unwrap_or("bottom")
        {
            "top" => construct_protocol::SessionGroupPosition::Top,
            _ => construct_protocol::SessionGroupPosition::Bottom,
        };
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.set_session_group(&sid, group_id, position).await })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "group_id"
);

simple_write_tool!(
    MoveSession,
    "agentd_move_session",
    "Reorder a session within its region — `direction` `up` swaps with the session above (or \
     exits into the previous region at its top edge), `down` is symmetric.",
    vec![(
        "direction",
        json!({ "type": "string", "enum": ["up", "down"] })
    )],
    json!(["session_id", "direction"]),
    |c: &Arc<Client>, sid: &str, input: &Value| {
        let dir = match input
            .get("direction")
            .and_then(|v| v.as_str())
            .unwrap_or("down")
        {
            "up" => construct_protocol::MoveDirection::Up,
            _ => construct_protocol::MoveDirection::Down,
        };
        let c = c.clone();
        let sid = sid.to_string();
        Box::pin(async move { c.move_session(&sid, dir).await.map(|_| ()) })
            as std::pin::Pin<Box<dyn std::future::Future<Output = Result<()>> + Send>>
    },
    "direction"
);

// ---------- Loops ----------

/// Helper: read the calling session's id from the env injected by
/// the daemon. The agentd-control tools default to "this" session
/// when the LLM doesn't supply a session_id explicitly.
fn calling_session_id() -> Option<String> {
    std::env::var("CONSTRUCT_SESSION_ID").ok()
}

pub struct LoopCreate;
#[async_trait]
impl Tool for LoopCreate {
    fn name(&self) -> &str {
        "agentd_loop_create"
    }
    fn description(&self) -> &str {
        "Create a recurring prompt that fires into a session at a regular interval. \
         `interval_seconds` sets the cadence (clamped to host bounds — default 30s..86400s). \
         `prompt` is the text that will be injected as if the user typed it. \
         `expires_in_seconds` optionally caps how long the loop runs. \
         `session_id` defaults to the calling session."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id":         { "type": "string" },
                "interval_seconds":   { "type": "integer", "minimum": 1 },
                "expires_in_seconds": { "type": "integer", "minimum": 1 },
                "prompt":             { "type": "string" }
            },
            "required": ["interval_seconds", "prompt"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        let secs = input
            .get("interval_seconds")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
        let preview: String = prompt.chars().take(40).collect();
        format!("{secs}s — {preview}")
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = input
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
            .or_else(calling_session_id)
            .ok_or_else(|| anyhow!("session_id required (and CONSTRUCT_SESSION_ID unset)"))?;
        let secs = input
            .get("interval_seconds")
            .and_then(|v| v.as_u64())
            .ok_or_else(|| anyhow!("interval_seconds required"))?;
        let prompt = need_str(&input, "prompt")?;
        let expires_in = input.get("expires_in_seconds").and_then(|v| v.as_u64());
        let expires_at_ms =
            expires_in.map(|d| chrono::Utc::now().timestamp_millis() + (d as i64) * 1000);
        let c = client(ctx).await?;
        let l = c
            .loop_create(construct_protocol::LoopCreateParams {
                session_id: sid,
                spec: construct_protocol::LoopSpec::Interval { seconds: secs },
                prompt,
                expires_at_ms,
            })
            .await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&l)?,
        })
    }
}

pub struct LoopList;
#[async_trait]
impl Tool for LoopList {
    fn name(&self) -> &str {
        "agentd_loop_list"
    }
    fn description(&self) -> &str {
        "List recurring prompts (loops) attached to a session, or to all sessions when \
         `session_id` is omitted. Returns metadata + next fire time."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "session_id": { "type": "string" } }
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = input
            .get("session_id")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let c = client(ctx).await?;
        let loops = c.loop_list(sid.as_deref()).await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&loops)?,
        })
    }
}

pub struct LoopUpdate;
#[async_trait]
impl Tool for LoopUpdate {
    fn name(&self) -> &str {
        "agentd_loop_update"
    }
    fn description(&self) -> &str {
        "Update a loop's interval / prompt / expiry. Omitted fields keep their current value."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "loop_id":            { "type": "string" },
                "interval_seconds":   { "type": "integer", "minimum": 1 },
                "prompt":             { "type": "string" },
                "expires_at_ms":      { "type": "integer" }
            },
            "required": ["loop_id"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let loop_id = need_str(&input, "loop_id")?;
        let spec = input
            .get("interval_seconds")
            .and_then(|v| v.as_u64())
            .map(|s| construct_protocol::LoopSpec::Interval { seconds: s });
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string());
        let expires_at_ms = input.get("expires_at_ms").and_then(|v| v.as_i64());
        let c = client(ctx).await?;
        let l = c
            .loop_update(construct_protocol::LoopUpdateParams {
                loop_id,
                spec,
                prompt,
                expires_at_ms,
            })
            .await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&l)?,
        })
    }
}

pub struct LoopRemove;
#[async_trait]
impl Tool for LoopRemove {
    fn name(&self) -> &str {
        "agentd_loop_remove"
    }
    fn description(&self) -> &str {
        "Remove a recurring prompt by loop_id. Stops future firings."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "loop_id": { "type": "string" } },
            "required": ["loop_id"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let loop_id = need_str(&input, "loop_id")?;
        let c = client(ctx).await?;
        c.loop_remove(&loop_id).await?;
        Ok(ToolOutcome {
            ok: true,
            output: format!("removed {loop_id}"),
        })
    }
}

// ---------- Program ----------

/// Native mirror of the MCP `construct_program_get` tool: read a session's
/// current Program document. Exists so a smith session without an MCP
/// connection can read Program content at all — no native tool covered this
/// before (spec 0089 verb subagents get the document inlined in their
/// prompt at spawn time, but this is their live, always-fresh fallback for a
/// document that changed since, or one too large to inline in full).
pub struct ProgramGet;
#[async_trait]
impl Tool for ProgramGet {
    fn name(&self) -> &str {
        "agentd_program_get"
    }
    fn description(&self) -> &str {
        "Read a session's current Program document: its Markdown, version, and per-block \
         shimmer state. `session_id` defaults to the calling session."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": { "session_id": { "type": "string" } }
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = input
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .or_else(calling_session_id)
            .ok_or_else(|| anyhow!("session_id required (and CONSTRUCT_SESSION_ID unset)"))?;
        let c = client(ctx).await?;
        let result = c.program_get(&sid).await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&result)?,
        })
    }
}

/// Native mirror of the MCP `construct_program_edit` tool, minus shimmer
/// declaration. Exists so a smith session — the orchestrator included — can
/// apply an anchored Program edit without an MCP connection; today the only
/// caller is verb-drift escalation (spec 0089), where the daemon asks the
/// Program-owning session to reconcile a subagent's result whose selection
/// anchor changed underneath it.
pub struct ProgramEdit;
#[async_trait]
impl Tool for ProgramEdit {
    fn name(&self) -> &str {
        "agentd_program_edit"
    }
    fn description(&self) -> &str {
        "Apply one or more anchored find/replace edits to a session's Program document. Each \
         edit replaces `old_string` with `new_string`; set `replace_all` to replace every \
         occurrence, or include enough surrounding context to make `old_string` unique. An \
         empty `old_string` appends `new_string`. Edits apply to the LATEST program content, so \
         concurrent changes to other regions merge cleanly. Fails — writing nothing — if an \
         `old_string` is missing or ambiguous, meaning that exact text changed underneath you: \
         re-read the program and retry. `session_id` defaults to the calling session."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "session_id": { "type": "string" },
                "edits": {
                    "type": "array",
                    "minItems": 1,
                    "items": {
                        "type": "object",
                        "properties": {
                            "old_string": { "type": "string" },
                            "new_string": { "type": "string" },
                            "replace_all": { "type": "boolean" }
                        },
                        "required": ["old_string", "new_string"]
                    }
                },
                "note": { "type": "string" }
            },
            "required": ["edits"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let sid = input
            .get("session_id")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string())
            .or_else(calling_session_id)
            .ok_or_else(|| anyhow!("session_id required (and CONSTRUCT_SESSION_ID unset)"))?;
        let edits_val = input
            .get("edits")
            .cloned()
            .ok_or_else(|| anyhow!("missing `edits`"))?;
        let edits: Vec<construct_protocol::ProgramEdit> = serde_json::from_value(edits_val)
            .map_err(|e| anyhow!("invalid `edits`: {e}"))?;
        if edits.is_empty() {
            return Err(anyhow!("edits must not be empty"));
        }
        let note = input
            .get("note")
            .and_then(|s| s.as_str())
            .map(|s| s.to_string());
        let params = construct_protocol::ProgramEditParams {
            session_id: sid,
            edits,
            actor: construct_protocol::ProgramUpdateActor::Agent,
            note,
            shimmer: Vec::new(),
        };
        let c = client(ctx).await?;
        let result = c.program_edit(params).await?;
        Ok(ToolOutcome {
            ok: true,
            output: serde_json::to_string(&result)?,
        })
    }
}
