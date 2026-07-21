//! MCP tool catalog and dispatchers. Each tool wraps one or more methods
//! on the daemon IPC client.

use anyhow::{anyhow, Result};
use base64::Engine;
use construct_client::Client;
use construct_protocol::{agent_context, CreateSessionParams, PtySize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

mod browser;

const CONTEXT_TOOL_NAME: &str = "construct_context";
const DEFAULT_PTY_BYTES: usize = 64 * 1024;
const MAX_MCP_PTY_BYTES: usize = 512 * 1024;
const DEFAULT_TRANSCRIPT_EVENTS: usize = 50;
const MAX_TRANSCRIPT_EVENTS: usize = 200;
const MAX_COMPACT_EVENT_CHARS: usize = 16 * 1024;
const DEFAULT_DIFF_CHARS: usize = 64 * 1024;
const MAX_DIFF_CHARS: usize = 512 * 1024;
const DEFAULT_USAGE_CHARS: usize = 16 * 1024;

fn truncate_chars(text: &str, limit: usize) -> (String, bool) {
    if text.chars().count() <= limit {
        return (text.to_string(), false);
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    (out, true)
}

fn compact_session_summary(
    summary: &construct_protocol::SessionSummary,
    group_name: Option<&str>,
) -> Value {
    let mut out = serde_json::Map::new();
    out.insert("id".into(), json!(summary.id));
    out.insert("harness".into(), json!(summary.harness));
    out.insert("state".into(), json!(summary.state));
    if let Some(title) = &summary.title {
        out.insert("title".into(), json!(title));
    }
    if let Some(model) = &summary.model {
        out.insert("model".into(), json!(model));
    }
    if let Some(effort) = &summary.effort {
        out.insert("effort".into(), json!(effort));
    }
    if summary.pending_input {
        out.insert("pending_input".into(), json!(true));
    }
    if summary.has_pty {
        out.insert("has_pty".into(), json!(true));
    }
    if let Some(last_event_at) = &summary.last_event_at {
        out.insert("last_event_at".into(), json!(last_event_at));
    }
    if let Some(last_pty_at_ms) = summary.last_pty_at_ms {
        out.insert("last_pty_at_ms".into(), json!(last_pty_at_ms));
    }
    if let Some(group_id) = &summary.group_id {
        out.insert("group_id".into(), json!(group_id));
    }
    if let Some(name) = group_name {
        out.insert("group_name".into(), json!(name));
    }
    if let Some(parent) = &summary.parent_session_id {
        out.insert("parent_session_id".into(), json!(parent));
    }
    if summary.archived {
        out.insert("archived".into(), json!(true));
    }
    Value::Object(out)
}

fn compact_program_blocks(blocks: &[construct_protocol::ProgramBlockView]) -> Vec<Value> {
    blocks
        .iter()
        .map(|block| {
            let mut out = serde_json::Map::new();
            out.insert("id".into(), json!(block.id));
            out.insert("start_line".into(), json!(block.start_line));
            out.insert("end_line".into(), json!(block.end_line));
            if block.shimmer {
                out.insert("pending".into(), json!(true));
            }
            if let Some(tooltip) = &block.tooltip {
                out.insert("tooltip".into(), json!(tooltip));
            }
            Value::Object(out)
        })
        .collect()
}

fn compact_program_run(run: &construct_protocol::ProgramRunProgress) -> Value {
    json!({
        "stage": run.stage,
        "pending": run.pending_block_count(),
        "settled": run.settled_block_count,
        "total": run.total_block_count,
    })
}

fn compact_transcript_event(event: &construct_protocol::TimestampedEvent) -> Value {
    let full = serde_json::to_value(event).unwrap_or_else(|_| json!({}));
    let encoded = serde_json::to_string(&full).unwrap_or_default();
    if encoded.chars().count() <= MAX_COMPACT_EVENT_CHARS {
        return full;
    }
    let event_value = serde_json::to_value(&event.event).unwrap_or_else(|_| json!({}));
    let event_type = event_value
        .get("type")
        .and_then(Value::as_str)
        .map(str::to_string)
        .unwrap_or_else(|| "event".to_string());
    let (preview, _) = truncate_chars(
        &serde_json::to_string(&event_value).unwrap_or_default(),
        MAX_COMPACT_EVENT_CHARS,
    );
    json!({
        "seq": event.seq,
        "at": event.at,
        "event": { "type": event_type, "preview": preview, "truncated": true }
    })
}

fn render_terminal_text(bytes: &[u8], size: Option<construct_protocol::PtySize>) -> String {
    let size = size.unwrap_or(construct_protocol::PtySize {
        cols: 100,
        rows: 30,
    });
    let mut parser = vt100::Parser::new(size.rows.max(1), size.cols.max(1), 200);
    parser.process(bytes);
    parser.screen().contents().trim_end().to_string()
}

/// Per-process memory of what the context tool already served — one MCP
/// server process serves one agent, so this is per-agent state. See
/// [`agent_context::ContextServeState`].
pub type ContextServeState = std::sync::Mutex<agent_context::ContextServeState>;

fn diff_stats(patch: &str) -> Value {
    let mut files = Vec::new();
    let mut additions = 0usize;
    let mut deletions = 0usize;
    for line in patch.lines() {
        if let Some(rest) = line.strip_prefix("diff --git a/") {
            let path = rest.split(" b/").next().unwrap_or(rest);
            files.push(path.to_string());
        } else if line.starts_with('+') && !line.starts_with("+++") {
            additions += 1;
        } else if line.starts_with('-') && !line.starts_with("---") {
            deletions += 1;
        }
    }
    json!({ "files": files, "additions": additions, "deletions": deletions })
}

/// Static tool catalog returned by `tools/list`.
pub fn catalog() -> Vec<Value> {
    let mut tools = vec![
        // ----- Read -----
        tool(
            CONTEXT_TOOL_NAME,
            "Load current construct memory, Program-run state, and widget paths. Call before each task. Repeat calls omit unchanged and already-served content automatically; everything omitted stays recoverable (memory by path, the program via construct_program_get).",
            json!({
                "type": "object",
                "properties": {
                    "refresh": { "type": "boolean", "description": "Resend static fields and unchanged content. Pass true when earlier construct_context results may have been compacted out of your context." },
                    "skip_memory": { "type": "boolean", "description": "Omit memory file contents you already hold verbatim (e.g. you just wrote them); paths and etags are still returned." },
                    "include_reference": { "type": "boolean", "description": "Include the full memory/widget policy and Markdown extension reference." }
                }
            }),
        ),
        tool(
            "construct_whoami",
            "Returns the CONSTRUCT_SESSION_ID env var visible to this MCP server, which is the construct session id that the calling agent is running inside. Returns null if unset (the MCP server is running outside a construct-managed session).",
            schema_empty(),
        ),
        tool(
            "construct_list_sessions",
            "List sessions as a compact fleet projection. Use `detail:full` only for UI/debug metadata.",
            json!({
                "type": "object",
                "properties": {
                    "limit": { "type": "integer", "minimum": 1, "maximum": 500 },
                    "detail": { "type": "string", "enum": ["compact", "full"] }
                }
            }),
        ),
        tool(
            "construct_list_harnesses",
            "List the available agent harnesses (shell, claude, codex, …). Each entry includes whether the binary was resolvable on this host.",
            schema_empty(),
        ),
        tool(
            "construct_usage_query",
            "Return a cached harness usage screen as bounded plain text; optionally trigger a background refresh.",
            json!({
                "type": "object",
                "properties": {
                    "harness": { "type": "string" },
                    "allow_refresh": { "type": "boolean" },
                    "max_chars": { "type": "integer", "minimum": 1, "maximum": 65536 }
                },
                "required": ["harness"]
            }),
        ),
        tool(
            "construct_get_session",
            "Fetch compact session detail. The default excludes transcript history; request a bounded `tail` or explicit `detail:full` when needed.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "tail": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "include_panels": { "type": "boolean" },
                    "detail": { "type": "string", "enum": ["compact", "full"] }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "construct_get_transcript",
            "Fetch bounded structured history. Defaults to the latest 50 events; oversized individual events are compacted unless `detail:full`.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "from":       { "type": "integer", "minimum": 0 },
                    "limit":      { "type": "integer", "minimum": 1, "maximum": 200 },
                    "tail":       { "type": "integer", "minimum": 1, "maximum": 200 },
                    "detail":     { "type": "string", "enum": ["compact", "full"] }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "construct_get_output",
            "Fetch a bounded PTY tail. Defaults to a rendered 64 KiB screen; offsets support deliberate backward paging.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "max_bytes": { "type": "integer", "minimum": 1, "maximum": 524288 },
                    "before_offset": { "type": "integer", "minimum": 0 },
                    "mode": { "type": "string", "enum": ["screen", "text"] }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "construct_get_diff",
            "Return a bounded worktree diff or compact diff statistics.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "mode": { "type": "string", "enum": ["patch", "stat"] },
                    "max_chars": { "type": "integer", "minimum": 1, "maximum": 524288 },
                    "offset": { "type": "integer", "minimum": 0 }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "construct_get_tasks",
            "List bounded running/background/recent tool tasks for a session.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 50 },
                    "active_only": { "type": "boolean" }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "construct_program_get",
            "Fetch current Program Markdown once plus compact block refs/status. Revisions are omitted unless explicitly requested.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "revisions": { "type": "string", "enum": ["none", "metadata", "full"] }
                }
            }),
        ),
        tool(
            "construct_search",
            "Search session names, Programs, and transcripts. Results are bounded; use hit `seq` with construct_get_transcript for context.",
            json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string", "description": "Case-insensitive substring to search for." },
                    "scopes": {
                        "type": "array",
                        "description": "Restrict the search to these scopes. Omit to search all three.",
                        "items": { "type": "string", "enum": ["name", "program", "transcript"] }
                    },
                    "session_id": { "type": "string", "description": "Restrict the search to one session." },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 100, "description": "Global hit cap; default 20." }
                },
                "required": ["query"]
            }),
        ),
        tool(
            "construct_program_list_templates",
            "List Program template metadata, or fetch one template body by id.",
            json!({ "type": "object", "properties": { "template_id": { "type": "string" } } }),
        ),
        tool(
            "construct_program_list_verbs",
            "List Program selection-verb metadata; internal purpose prompts are omitted.",
            schema_empty(),
        ),
        // ----- Write -----
        tool(
            "construct_program_edit",
            "Edit Program text and/or run status against the latest document. Anchored replacements are atomic; put both halves of a move in ONE call. Set `keep_pending` when an edit changes unfinished work. `pending` maps stable block refs to concise hover statuses; `settled` clears refs. On the first planning pass, set `settle_others` to clear every block omitted from `pending`. Stale refs fail closed. The compact response returns only refs created or changed by this call.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "edits": {
                        "type": "array",
                        "description": "All targeted replacements to apply atomically in order. For moves, put the removal and insertion edits in this same array so the block never disappears between tool calls.",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": { "type": "string" },
                                "new_string": { "type": "string" },
                                "replace_all": { "type": "boolean" },
                                "keep_pending": { "type": "boolean", "description": "Keep the block this edit produces shimmering (still pending) — adds its new stable ref in the same call. Set when an edit changes a block whose work is still in flight (e.g. moving a task to In progress or appending a @{session} clip)." }
                            },
                            "required": ["old_string", "new_string"]
                        }
                    },
                    "pending": {
                        "type": "object",
                        "description": "Pending block refs mapped to concise (≤10-word) hover statuses.",
                        "additionalProperties": { "type": "string" }
                    },
                    "settled": {
                        "type": "array",
                        "description": "Block refs whose work has settled.",
                        "items": { "type": "string" }
                    },
                    "settle_others": { "type": "boolean", "description": "Planning pass: settle every current block omitted from `pending`." },
                    "note": { "type": "string" }
                }
            }),
        ),
        tool(
            "construct_program_update",
            "Replace the entire Program document. Prefer anchored edit for targeted changes. `pending` maps zero-based block indexes in the new Markdown to concise statuses; omitted blocks settle. Returns compact block refs/status.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "markdown": { "type": "string" },
                    "base_version": { "type": "integer", "minimum": 0 },
                    "template_id": { "type": "string" },
                    "note": { "type": "string" },
                    "pending": {
                        "type": "object",
                        "description": "Zero-based block indexes mapped to concise pending statuses; omitted blocks settle.",
                        "additionalProperties": { "type": "string" }
                    }
                },
                "required": ["markdown"]
            }),
        ),
        tool(
            "construct_program_execute",
            "Execute the full Program or a selected fragment and return a compact run acknowledgement.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "selection": { "type": "string" },
                    "base_version": { "type": "integer", "minimum": 0 },
                    "shimmer": {
                        "type": "array",
                        "description": "Optional initial pending set: one boolean per block of the executed body in document order (true = pending). Omit to shimmer the whole executed region.",
                        "items": { "type": "boolean" }
                    }
                }
            }),
        ),
        tool(
            "construct_program_verb_execute",
            "Run a selection verb in a scoped subagent; the daemon merges its result. Returns the subagent id and only changed block refs.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "verb": { "type": "string", "description": "A verb `name` from construct_program_list_verbs." },
                    "selection": { "type": "string", "description": "The exact Markdown substring to refine — must match the current program content." },
                    "base_version": { "type": "integer", "minimum": 0 },
                    "comment": { "type": "string", "description": "Optional one-line instruction composed onto the verb's purpose prompt." }
                },
                "required": ["verb", "selection"]
            }),
        ),
        tool(
            "construct_create_session",
            "Spawn a new top-level/visible session in the fleet. Use this when the user asks for a new session or independent session. If the user says subagent, use construct_subagent_create instead so the child is parented to the current session. `harness` must match an available harness name (see construct_list_harnesses). `cwd` defaults to the caller's cwd when provided by the adapter, otherwise the daemon's process cwd. Set `worktree:true` to start the session in an isolated git worktree.",
            json!({
                "type": "object",
                "properties": {
                    "harness":  { "type": "string" },
                    "cwd":      { "type": "string" },
                    "prompt":   { "type": "string" },
                    "title":    { "type": "string" },
                    "mode":     { "type": "string", "enum": ["interactive", "headless"] },
                    "worktree": { "type": "boolean" },
                    "model": {
                        "type": "string",
                        "description": "Optional harness model spec; consult construct_list_harnesses or config."
                    }
                },
                "required": ["harness"]
            }),
        ),
        tool(
            "construct_fork_session",
            "Fork an existing session into a NEW sibling session backed by `harness` (which may differ from the source's). The fork inherits the source's working directory and group and runs independently — NOT a child/subagent, and the original session is left untouched (a session's own harness can't be changed in place). Unless `seed:false` (or the target is the `shell` harness), the fork is seeded with a summary of the source transcript so an agent harness can continue the prior context. Use this to continue a conversation under a different harness or model. Returns the new session_id.",
            json!({
                "type": "object",
                "properties": {
                    "source_session_id": { "type": "string" },
                    "harness":           { "type": "string" },
                    "model":             { "type": "string" },
                    "prompt":            { "type": "string" },
                    "seed":              { "type": "boolean" }
                },
                "required": ["source_session_id", "harness"]
            }),
        ),
        tool(
            "construct_merge_session",
            "Merge a forked session back into its parent. This stamps the fork's outcome onto the parent's event timeline and allows the parent to observe the result. `mode` must be either \"result\" (fork succeeded/produced value) or \"discard\" (fork was aborted/unsuccessful).",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "mode": { "type": "string", "enum": ["result", "discard"] }
                },
                "required": ["session_id", "mode"]
            }),
        ),
        tool(
            "construct_send_input",
            "Send a line of text to a session as user input. For PTY sessions a trailing newline is added automatically.",
            schema_obj(&[
                ("session_id", "string", true),
                ("text",       "string", true),
            ]),
        ),
        tool(
            "construct_send_keys",
            "Send raw bytes to a PTY-backed session (base64-encoded). Use this for control characters or arrow keys — e.g. `\\u0003` (C-c) base64 = \"Aw==\".",
            schema_obj(&[
                ("session_id", "string", true),
                ("bytes_b64",  "string", true),
            ]),
        ),
        tool(
            "construct_interrupt_session",
            "Send an interrupt to the session (the adapter decides the exact semantic: usually SIGINT-equivalent to the running child).",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "construct_stop_session",
            "Stop the session cleanly (asks the adapter to shut down). Use kill_session for hard kill.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "construct_kill_session",
            "SIGKILL the adapter (and its child). The session record stays; use delete_session to also drop the record.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "construct_delete_session",
            "Delete a session entirely: kill if running, remove transcript, worktree, and metadata from disk.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "construct_archive_session",
            "Archive a session (soft, reversible): terminate its adapter and hide it from the \
             session list, but KEEP its transcript and worktree on disk. Unlike delete_session, \
             nothing is removed and the session can be restarted later. Prefer this over \
             delete_session when you want to tidy up finished sessions without losing their history.",
            schema_obj(&[("session_id", "string", true)]),
        ),
        tool(
            "construct_pin_session",
            "Pin or unpin a session so it shows as a live tile in the TUI pin strip.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "pinned":     { "type": "boolean" }
                },
                "required": ["session_id", "pinned"]
            }),
        ),
        tool(
            "construct_rename_session",
            "Set a user-facing title for the session. Empty or omitted `title` clears it.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "title":      { "type": "string" }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "construct_set_session_group",
            "Move a session into a group, or ungroup it. Omit `group_id` (or pass null) \
             to remove the session from its current group. `position` is `top` or `bottom` \
             of the target region (default `bottom`).",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "group_id":   { "type": ["string", "null"] },
                    "position":   { "type": "string", "enum": ["top", "bottom"] }
                },
                "required": ["session_id"]
            }),
        ),
        tool(
            "construct_move_session",
            "Reorder a session within its current region — `direction` `up` swaps with the \
             session above (or moves into the previous group/ungrouped region when at the top \
             of its region); `down` is symmetric.",
            json!({
                "type": "object",
                "properties": {
                    "session_id": { "type": "string" },
                    "direction":  { "type": "string", "enum": ["up", "down"] }
                },
                "required": ["session_id", "direction"]
            }),
        ),
        tool(
            "construct_subagent_create",
            "Create a subagent: a child agent owned by the current construct session. Use this \
             by default when the user says subagent, asks to split work, or asks to \
             parallelize bounded review/research tasks. The subagent is backed by any \
             registered harness and appears nested under the parent session in clients.",
            json!({
                "type": "object",
                "properties": {
                    "harness":  { "type": "string" },
                    "cwd":      { "type": "string" },
                    "prompt":   { "type": "string" },
                    "title":    { "type": "string" },
                    "mode":     { "type": "string", "enum": ["interactive", "headless"] },
                    "worktree": { "type": "boolean" },
                    "model": {
                        "type": "string",
                        "description": "Optional harness model spec; consult construct_list_harnesses or config."
                    }
                },
                "required": ["harness"]
            }),
        ),
        tool(
            "construct_subagent_list",
            "List subagents owned by the current construct session, including current backing \
             session summaries.",
            schema_empty(),
        ),
        tool(
            "construct_subagent_peek",
            "Peek at bounded subagent output. PTY output defaults to a rendered 64 KiB screen; headless output defaults to 20 events.",
            json!({
                "type": "object",
                "properties": {
                    "subagent_id": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": 200 },
                    "max_bytes": { "type": "integer", "minimum": 1, "maximum": 524288 },
                    "before_offset": { "type": "integer", "minimum": 0 },
                    "mode": { "type": "string", "enum": ["screen", "text"] },
                    "detail": { "type": "string", "enum": ["compact", "full"] }
                },
                "required": ["subagent_id"]
            }),
        ),
        tool(
            "construct_subagent_enqueue",
            "Enqueue a user message/input line for a subagent owned by the current session.",
            schema_obj(&[
                ("subagent_id", "string", true),
                ("text",        "string", true),
            ]),
        ),
        tool(
            "construct_subagent_cancel",
            "Interrupt a subagent's current turn/task without deleting the subagent.",
            schema_obj(&[("subagent_id", "string", true)]),
        ),
        tool(
            "construct_subagent_delete",
            "Delete a subagent owned by the current session.",
            schema_obj(&[("subagent_id", "string", true)]),
        ),
        tool(
            "construct_subagent_archive",
            "Archive a subagent owned by the current session (soft, reversible): terminate it but \
             KEEP its transcript and worktree. Unlike subagent_delete, nothing is wiped and it can \
             be restarted later. Use this to tidy up finished subagents without losing their work.",
            schema_obj(&[("subagent_id", "string", true)]),
        ),
    ];
    tools.extend(browser::catalog());
    // Dev-only: hot-reload the daemon's web UI from a worktree's assets.
    // Only advertised in debug builds so it never appears in production.
    #[cfg(debug_assertions)]
    tools.push(tool(
        "webui_hot_reload",
        "Dev-only. Point the running construct daemon's web UI at a directory of assets \
         (typically `<worktree>/crates/daemon/assets`) so edits to `index.html` / static files \
         show up on browser refresh — a live-reload poller is injected so the page reloads on \
         save. Pass `dir: null` (or omit) to revert to the embedded assets. Lets a dev session \
         iterate on the web UI against an already-running daemon with no rebuild or restart.",
        json!({
            "type": "object",
            "properties": { "dir": { "type": "string" } }
        }),
    ));
    tools
}

fn tool(name: &str, description: &str, schema: Value) -> Value {
    json!({ "name": name, "description": description, "inputSchema": schema })
}

fn schema_empty() -> Value {
    json!({ "type": "object", "properties": {} })
}

fn schema_obj(fields: &[(&str, &str, bool)]) -> Value {
    let mut props = serde_json::Map::new();
    let mut required = Vec::new();
    for (n, ty, req) in fields {
        props.insert(n.to_string(), json!({ "type": ty }));
        if *req {
            required.push(n.to_string());
        }
    }
    json!({
        "type": "object",
        "properties": props,
        "required": required,
    })
}

/// Dispatch a `tools/call` to the right method. Returns the full
/// `tools/call` response payload (a `{content: [...], isError?}` object).
pub async fn call(
    client: &Arc<Client>,
    session_id: Option<&str>,
    context_state: &ContextServeState,
    params: Value,
) -> Result<Value> {
    let name = params
        .get("name")
        .and_then(|v| v.as_str())
        .ok_or_else(|| anyhow!("missing tool name"))?
        .to_string();
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    if matches!(
        name.as_str(),
        "browser_open" | "browser_inspect" | "browser_screenshot" | "browser_eval"
    ) {
        let result_json = browser::call(client.clone(), session_id, name.as_str(), args).await?;
        let text = serde_json::to_string(&result_json)?;
        return Ok(json!({
            "content": [{ "type": "text", "text": text }],
            "isError": false,
        }));
    }

    let result_json: Value = match name.as_str() {
        // ----- Read -----
        CONTEXT_TOOL_NAME => {
            let request = agent_context::ContextRequest::from_args(&args);
            let mut state = context_state.lock().unwrap_or_else(|p| p.into_inner());
            agent_context::compact_response(agent_context::build_from_env(), &request, &mut state)
        }
        "construct_whoami" => json!({ "session_id": session_id }),
        "construct_list_sessions" => {
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(100)
                .min(500) as usize;
            let detail_full = args.get("detail").and_then(Value::as_str) == Some("full");
            let sessions = client.list().await?;
            let groups = client.list_groups().await.unwrap_or_default();
            let group_name_by_id: HashMap<&str, &str> = groups
                .iter()
                .map(|g| (g.id.as_str(), g.name.as_str()))
                .collect();
            let enriched: Vec<Value> = sessions
                .iter()
                .take(limit)
                .map(|s| {
                    let group_name = s
                        .group_id
                        .as_deref()
                        .and_then(|id| group_name_by_id.get(id).copied());
                    if detail_full {
                        let mut value = serde_json::to_value(s).unwrap_or_else(|_| json!({}));
                        if let (Some(name), Value::Object(map)) = (group_name, &mut value) {
                            map.insert("group_name".into(), json!(name));
                        }
                        value
                    } else {
                        compact_session_summary(s, group_name)
                    }
                })
                .collect();
            let returned = enriched.len();
            json!({
                "sessions": enriched,
                "returned": returned,
                "total": sessions.len(),
                "truncated": sessions.len() > limit,
            })
        }
        "construct_list_harnesses" => Value::Array(
            client
                .harnesses()
                .await?
                .iter()
                .map(|harness| {
                    json!({
                        "name": harness.name,
                        "available": harness.available,
                        "detail": harness.detail,
                        "description": harness.description,
                        "capabilities": harness.capabilities,
                    })
                })
                .collect(),
        ),
        "construct_usage_query" => {
            let harness = arg_str(&args, "harness")?;
            let allow_refresh = args
                .get("allow_refresh")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);
            let result = client.usage_query(&harness, allow_refresh).await?;
            let max_chars = args
                .get("max_chars")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_USAGE_CHARS as u64)
                .min(65_536) as usize;
            let snapshot = result.snapshot.map(|snapshot| {
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(snapshot.bytes)
                    .unwrap_or_default();
                let rendered = render_terminal_text(
                    &bytes,
                    Some(construct_protocol::PtySize {
                        cols: snapshot.cols,
                        rows: snapshot.rows,
                    }),
                );
                let (text, truncated) = truncate_chars(&rendered, max_chars);
                json!({
                    "text": text,
                    "truncated": truncated,
                    "cols": snapshot.cols,
                    "rows": snapshot.rows,
                    "captured_at_ms": snapshot.captured_at_ms,
                })
            });
            json!({
                "snapshot": snapshot,
                "refreshing": result.refreshing,
                "enabled": result.enabled,
            })
        }
        "construct_get_session" => {
            let sid = arg_str(&args, "session_id")?;
            let summaries = client.list().await?;
            let summary = summaries
                .iter()
                .find(|summary| summary.id == sid)
                .ok_or_else(|| anyhow!("session not found: {sid}"))?;
            if args.get("detail").and_then(Value::as_str) == Some("full") {
                serde_json::to_value(client.get(&sid).await?)?
            } else {
                let mut out = serde_json::Map::new();
                out.insert("summary".into(), compact_session_summary(summary, None));
                if let Some(tail) = args.get("tail").and_then(Value::as_u64) {
                    let mut transcript = client
                        .transcript_tail(&sid, (tail as usize).min(MAX_TRANSCRIPT_EVENTS))
                        .await?;
                    transcript
                        .events
                        .retain(|event| !construct_protocol::slash::is_model_hidden(&event.event));
                    out.insert(
                        "events".into(),
                        Value::Array(
                            transcript
                                .events
                                .iter()
                                .map(compact_transcript_event)
                                .collect(),
                        ),
                    );
                    out.insert("event_total".into(), json!(transcript.total));
                }
                if args
                    .get("include_panels")
                    .and_then(Value::as_bool)
                    .unwrap_or(false)
                {
                    out.insert("ui_panels".into(), json!(client.get(&sid).await?.ui_panels));
                }
                Value::Object(out)
            }
        }
        "construct_get_transcript" => {
            let sid = arg_str(&args, "session_id")?;
            let count = args
                .get("tail")
                .or_else(|| args.get("limit"))
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_TRANSCRIPT_EVENTS as u64)
                .min(MAX_TRANSCRIPT_EVENTS as u64) as usize;
            let mut tr = if let Some(from) = args.get("from").and_then(Value::as_u64) {
                client.transcript(&sid, from, Some(count)).await?
            } else {
                client.transcript_tail(&sid, count).await?
            };
            // Strip model-invisible control commands (e.g. `/zoom`) so UI noise
            // never enters a reading model's context. The policy lives in the
            // shared slash registry, not a hardcoded tool-name check — this is
            // the principled fix for the old `tui` ToolUse transcript leak.
            tr.events
                .retain(|ev| !construct_protocol::slash::is_model_hidden(&ev.event));
            if args.get("detail").and_then(Value::as_str) == Some("full") {
                serde_json::to_value(tr)?
            } else {
                let events: Vec<Value> = tr.events.iter().map(compact_transcript_event).collect();
                json!({ "events": events, "returned": events.len(), "total": tr.total })
            }
        }
        "construct_get_output" => {
            let sid = arg_str(&args, "session_id")?;
            let max_bytes = args
                .get("max_bytes")
                .and_then(Value::as_u64)
                .unwrap_or(DEFAULT_PTY_BYTES as u64)
                .min(MAX_MCP_PTY_BYTES as u64) as usize;
            let before_offset = args.get("before_offset").and_then(Value::as_u64);
            let snap = client
                .pty_replay_range(&sid, max_bytes, before_offset)
                .await?;
            let bytes = base64::engine::general_purpose::STANDARD
                .decode(&snap.data)
                .unwrap_or_default();
            let text = if args.get("mode").and_then(Value::as_str) == Some("text") {
                String::from_utf8_lossy(&bytes).to_string()
            } else {
                render_terminal_text(&bytes, snap.size.clone())
            };
            json!({
                "text": text,
                "start_offset": snap.start_offset,
                "end_offset": snap.end_offset,
                "total_bytes": snap.total_bytes,
                "has_older": snap.start_offset > 0,
                "size": snap.size,
            })
        }
        "construct_get_diff" => {
            let sid = arg_str(&args, "session_id")?;
            let patch = client.diff(&sid).await?.patch;
            if args.get("mode").and_then(Value::as_str) == Some("stat") {
                diff_stats(&patch)
            } else {
                let max_chars = args
                    .get("max_chars")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_DIFF_CHARS as u64)
                    .min(MAX_DIFF_CHARS as u64) as usize;
                let total_chars = patch.chars().count();
                let offset = args.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
                let patch: String = patch.chars().skip(offset).take(max_chars).collect();
                let end_offset = offset
                    .saturating_add(patch.chars().count())
                    .min(total_chars);
                json!({
                    "patch": patch,
                    "offset": offset.min(total_chars),
                    "next_offset": (end_offset < total_chars).then_some(end_offset),
                    "truncated": offset > 0 || end_offset < total_chars,
                    "total_chars": total_chars,
                })
            }
        }
        "construct_get_tasks" => {
            let sid = arg_str(&args, "session_id")?;
            let mut tasks = client.list_tasks(&sid).await?;
            if args
                .get("active_only")
                .and_then(Value::as_bool)
                .unwrap_or(false)
            {
                tasks.retain(|task| {
                    matches!(
                        task.state,
                        construct_protocol::TaskState::Running
                            | construct_protocol::TaskState::Backgrounded
                    )
                });
            }
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .unwrap_or(20)
                .min(50) as usize;
            tasks.truncate(limit);
            json!({ "tasks": tasks })
        }
        "construct_search" => {
            let query = arg_str(&args, "query")?;
            let scopes = arg_scopes(&args, "scopes");
            let session_ids = arg_str(&args, "session_id").ok().map(|id| vec![id]);
            let limit = Some(arg_usize(&args, "limit").unwrap_or(20).min(100));
            serde_json::to_value(
                client
                    .search(construct_protocol::SearchParams {
                        query,
                        scopes,
                        session_ids,
                        limit,
                        per_session_limit: Some(3),
                    })
                    .await?,
            )?
        }
        "construct_program_get" => {
            let sid = optional_session_arg(&args, session_id)?;
            let result = client.program_get(&sid).await?;
            let mut out = serde_json::Map::new();
            out.insert("session_id".into(), json!(result.program.session_id));
            out.insert("markdown".into(), json!(result.program.markdown));
            out.insert("version".into(), json!(result.program.version));
            out.insert("updated_at_ms".into(), json!(result.program.updated_at_ms));
            if let Some(template_id) = result.program.template_id {
                out.insert("template_id".into(), json!(template_id));
            }
            out.insert(
                "blocks".into(),
                json!(compact_program_blocks(&result.blocks)),
            );
            if let Some(run) = &result.active_run {
                out.insert("run".into(), compact_program_run(run));
            }
            match args
                .get("revisions")
                .and_then(Value::as_str)
                .unwrap_or("none")
            {
                "metadata" => {
                    out.insert(
                        "revisions".into(),
                        Value::Array(
                            result
                                .revisions
                                .iter()
                                .map(|revision| {
                                    json!({
                                        "version": revision.version,
                                        "actor": revision.actor,
                                        "at_ms": revision.at_ms,
                                        "note": revision.note,
                                    })
                                })
                                .collect(),
                        ),
                    );
                }
                "full" => {
                    out.insert("revisions".into(), json!(result.revisions));
                }
                _ => {}
            }
            Value::Object(out)
        }
        "construct_program_list_templates" => {
            let templates = client.program_templates().await?.templates;
            if let Ok(template_id) = arg_str(&args, "template_id") {
                let template = templates
                    .into_iter()
                    .find(|template| template.id == template_id)
                    .ok_or_else(|| anyhow!("template not found: {template_id}"))?;
                serde_json::to_value(template)?
            } else {
                Value::Array(
                    templates
                        .iter()
                        .map(|template| {
                            json!({
                                "id": template.id,
                                "name": template.name,
                                "description": template.description,
                                "built_in": template.built_in,
                            })
                        })
                        .collect(),
                )
            }
        }
        "construct_program_list_verbs" => {
            let verbs = client.program_verbs().await?.verbs;
            Value::Array(
                verbs
                    .iter()
                    .map(|verb| {
                        json!({
                            "name": verb.name,
                            "label": verb.label,
                            "description": verb.description,
                            "effect": verb.effect,
                            "interaction": verb.interaction,
                        })
                    })
                    .collect(),
            )
        }
        // ----- Write -----
        "construct_program_update" => {
            let sid = optional_session_arg(&args, session_id)?;
            let markdown = arg_str(&args, "markdown")?;
            let block_count = construct_protocol::program_block_spans(&markdown).len();
            let pending: HashMap<String, String> = match args.get("pending") {
                Some(value) => serde_json::from_value(value.clone()).map_err(|error| {
                    anyhow!(
                        "invalid `pending` (expected an object of block-index: status): {error}"
                    )
                })?,
                None => HashMap::new(),
            };
            let mut shimmer = vec![false; block_count];
            let mut tooltips = vec![None; block_count];
            for (raw_index, status) in pending {
                let index: usize = raw_index
                    .parse()
                    .map_err(|_| anyhow!("pending block index must be an integer: {raw_index}"))?;
                if index >= block_count {
                    return Err(anyhow!(
                        "pending block index {index} is outside the {block_count}-block document"
                    ));
                }
                if status.trim().is_empty() {
                    return Err(anyhow!("pending block {index} needs a concise status"));
                }
                shimmer[index] = true;
                tooltips[index] = Some(status);
            }
            let params = construct_protocol::ProgramUpdateParams {
                session_id: sid,
                markdown,
                base_version: args.get("base_version").and_then(|v| v.as_u64()),
                actor: construct_protocol::ProgramUpdateActor::Agent,
                template_id: arg_str(&args, "template_id").ok(),
                note: arg_str(&args, "note").ok(),
                shimmer: Some(shimmer),
                shimmer_tooltips: Some(tooltips),
            };
            let result = client.program_update(params).await?;
            let mut out = serde_json::Map::new();
            out.insert("ok".into(), json!(true));
            out.insert("version".into(), json!(result.program.version));
            out.insert(
                "blocks".into(),
                json!(compact_program_blocks(&result.blocks)),
            );
            if let Some(run) = &result.active_run {
                out.insert("run".into(), compact_program_run(run));
            }
            Value::Object(out)
        }
        "construct_program_edit" => {
            let sid = optional_session_arg(&args, session_id)?;
            let before = client.program_get(&sid).await?;
            let edits: Vec<construct_protocol::ProgramEdit> = match args.get("edits") {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| anyhow!("invalid `edits`: {e}"))?,
                None => Vec::new(),
            };
            let pending: HashMap<String, String> = match args.get("pending") {
                Some(v) => serde_json::from_value(v.clone()).map_err(|e| {
                    anyhow!("invalid `pending` (expected an object of block-ref: status): {e}")
                })?,
                None => HashMap::new(),
            };
            let settled: Vec<String> = match args.get("settled") {
                Some(v) => serde_json::from_value(v.clone())
                    .map_err(|e| anyhow!("invalid `settled` (expected block-ref array): {e}"))?,
                None => Vec::new(),
            };
            let settle_others = args
                .get("settle_others")
                .and_then(Value::as_bool)
                .unwrap_or(false);
            if edits.is_empty() && pending.is_empty() && settled.is_empty() && !settle_others {
                return Err(anyhow!(
                    "program edit needs at least one edit or run-status change"
                ));
            }
            for (id, status) in &pending {
                if status.trim().is_empty() {
                    return Err(anyhow!(
                        "pending block {id} needs a concise run-status tooltip"
                    ));
                }
                if settled.contains(id) {
                    return Err(anyhow!("block {id} cannot be both pending and settled"));
                }
            }
            let mut shimmer: Vec<construct_protocol::ProgramShimmerDecl> = pending
                .iter()
                .map(|(id, tooltip)| construct_protocol::ProgramShimmerDecl {
                    id: id.clone(),
                    shimmer: true,
                    tooltip: Some(tooltip.clone()),
                })
                .collect();
            let mut settled_refs: HashSet<String> = settled.into_iter().collect();
            if settle_others {
                settled_refs.extend(
                    before
                        .blocks
                        .iter()
                        .filter(|block| !pending.contains_key(&block.id))
                        .map(|block| block.id.clone()),
                );
            }
            shimmer.extend(settled_refs.into_iter().map(|id| {
                construct_protocol::ProgramShimmerDecl {
                    id,
                    shimmer: false,
                    tooltip: None,
                }
            }));
            let params = construct_protocol::ProgramEditParams {
                session_id: sid.clone(),
                edits,
                actor: construct_protocol::ProgramUpdateActor::Agent,
                note: arg_str(&args, "note").ok(),
                shimmer,
            };
            let result = client.program_edit(params).await?;
            let before_refs: HashSet<&str> = before
                .blocks
                .iter()
                .map(|block| block.id.as_str())
                .collect();
            let changed_blocks: Vec<Value> = result
                .blocks
                .iter()
                .filter(|block| !before_refs.contains(block.id.as_str()))
                .map(|block| {
                    let mut value = json!({
                        "id": block.id,
                        "text": block.text,
                        "pending": block.shimmer,
                    });
                    if let Some(tooltip) = &block.tooltip {
                        value["tooltip"] = json!(tooltip);
                    }
                    value
                })
                .collect();
            let mut response = serde_json::Map::new();
            response.insert("ok".into(), json!(true));
            response.insert("version".into(), json!(result.program.version));
            if result.program.version != before.program.version {
                response.insert("changed".into(), json!(true));
            }
            if !changed_blocks.is_empty() {
                response.insert("blocks".into(), json!(changed_blocks));
            }
            if let Some(run) = result.active_run {
                response.insert(
                    "run".into(),
                    json!({
                        "stage": run.stage,
                        "pending": run.pending_block_count(),
                        "settled": run.settled_block_count,
                        "total": run.total_block_count,
                    }),
                );
            }
            Value::Object(response)
        }
        "construct_program_execute" => {
            let sid = optional_session_arg(&args, session_id)?;
            let before_refs: HashSet<String> = client
                .program_get(&sid)
                .await?
                .blocks
                .into_iter()
                .map(|block| block.id)
                .collect();
            let shimmer: Option<Vec<bool>> = match args.get("shimmer") {
                Some(v) => Some(serde_json::from_value(v.clone()).map_err(|e| {
                    anyhow!("invalid `shimmer` (expected an array of booleans): {e}")
                })?),
                None => None,
            };
            let params = construct_protocol::ProgramExecuteParams {
                session_id: sid,
                selection: arg_str(&args, "selection").ok(),
                base_version: args.get("base_version").and_then(|v| v.as_u64()),
                comment: None,
                shimmer,
                selection_block_ids: None,
                fork: false,
            };
            let result = client.program_execute(params).await?;
            let mut out = serde_json::Map::new();
            out.insert("ok".into(), json!(true));
            out.insert("version".into(), json!(result.program.version));
            let changed: Vec<_> = result
                .blocks
                .iter()
                .filter(|block| !before_refs.contains(&block.id))
                .cloned()
                .collect();
            if !changed.is_empty() {
                out.insert("blocks".into(), json!(compact_program_blocks(&changed)));
            }
            if let Some(run) = &result.active_run {
                out.insert("run".into(), compact_program_run(run));
            }
            Value::Object(out)
        }
        "construct_program_verb_execute" => {
            let sid = optional_session_arg(&args, session_id)?;
            let before_refs: HashSet<String> = client
                .program_get(&sid)
                .await?
                .blocks
                .into_iter()
                .map(|block| block.id)
                .collect();
            let params = construct_protocol::ProgramVerbExecuteParams {
                session_id: sid,
                verb: arg_str(&args, "verb")?,
                selection: arg_str(&args, "selection")?,
                base_version: args.get("base_version").and_then(|v| v.as_u64()),
                comment: arg_str(&args, "comment").ok(),
                selection_block_ids: None,
                run_on_owner: false,
                direct_edit: false,
            };
            let result = client.program_verb_execute(params).await?;
            let changed: Vec<Value> = compact_program_blocks(
                &result
                    .blocks
                    .into_iter()
                    .filter(|block| !before_refs.contains(&block.id))
                    .collect::<Vec<_>>(),
            );
            json!({
                "ok": true,
                "version": result.program.version,
                "subagent_id": result.subagent_session_id,
                "verb": result.verb,
                "blocks": changed,
            })
        }
        "construct_create_session" => {
            let harness = arg_str(&args, "harness")?;
            let cwd = arg_str(&args, "cwd").unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });
            let params = CreateSessionParams {
                harness,
                cwd,
                prompt: arg_str(&args, "prompt").ok(),
                model: arg_str(&args, "model").ok().filter(|s| !s.is_empty()),
                title: arg_str(&args, "title").ok(),
                mode: arg_str(&args, "mode").ok(),
                pty_size: Some(PtySize {
                    cols: 100,
                    rows: 30,
                }),
                worktree: args
                    .get("worktree")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                env: Default::default(),
                args: Vec::new(),
                kind: construct_protocol::SessionKind::User,
                parent_session_id: None,
                group_id: arg_str(&args, "group_id").ok(),
                position_after_session_id: None,
                forked_from: None,
            };
            let sid = client.create(params).await?;
            json!({ "session_id": sid })
        }
        "construct_fork_session" => {
            let source = arg_str(&args, "source_session_id")?;
            let harness = arg_str(&args, "harness")?;
            let opts = construct_client::ForkOptions {
                model: arg_str(&args, "model").ok(),
                prompt: arg_str(&args, "prompt").ok(),
                seed: args.get("seed").and_then(|v| v.as_bool()).unwrap_or(true),
                pty_size: None,
                ..Default::default()
            };
            let sid = client.fork_session(&source, &harness, opts).await?;
            json!({ "session_id": sid })
        }
        "construct_merge_session" => {
            let sid = arg_str(&args, "session_id")?;
            let mode_str = arg_str(&args, "mode")?;
            let mode = match mode_str.as_str() {
                "result" => construct_protocol::ForkMergeMode::Result,
                "discard" => construct_protocol::ForkMergeMode::Discard,
                _ => anyhow::bail!("invalid merge mode"),
            };
            client.merge(&sid, mode).await?;
            json!({ "ok": true })
        }
        "construct_send_input" => {
            let sid = arg_str(&args, "session_id")?;
            let text = arg_str(&args, "text")?;
            client.send_input(&sid, text).await?;
            json!({ "ok": true })
        }
        "construct_send_keys" => {
            let sid = arg_str(&args, "session_id")?;
            let b64 = arg_str(&args, "bytes_b64")?;
            let bytes = base64::engine::general_purpose::STANDARD.decode(b64.as_bytes())?;
            client.pty_input(&sid, bytes).await?;
            json!({ "ok": true })
        }
        "construct_interrupt_session" => {
            client.interrupt(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "construct_stop_session" => {
            client.stop(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "construct_kill_session" => {
            client.kill(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "construct_delete_session" => {
            client.delete(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "construct_archive_session" => {
            client.archive(&arg_str(&args, "session_id")?).await?;
            json!({ "ok": true })
        }
        "construct_pin_session" => {
            let sid = arg_str(&args, "session_id")?;
            let pinned = args
                .get("pinned")
                .and_then(|v| v.as_bool())
                .ok_or_else(|| anyhow!("missing or non-bool `pinned`"))?;
            client.set_pinned(&sid, pinned).await?;
            json!({ "ok": true })
        }
        "construct_rename_session" => {
            let sid = arg_str(&args, "session_id")?;
            let title = arg_str(&args, "title")
                .ok()
                .filter(|s| !s.trim().is_empty());
            client.set_title(&sid, title).await?;
            json!({ "ok": true })
        }
        "construct_set_session_group" => {
            let sid = arg_str(&args, "session_id")?;
            let group_id = match args.get("group_id") {
                Some(serde_json::Value::Null) | None => None,
                Some(serde_json::Value::String(s)) => Some(s.clone()),
                Some(_) => return Err(anyhow!("`group_id` must be a string or null")),
            };
            let position = match args
                .get("position")
                .and_then(|v| v.as_str())
                .unwrap_or("bottom")
            {
                "top" => construct_protocol::SessionGroupPosition::Top,
                "bottom" => construct_protocol::SessionGroupPosition::Bottom,
                other => {
                    return Err(anyhow!(
                        "`position` must be \"top\" or \"bottom\", got {other:?}"
                    ))
                }
            };
            client.set_session_group(&sid, group_id, position).await?;
            json!({ "ok": true })
        }
        "construct_move_session" => {
            let sid = arg_str(&args, "session_id")?;
            let direction = match arg_str(&args, "direction")?.as_str() {
                "up" => construct_protocol::MoveDirection::Up,
                "down" => construct_protocol::MoveDirection::Down,
                other => {
                    return Err(anyhow!(
                        "`direction` must be \"up\" or \"down\", got {other:?}"
                    ))
                }
            };
            let moved = client.move_session(&sid, direction).await?;
            json!({ "ok": true, "moved": moved })
        }
        "construct_subagent_create" => {
            let parent_id = require_session_id(session_id)?;
            let harness = arg_str(&args, "harness")?;
            let cwd = arg_str(&args, "cwd").unwrap_or_else(|_| {
                std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string())
            });
            let mut env = HashMap::new();
            env.insert("CONSTRUCT_PARENT_SESSION_ID".to_string(), parent_id.clone());
            let params = CreateSessionParams {
                title: arg_str(&args, "title")
                    .ok()
                    .or_else(|| Some(format!("subagent:{harness}"))),
                harness,
                cwd,
                prompt: arg_str(&args, "prompt").ok(),
                model: arg_str(&args, "model").ok().filter(|s| !s.is_empty()),
                mode: Some(arg_str(&args, "mode").unwrap_or_else(|_| "headless".to_string())),
                pty_size: Some(PtySize {
                    cols: 100,
                    rows: 30,
                }),
                worktree: args
                    .get("worktree")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false),
                env,
                args: Vec::new(),
                kind: construct_protocol::SessionKind::Subagent,
                parent_session_id: Some(parent_id),
                group_id: None,
                position_after_session_id: None,
                forked_from: None,
            };
            let sid = client.create(params).await?;
            json!({ "subagent_id": sid })
        }
        "construct_subagent_list" => {
            let parent_id = require_session_id(session_id)?;
            let subagents: Vec<_> = client
                .list()
                .await?
                .into_iter()
                .filter(|s| {
                    s.kind == construct_protocol::SessionKind::Subagent
                        && s.parent_session_id.as_deref() == Some(parent_id.as_str())
                })
                .collect();
            json!({
                "subagents": subagents
                    .iter()
                    .map(|summary| compact_session_summary(summary, None))
                    .collect::<Vec<_>>()
            })
        }
        "construct_subagent_peek" => {
            let sid = arg_str(&args, "subagent_id")?;
            let detail = owned_subagent_detail(client, session_id, &sid).await?;
            if detail.summary.has_pty {
                let max_bytes = args
                    .get("max_bytes")
                    .and_then(Value::as_u64)
                    .unwrap_or(DEFAULT_PTY_BYTES as u64)
                    .min(MAX_MCP_PTY_BYTES as u64) as usize;
                let before_offset = args.get("before_offset").and_then(Value::as_u64);
                let snap = client
                    .pty_replay_range(&sid, max_bytes, before_offset)
                    .await?;
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(&snap.data)
                    .unwrap_or_default();
                let text = if args.get("mode").and_then(Value::as_str) == Some("text") {
                    String::from_utf8_lossy(&bytes).to_string()
                } else {
                    render_terminal_text(&bytes, snap.size.clone())
                };
                json!({
                    "summary": compact_session_summary(&detail.summary, None),
                    "output": text,
                    "start_offset": snap.start_offset,
                    "end_offset": snap.end_offset,
                    "total_bytes": snap.total_bytes,
                    "has_older": snap.start_offset > 0,
                })
            } else {
                let limit = args
                    .get("limit")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .min(MAX_TRANSCRIPT_EVENTS as u64) as usize;
                let start = detail.events.len().saturating_sub(limit);
                let full = args.get("detail").and_then(Value::as_str) == Some("full");
                let events: Vec<Value> = detail.events[start..]
                    .iter()
                    .map(|event| {
                        if full {
                            serde_json::to_value(event).unwrap_or_else(|_| json!({}))
                        } else {
                            compact_transcript_event(event)
                        }
                    })
                    .collect();
                json!({
                    "summary": compact_session_summary(&detail.summary, None),
                    "events": events,
                })
            }
        }
        "construct_subagent_enqueue" => {
            let sid = arg_str(&args, "subagent_id")?;
            owned_subagent_detail(client, session_id, &sid).await?;
            client.send_input(&sid, arg_str(&args, "text")?).await?;
            json!({ "ok": true })
        }
        "construct_subagent_cancel" => {
            let sid = arg_str(&args, "subagent_id")?;
            owned_subagent_detail(client, session_id, &sid).await?;
            client.interrupt(&sid).await?;
            json!({ "ok": true })
        }
        "construct_subagent_delete" => {
            let sid = arg_str(&args, "subagent_id")?;
            owned_subagent_detail(client, session_id, &sid).await?;
            client.delete(&sid).await?;
            json!({ "ok": true })
        }
        "construct_subagent_archive" => {
            let sid = arg_str(&args, "subagent_id")?;
            owned_subagent_detail(client, session_id, &sid).await?;
            client.archive(&sid).await?;
            json!({ "ok": true })
        }
        "webui_hot_reload" => {
            let dir = args.get("dir").and_then(|v| v.as_str()).map(String::from);
            let res = client.dev_set_assets(dir).await?;
            json!({ "dir": res.dir, "embedded": res.dir.is_none() })
        }
        other => return Err(anyhow!("unknown tool: {other}")),
    };

    // Per MCP, `tools/call` returns a `content` array. Surface the JSON
    // result as a single text block — the LLM parses it.
    let text = serde_json::to_string(&result_json)?;
    Ok(json!({
        "content": [{ "type": "text", "text": text }],
        "isError": false,
    }))
}

fn arg_str(args: &Value, name: &str) -> Result<String> {
    args.get(name)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
        .ok_or_else(|| anyhow!("missing or non-string `{name}`"))
}

fn arg_usize(args: &Value, name: &str) -> Option<usize> {
    args.get(name).and_then(|v| v.as_u64()).map(|n| n as usize)
}

/// Parse an optional array-of-strings arg into [`construct_protocol::SearchScope`]s.
/// Unrecognized entries are ignored rather than erroring, so a caller passing
/// e.g. a stray plural ("names") degrades to "search everything" instead of
/// failing the whole call.
fn arg_scopes(args: &Value, name: &str) -> Option<Vec<construct_protocol::SearchScope>> {
    let arr = args.get(name).and_then(|v| v.as_array())?;
    let scopes: Vec<construct_protocol::SearchScope> = arr
        .iter()
        .filter_map(|v| v.as_str())
        .filter_map(|s| match s {
            "name" => Some(construct_protocol::SearchScope::Name),
            "program" => Some(construct_protocol::SearchScope::Program),
            "transcript" => Some(construct_protocol::SearchScope::Transcript),
            _ => None,
        })
        .collect();
    if scopes.is_empty() {
        None
    } else {
        Some(scopes)
    }
}

fn require_session_id(session_id: Option<&str>) -> Result<String> {
    session_id
        .map(ToOwned::to_owned)
        .ok_or_else(|| anyhow!("subagent tools require CONSTRUCT_SESSION_ID"))
}

fn optional_session_arg(args: &Value, session_id: Option<&str>) -> Result<String> {
    arg_str(args, "session_id").or_else(|_| require_session_id(session_id))
}

async fn owned_subagent_detail(
    client: &Arc<Client>,
    session_id: Option<&str>,
    subagent_id: &str,
) -> Result<construct_protocol::SessionDetail> {
    let parent_id = require_session_id(session_id)?;
    let detail = client.get(subagent_id).await?;
    if detail.summary.kind != construct_protocol::SessionKind::Subagent {
        return Err(anyhow!("{subagent_id} is not a subagent"));
    }
    if detail.summary.parent_session_id.as_deref() != Some(parent_id.as_str()) {
        return Err(anyhow!("{subagent_id} is not owned by this session"));
    }
    Ok(detail)
}

#[cfg(test)]
mod tests {
    use super::*;
    use construct_protocol::{ipc_method, SessionKind, SessionState};
    use tokio::io::{AsyncWriteExt, BufReader};
    use tokio::net::UnixListener;

    #[test]
    fn bounded_helpers_preserve_signal_and_report_truncation() {
        let (short, truncated) = truncate_chars("hello", 10);
        assert_eq!(short, "hello");
        assert!(!truncated);
        let (unicode, truncated) = truncate_chars("a🙂bc", 2);
        assert_eq!(unicode, "a🙂…");
        assert!(truncated);

        let stats =
            diff_stats("diff --git a/a.rs b/a.rs\n--- a/a.rs\n+++ b/a.rs\n-old\n+new\n+more\n");
        assert_eq!(stats["files"], json!(["a.rs"]));
        assert_eq!(stats["additions"], 2);
        assert_eq!(stats["deletions"], 1);
    }

    #[test]
    fn compact_program_blocks_omit_duplicate_text_and_legacy_ids() {
        let blocks = compact_program_blocks(&[construct_protocol::ProgramBlockView {
            id: "b0:0".into(),
            block_id: "b0".into(),
            content_epoch: 0,
            block_ref: "b0:0".into(),
            content_id: "legacy".into(),
            start_line: 2,
            end_line: 3,
            text: "large duplicate text".into(),
            shimmer: true,
            tooltip: Some("Building".into()),
        }]);
        assert_eq!(blocks[0]["id"], "b0:0");
        assert_eq!(blocks[0]["pending"], true);
        assert!(blocks[0].get("text").is_none());
        assert!(blocks[0].get("block_ref").is_none());
        assert!(blocks[0].get("content_id").is_none());
    }

    #[test]
    fn oversized_transcript_event_becomes_bounded_preview() {
        let event: construct_protocol::TimestampedEvent = serde_json::from_value(json!({
            "seq": 7,
            "at": "2026-07-14T00:00:00Z",
            "event": {
                "type": "tool_result",
                "tool": "exec",
                "ok": true,
                "output": "x".repeat(MAX_COMPACT_EVENT_CHARS * 2),
                "call_id": "c1"
            }
        }))
        .unwrap();
        let compact = compact_transcript_event(&event);
        assert_eq!(compact["seq"], 7);
        assert_eq!(compact["event"]["truncated"], true);
        assert!(serde_json::to_string(&compact).unwrap().len() < MAX_COMPACT_EVENT_CHARS + 500);
    }

    #[test]
    fn context_serve_state_omits_repeat_content_across_calls() {
        // The dedup logic itself is covered in construct_protocol::agent_context;
        // this pins the MCP dispatcher wiring: one process-level state, mutated
        // across calls.
        let make_context = || agent_context::AgentdContext {
            session_id: Some("s1".into()),
            project_id: Some("p1".into()),
            instructions: vec!["act".into()],
            memory_policy: vec!["long memory reference".into()],
            widget_policy: vec!["long widget reference".into()],
            markdown_extensions: Vec::new(),
            global_memory: Some(agent_context::MemoryFile {
                path: "/tmp/global.md".into(),
                content: "remember this".into(),
                truncated: false,
                remaining_bytes: 0,
            }),
            project_memory: None,
            session_widgets: None,
            program_run: None,
        };
        let state = ContextServeState::default();
        let request = agent_context::ContextRequest::from_args(&json!({}));
        let first = agent_context::compact_response(
            make_context(),
            &request,
            &mut state.lock().unwrap(),
        );
        assert_eq!(first["global_memory"]["content"], "remember this");
        assert!(first.get("memory_policy").is_none());

        let second = agent_context::compact_response(
            make_context(),
            &request,
            &mut state.lock().unwrap(),
        );
        assert_eq!(second["global_memory"]["unchanged"], true);
        assert!(second["global_memory"].get("content").is_none());
    }

    #[test]
    fn terminal_render_removes_escape_sequences() {
        let text = render_terminal_text(
            b"\x1b[31mred\x1b[0m",
            Some(construct_protocol::PtySize { cols: 20, rows: 2 }),
        );
        assert_eq!(text, "red");
    }

    #[test]
    fn catalog_includes_browser_tools() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
            })
            .collect();

        for expected in [
            "browser_open",
            "browser_inspect",
            "browser_screenshot",
            "browser_eval",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn catalog_includes_construct_context_tool() {
        let tools = catalog();
        let context = tools
            .iter()
            .find(|tool| {
                tool.get("name").and_then(|name| name.as_str()) == Some("construct_context")
            })
            .expect("missing construct_context");

        assert!(context
            .get("description")
            .and_then(|description| description.as_str())
            .unwrap_or_default()
            .contains("omit unchanged and already-served content"));
    }

    #[test]
    fn catalog_does_not_advertise_agentd_prefixed_tools() {
        let agentd_names: Vec<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .filter(|name| name.starts_with("agentd_"))
                    .map(|name| name.to_string())
            })
            .collect();

        assert!(
            agentd_names.is_empty(),
            "catalog advertised old agentd-prefixed tools: {agentd_names:?}"
        );
    }

    #[test]
    fn catalog_includes_subagent_tools() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
            })
            .collect();

        for expected in [
            "construct_subagent_create",
            "construct_subagent_list",
            "construct_subagent_peek",
            "construct_subagent_enqueue",
            "construct_subagent_cancel",
            "construct_subagent_delete",
            "construct_subagent_archive",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn catalog_includes_session_lifecycle_tools() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
            })
            .collect();

        // Archive is the soft/reversible sibling of delete; both must be advertised.
        for expected in ["construct_delete_session", "construct_archive_session"] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn catalog_includes_program_tools() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|tool| {
                tool.get("name")
                    .and_then(|name| name.as_str())
                    .map(|name| name.to_string())
            })
            .collect();

        for expected in [
            "construct_program_get",
            "construct_program_list_templates",
            "construct_program_update",
            "construct_program_edit",
            "construct_program_execute",
            "construct_program_list_verbs",
            "construct_program_verb_execute",
        ] {
            assert!(names.contains(expected), "missing {expected}");
        }
    }

    #[test]
    fn program_edit_tool_guides_moves_into_one_call() {
        let tool = catalog()
            .into_iter()
            .find(|tool| {
                tool.get("name").and_then(|name| name.as_str()) == Some("construct_program_edit")
            })
            .expect("program edit tool");
        let description = tool
            .get("description")
            .and_then(|description| description.as_str())
            .expect("description");
        let edits_description = tool
            .pointer("/inputSchema/properties/edits/description")
            .and_then(|description| description.as_str())
            .expect("edits description");

        assert!(description.contains("ONE call"));
        assert!(
            edits_description.contains("For moves"),
            "edits schema description should reinforce atomic move edits"
        );
        let schema = tool.get("inputSchema").expect("input schema");
        assert!(schema.pointer("/properties/pending").is_some());
        assert!(schema.pointer("/properties/settled").is_some());
        assert!(schema.pointer("/properties/settle_others").is_some());
        assert!(schema.pointer("/properties/shimmer").is_none());
        assert!(
            schema.get("required").is_none(),
            "status-only calls need no edits"
        );
    }

    #[test]
    fn catalog_exposes_bounded_compact_read_controls() {
        let tools = catalog();
        let find = |name: &str| {
            tools
                .iter()
                .find(|tool| tool.get("name").and_then(Value::as_str) == Some(name))
                .unwrap_or_else(|| panic!("missing {name}"))
        };

        assert!(find("construct_get_output")
            .pointer("/inputSchema/properties/max_bytes")
            .is_some());
        assert!(find("construct_get_transcript")
            .pointer("/inputSchema/properties/tail")
            .is_some());
        assert!(find("construct_get_diff")
            .pointer("/inputSchema/properties/max_chars")
            .is_some());
        assert!(find("construct_program_get")
            .pointer("/inputSchema/properties/revisions")
            .is_some());
        assert!(find(CONTEXT_TOOL_NAME)
            .pointer("/inputSchema/properties/refresh")
            .is_some());
        assert!(find(CONTEXT_TOOL_NAME)
            .pointer("/inputSchema/properties/skip_memory")
            .is_some());

        let update = find("construct_program_update");
        assert!(update.pointer("/inputSchema/properties/pending").is_some());
        assert!(update.pointer("/inputSchema/properties/shimmer").is_none());
        assert!(update.pointer("/inputSchema/properties/tooltips").is_none());
    }

    #[test]
    fn webui_hot_reload_tool_is_debug_only() {
        let names: std::collections::HashSet<String> = catalog()
            .into_iter()
            .filter_map(|t| t.get("name").and_then(|n| n.as_str()).map(String::from))
            .collect();
        // Tests run in debug, so the dev-only tool is advertised here;
        // it's `#[cfg(debug_assertions)]`-gated out of release builds.
        assert_eq!(names.contains("webui_hot_reload"), cfg!(debug_assertions));
    }

    #[tokio::test]
    async fn subagent_tools_flow_through_mcp_with_parent_scope() {
        let dir = std::env::temp_dir().join(format!(
            "construct-mcp-subagent-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("test dir");
        let sock = dir.join("construct.sock");
        let listener = UnixListener::bind(&sock).expect("bind mock daemon");
        let server = tokio::spawn(async move {
            let (stream, _) = listener.accept().await.expect("accept client");
            let (reader, mut writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut created = subagent_summary("ssub", "sparent", false);
            loop {
                let raw = match construct_protocol::transport::read_message(&mut reader).await {
                    Ok(Some(raw)) => raw,
                    _ => break,
                };
                let id = raw.get("id").cloned().unwrap_or_else(|| json!(0));
                let method = raw.get("method").and_then(|m| m.as_str()).unwrap_or("");
                let params = raw.get("params").cloned().unwrap_or_else(|| json!({}));
                let result = match method {
                    ipc_method::SESSION_CREATE => {
                        let p: CreateSessionParams =
                            serde_json::from_value(params).expect("create params");
                        assert_eq!(p.kind, SessionKind::Subagent);
                        assert_eq!(p.parent_session_id.as_deref(), Some("sparent"));
                        assert_eq!(
                            p.env.get("CONSTRUCT_PARENT_SESSION_ID").map(String::as_str),
                            Some("sparent")
                        );
                        assert_eq!(p.mode.as_deref(), Some("headless"));
                        assert_eq!(p.harness, "codex");
                        json!({ "session_id": "ssub" })
                    }
                    ipc_method::SESSION_LIST => json!([
                        created.clone(),
                        subagent_summary("sother", "other-parent", false),
                    ]),
                    ipc_method::SESSION_GET => {
                        let sid = params
                            .get("session_id")
                            .and_then(|s| s.as_str())
                            .expect("session_id");
                        let summary = match sid {
                            "ssub" => created.clone(),
                            "sother" => subagent_summary("sother", "other-parent", false),
                            other => panic!("unexpected get {other}"),
                        };
                        json!({
                            "summary": summary,
                            "events": [
                                {
                                    "seq": 1,
                                    "at": "2026-05-24T00:00:00Z",
                                    "event": {
                                        "type": "message",
                                        "role": "assistant",
                                        "text": "done"
                                    }
                                }
                            ]
                        })
                    }
                    ipc_method::SESSION_INPUT => {
                        assert_eq!(
                            params.get("session_id").and_then(|s| s.as_str()),
                            Some("ssub")
                        );
                        assert_eq!(
                            params.get("text").and_then(|s| s.as_str()),
                            Some("continue")
                        );
                        json!(null)
                    }
                    ipc_method::SESSION_INTERRUPT => {
                        assert_eq!(
                            params.get("session_id").and_then(|s| s.as_str()),
                            Some("ssub")
                        );
                        json!(null)
                    }
                    ipc_method::SESSION_ARCHIVE => {
                        assert_eq!(
                            params.get("session_id").and_then(|s| s.as_str()),
                            Some("ssub")
                        );
                        // Archive is soft: flip the flag, keep the record + events.
                        created.archived = true;
                        created.state = SessionState::Done;
                        json!(null)
                    }
                    ipc_method::SESSION_DELETE => {
                        assert_eq!(
                            params.get("session_id").and_then(|s| s.as_str()),
                            Some("ssub")
                        );
                        created.state = SessionState::Done;
                        json!(null)
                    }
                    other => panic!("unexpected method {other}"),
                };
                let resp = json!({ "jsonrpc": "2.0", "id": id, "result": result });
                let line = resp.to_string() + "\n";
                writer.write_all(line.as_bytes()).await.expect("write resp");
            }
        });

        let client = Client::connect(&sock).await.expect("connect mock daemon");
        let created = call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_create",
            json!({
                "harness": "codex",
                "prompt": "summarize",
            }),
        )
        .await;
        assert_eq!(created["subagent_id"], "ssub");

        let listed = call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_list",
            json!({}),
        )
        .await;
        assert_eq!(listed["subagents"].as_array().expect("subagents").len(), 1);
        assert_eq!(listed["subagents"][0]["id"], "ssub");

        let peeked = call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_peek",
            json!({ "subagent_id": "ssub" }),
        )
        .await;
        assert_eq!(peeked["events"][0]["event"]["text"], "done");

        call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_enqueue",
            json!({ "subagent_id": "ssub", "text": "continue" }),
        )
        .await;
        call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_cancel",
            json!({ "subagent_id": "ssub" }),
        )
        .await;

        // Archive is the soft, reversible counterpart of delete: it flips the
        // `archived` flag on the daemon side but leaves the record + transcript
        // in place. A peek afterwards still returns the subagent and its events.
        call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_archive",
            json!({ "subagent_id": "ssub" }),
        )
        .await;
        let after_archive = call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_peek",
            json!({ "subagent_id": "ssub" }),
        )
        .await;
        assert_eq!(after_archive["summary"]["archived"], true);
        assert_eq!(after_archive["events"][0]["event"]["text"], "done");

        call_tool(
            &client,
            Some("sparent"),
            "construct_subagent_delete",
            json!({ "subagent_id": "ssub" }),
        )
        .await;

        let blocked = call(
            &client,
            Some("sparent"),
            &ContextServeState::default(),
            json!({
                "name": "construct_subagent_peek",
                "arguments": { "subagent_id": "sother" }
            }),
        )
        .await
        .expect_err("ownership error should fail direct dispatcher calls");
        assert!(
            blocked.to_string().contains("not owned by this session"),
            "unexpected error: {blocked}"
        );

        let legacy_name = call(
            &client,
            Some("sparent"),
            &ContextServeState::default(),
            json!({
                "name": "agentd_subagent_peek",
                "arguments": { "subagent_id": "ssub" }
            }),
        )
        .await
        .expect_err("legacy agentd-prefixed tool names should not be accepted");
        assert!(
            legacy_name
                .to_string()
                .contains("unknown tool: agentd_subagent_peek"),
            "unexpected error: {legacy_name}"
        );

        drop(client);
        server.await.expect("server task");
    }

    async fn call_tool(
        client: &Arc<Client>,
        session_id: Option<&str>,
        name: &str,
        arguments: Value,
    ) -> Value {
        let response = call(
            client,
            session_id,
            &ContextServeState::default(),
            json!({ "name": name, "arguments": arguments }),
        )
        .await
        .expect("tool call");
        assert_eq!(response["isError"], false, "{response:?}");
        let text = response["content"][0]["text"]
            .as_str()
            .expect("text result");
        serde_json::from_str(text).expect("json tool result")
    }

    fn subagent_summary(
        id: &str,
        parent: &str,
        has_pty: bool,
    ) -> construct_protocol::SessionSummary {
        construct_protocol::SessionSummary {
            id: id.to_string(),
            harness: "codex".to_string(),
            cwd: "/tmp".to_string(),
            title: None,
            state: SessionState::Running,
            created_at: "2026-05-24T00:00:00Z".parse().expect("timestamp"),
            last_event_at: None,
            cost_usd: None,
            model: None,
            effort: None,
            worktree: None,
            pending_input: false,
            last_prompt: None,
            event_count: 1,
            has_pty,
            mode: Some("headless".to_string()),
            pinned: false,
            position: 0,
            group_id: None,
            parent_session_id: Some(parent.to_string()),
            native_subagent: None,
            last_pty_at_ms: None,
            busy_ms: 0,
            busy_running_since_ms: None,
            message_count: 0,
            tokens: construct_protocol::TokenTally::default(),
            approval_mode: construct_protocol::ApprovalMode::Manual,
            kind: SessionKind::Subagent,
            archived: false,
            operator_loop_disabled: false,
            needs_attention: false,
            forked_from: None,
            merge: None,
        }
    }
}
