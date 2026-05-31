//! Zarvis subagent tools.
//!
//! A subagent is backed by an agentd session so every harness can run
//! unchanged, but it is marked `SessionKind::Subagent` and tracked in
//! the parent Zarvis session's data dir. The parent sees task-like
//! create/list/peek/enqueue/cancel/delete operations; the main TUI list
//! does not show the backing sessions.

use super::agentd::client;
use super::{Tool, ToolCtx, ToolOutcome};
use agentd_protocol::{CreateSessionParams, PtySize, SessionDetail, ToolRisk};
use anyhow::{anyhow, Context, Result};
use async_trait::async_trait;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize)]
struct SubagentRecord {
    id: String,
    harness: String,
    cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    prompt: Option<String>,
    created_at_ms: i64,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct SubagentRegistry {
    #[serde(default)]
    subagents: Vec<SubagentRecord>,
}

fn need_str(input: &Value, key: &str) -> Result<String> {
    input
        .get(key)
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("missing `{key}`"))
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn registry_path() -> Result<PathBuf> {
    let dir = std::env::var("AGENTD_SESSION_DATA_DIR")
        .context("AGENTD_SESSION_DATA_DIR is not set; subagents require an agentd session")?;
    Ok(PathBuf::from(dir).join("zarvis-subagents.json"))
}

fn load_registry() -> Result<SubagentRegistry> {
    let path = registry_path()?;
    if !path.exists() {
        return Ok(SubagentRegistry::default());
    }
    let data = std::fs::read_to_string(&path)
        .with_context(|| format!("read subagent registry {}", path.display()))?;
    Ok(serde_json::from_str(&data)
        .with_context(|| format!("parse subagent registry {}", path.display()))?)
}

fn save_registry(registry: &SubagentRegistry) -> Result<()> {
    let path = registry_path()?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create subagent registry dir {}", parent.display()))?;
    }
    let data = serde_json::to_string_pretty(registry)?;
    std::fs::write(&path, data)
        .with_context(|| format!("write subagent registry {}", path.display()))?;
    Ok(())
}

fn record_for(input: &Value, ctx: &ToolCtx, id: String) -> Result<SubagentRecord> {
    let harness = need_str(input, "harness")?;
    let cwd = input
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string());
    Ok(SubagentRecord {
        id,
        harness,
        cwd,
        title: input
            .get("title")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        prompt: input
            .get("prompt")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned),
        created_at_ms: now_ms(),
    })
}

fn require_owned(registry: &SubagentRegistry, id: &str) -> Result<()> {
    if registry.subagents.iter().any(|r| r.id == id) {
        Ok(())
    } else {
        Err(anyhow!("unknown subagent `{id}`"))
    }
}

fn schema_id_only() -> Value {
    json!({
        "type": "object",
        "properties": {
            "subagent_id": { "type": "string" }
        },
        "required": ["subagent_id"]
    })
}

pub struct Create;
#[async_trait]
impl Tool for Create {
    fn name(&self) -> &str {
        "agentd_subagent_create"
    }
    fn description(&self) -> &str {
        "Create a subagent: a child agent parented to the current session, shown nested \
         under it in clients, and backed by any agentd harness. Use this by default \
         when the user says subagent, asks to split work, or asks to parallelize \
         bounded review/research tasks. The returned subagent_id is used with \
         agentd_subagent_* tools."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "harness": { "type": "string" },
                "prompt": { "type": "string" },
                "cwd": { "type": "string" },
                "title": { "type": "string" },
                "mode": {
                    "type": "string",
                    "description": "Adapter mode. Defaults to headless."
                },
                "worktree": { "type": "boolean" }
            },
            "required": ["harness"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        let harness = input.get("harness").and_then(|v| v.as_str()).unwrap_or("?");
        let title = input.get("title").and_then(|v| v.as_str()).unwrap_or("");
        if title.is_empty() {
            format!("harness={harness}")
        } else {
            format!("harness={harness} title={title}")
        }
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let harness = need_str(&input, "harness")?;
        let cwd = input
            .get("cwd")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| ctx.cwd.to_string_lossy().to_string());
        let mode = input
            .get("mode")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| "headless".to_string());
        let title = input
            .get("title")
            .and_then(|v| v.as_str())
            .map(ToOwned::to_owned)
            .or_else(|| Some(format!("subagent:{}", harness)));
        let mut env = std::collections::HashMap::new();
        env.insert(
            "AGENTD_PARENT_SESSION_ID".to_string(),
            ctx.session_id.clone(),
        );
        let params = CreateSessionParams {
            harness,
            cwd,
            prompt: input
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(ToOwned::to_owned),
            model: None,
            title,
            mode: Some(mode),
            pty_size: Some(PtySize {
                cols: 100,
                rows: 30,
            }),
            worktree: input
                .get("worktree")
                .and_then(|v| v.as_bool())
                .unwrap_or(false),
            env,
            args: Vec::new(),
            kind: agentd_protocol::SessionKind::Subagent,
            parent_session_id: Some(ctx.session_id.clone()),
            group_id: None,
        };
        let c = client(ctx).await?;
        let id = c.create(params).await?;
        let mut registry = load_registry()?;
        let record = record_for(&input, ctx, id.clone())?;
        registry.subagents.retain(|r| r.id != id);
        registry.subagents.push(record.clone());
        save_registry(&registry)?;
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "subagent_id": id, "subagent": record }).to_string(),
        })
    }
}

pub struct List;
#[async_trait]
impl Tool for List {
    fn name(&self) -> &str {
        "agentd_subagent_list"
    }
    fn description(&self) -> &str {
        "List subagents created by this Zarvis session, including current backing \
         session summaries when still present."
    }
    fn schema(&self) -> Value {
        json!({ "type": "object", "properties": {} })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    async fn run(&self, _input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let registry = load_registry()?;
        let c = client(ctx).await?;
        let mut items = Vec::new();
        for record in registry.subagents {
            match c.get(&record.id).await {
                Ok(detail) => items.push(json!({
                    "subagent": record,
                    "summary": detail.summary,
                })),
                Err(err) => items.push(json!({
                    "subagent": record,
                    "error": err.to_string(),
                })),
            }
        }
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "subagents": items }).to_string(),
        })
    }
}

pub struct Peek;
#[async_trait]
impl Tool for Peek {
    fn name(&self) -> &str {
        "agentd_subagent_peek"
    }
    fn description(&self) -> &str {
        "Peek at a subagent's current output. PTY-backed subagents return recent \
         scrollback text; headless subagents return a tail of structured events."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subagent_id": { "type": "string" },
                "limit": { "type": "integer", "minimum": 1, "description": "Event tail size for non-PTY subagents; default 20." }
            },
            "required": ["subagent_id"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Safe
    }
    fn args_summary(&self, input: &Value) -> String {
        input
            .get("subagent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let id = need_str(&input, "subagent_id")?;
        let registry = load_registry()?;
        require_owned(&registry, &id)?;
        let c = client(ctx).await?;
        let detail: SessionDetail = c.get(&id).await?;
        if detail.summary.has_pty {
            let snap = c.pty_replay(&id).await?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&snap.data)
                .unwrap_or_default();
            let text = String::from_utf8_lossy(&bytes).to_string();
            return Ok(ToolOutcome {
                ok: true,
                output: json!({
                    "summary": detail.summary,
                    "output": text,
                })
                .to_string(),
            });
        }
        let limit = input.get("limit").and_then(|v| v.as_u64()).unwrap_or(20) as usize;
        let start = detail.events.len().saturating_sub(limit);
        let events = detail.events[start..].to_vec();
        Ok(ToolOutcome {
            ok: true,
            output: json!({
                "summary": detail.summary,
                "events": events,
            })
            .to_string(),
        })
    }
}

pub struct Enqueue;
#[async_trait]
impl Tool for Enqueue {
    fn name(&self) -> &str {
        "agentd_subagent_enqueue"
    }
    fn description(&self) -> &str {
        "Enqueue a user message/input line for a subagent."
    }
    fn schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "subagent_id": { "type": "string" },
                "text": { "type": "string" }
            },
            "required": ["subagent_id", "text"]
        })
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        let id = input
            .get("subagent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?");
        let text = input.get("text").and_then(|v| v.as_str()).unwrap_or("");
        format!("{id} {}", super::truncate_for_model(text, 120))
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let id = need_str(&input, "subagent_id")?;
        let text = need_str(&input, "text")?;
        let registry = load_registry()?;
        require_owned(&registry, &id)?;
        let c = client(ctx).await?;
        c.send_input(&id, text).await?;
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "ok": true }).to_string(),
        })
    }
}

pub struct Cancel;
#[async_trait]
impl Tool for Cancel {
    fn name(&self) -> &str {
        "agentd_subagent_cancel"
    }
    fn description(&self) -> &str {
        "Interrupt a subagent's current turn/task without deleting the subagent."
    }
    fn schema(&self) -> Value {
        schema_id_only()
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        input
            .get("subagent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let id = need_str(&input, "subagent_id")?;
        let registry = load_registry()?;
        require_owned(&registry, &id)?;
        let c = client(ctx).await?;
        c.interrupt(&id).await?;
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "ok": true }).to_string(),
        })
    }
}

pub struct Delete;
#[async_trait]
impl Tool for Delete {
    fn name(&self) -> &str {
        "agentd_subagent_delete"
    }
    fn description(&self) -> &str {
        "Delete a subagent and remove it from this Zarvis session's registry."
    }
    fn schema(&self) -> Value {
        schema_id_only()
    }
    fn risk(&self) -> ToolRisk {
        ToolRisk::Risky
    }
    fn args_summary(&self, input: &Value) -> String {
        input
            .get("subagent_id")
            .and_then(|v| v.as_str())
            .unwrap_or("?")
            .to_string()
    }
    async fn run(&self, input: Value, ctx: &ToolCtx) -> Result<ToolOutcome> {
        let id = need_str(&input, "subagent_id")?;
        let mut registry = load_registry()?;
        require_owned(&registry, &id)?;
        let c = client(ctx).await?;
        let delete_result = c.delete(&id).await;
        registry.subagents.retain(|r| r.id != id);
        save_registry(&registry)?;
        delete_result?;
        Ok(ToolOutcome {
            ok: true,
            output: json!({ "ok": true }).to_string(),
        })
    }
}
