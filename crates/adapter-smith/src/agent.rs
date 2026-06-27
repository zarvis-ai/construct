//! The smith agent loop. Pulls user input from the inbox, calls the
//! provider, runs any tool calls (gating Risky ones behind an approval
//! prompt unless unsafe-auto is on), feeds results back, and loops until
//! the model signals end-of-turn.

use crate::context;
use crate::persist::{self, Persist};
use crate::provider::{self, Content, LlmProvider, Message, Role, StopReason, TextSink, ToolCall};
use crate::tools::{truncate_for_model, ToolCtx, ToolOutcome, ToolRegistry};
use agentd_protocol::adapter::{AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{MessageRole, SessionEvent, SessionStartParams, SessionState, ToolRisk};
use anyhow::{anyhow, Context, Result};
use serde_json::json;
use std::collections::{BTreeMap, VecDeque};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AutoReviewResult {
    Approve,
    Deny,
    AskUser,
}

#[derive(Debug, Clone)]
pub struct ApprovalHistoryEntry {
    pub decision: String,
    pub tool: String,
    pub args_summary: String,
}

#[derive(Debug, Clone)]
pub struct AutoReviewContext {
    pub cwd: String,
    pub current_task: Option<String>,
    pub recent_approvals: Vec<ApprovalHistoryEntry>,
}

impl AutoReviewContext {
    fn format_for_prompt(&self) -> String {
        let mut out = String::new();
        out.push_str("Current task:\n");
        out.push_str(self.current_task.as_deref().unwrap_or("(unknown)"));
        out.push_str("\n\nCWD:\n");
        out.push_str(&self.cwd);
        out.push_str("\n\nRecent approval decisions:\n");
        if self.recent_approvals.is_empty() {
            out.push_str("(none)\n");
        } else {
            for entry in self.recent_approvals.iter().rev() {
                out.push_str("- ");
                out.push_str(&entry.decision);
                out.push_str(": ");
                out.push_str(&entry.tool);
                out.push('(');
                out.push_str(&entry.args_summary);
                out.push_str(")\n");
            }
        }
        out
    }
}

const APPROVAL_HISTORY_LIMIT: usize = 20;

/// Consecutive tool-call rounds with no successful tool result before the
/// harness nudges the model to change approach instead of thrashing.
/// Purely a function of observed progress — model- and provider-agnostic.
const NONPRODUCTIVE_STREAK_LIMIT: usize = 4;
const GROK_BASE_URL: &str = "https://api.x.ai/v1";

fn record_approval_history(
    history: &mut VecDeque<ApprovalHistoryEntry>,
    decision: impl Into<String>,
    tool: impl Into<String>,
    args_summary: impl Into<String>,
) {
    history.push_back(ApprovalHistoryEntry {
        decision: decision.into(),
        tool: tool.into(),
        args_summary: args_summary.into(),
    });
    while history.len() > APPROVAL_HISTORY_LIMIT {
        history.pop_front();
    }
}

struct NullSink;
impl TextSink for NullSink {
    fn delta(&mut self, _text: &str) {}
}

const AUTO_REVIEW_SYSTEM_PROMPT: &str = r#"You are an approval reviewer for a local coding agent.
Decide whether a pending risky tool call is reasonable, bounded, and clearly related to the current task.
Return ONLY compact JSON: {"decision":"approve"|"ask_user","reason":"..."}.
You can only approve or defer to the user; you cannot reject an action yourself, so when in any doubt, ask.

Use task context and recent user approval decisions as preference signals for similar actions, not as blanket permission.

Approve routine, expected development actions in the current task when they are bounded to the repo or current working directory. Examples include:
- file edits inside the active git worktree for the requested task, including source, tests, docs, configs, fixtures, and generated assets that are normally committed
- build, format, lint, and test loops for the active project
- inspection commands (git history/status/diff views, search/list, and similar read-oriented discovery like `rg`, `ls`, `find`, `sed -n`, etc.)
- ordinary repo hygiene commands that are narrow in scope and explicitly anchored to task-relevant files
- creating, updating, reading, or removing files inside the session widget directory shown below — the agent's own session-UI scratch space, which it is expected to maintain

Do not ask the user merely because a shell command chains routine steps with `&&`.
Do not ask the user merely because an edit changes files in the active git worktree; git makes those changes inspectable and reversible.

Never approve clearly destructive, secret-exfiltrating, unrelated, or user-hostile actions — ask the user, who makes the final call to reject.
Also ask the user when context is insufficient, prior decisions conflict, the action targets broad/unscoped paths, touches secrets or credentials, mutates outside the active git worktree, changes user/home/system files, removes large amounts of data, or is ambiguous enough that a reasonable reviewer could not tell what will change."#;

pub async fn auto_review_for_adapter(
    provider: &dyn LlmProvider,
    model: &str,
    tool: &str,
    args_summary: &str,
    input: &serde_json::Value,
    ctx: &AutoReviewContext,
) -> AutoReviewResult {
    if is_auto_review_routine_shell_command(tool, input) {
        return AutoReviewResult::Approve;
    }

    // Tell the reviewer where this session's widget directory is, so file
    // operations bounded to it (which the agent is expected to maintain) read
    // as routine rather than ambiguous. `edit_file` writes there are already
    // auto-approved deterministically via the auto-approve policy; this covers
    // the residual cases that still reach the reviewer (e.g. shell reads or
    // removals of widget files).
    let widgets_hint = match std::env::var(agentd_protocol::agent_context::ENV_SESSION_WIDGETS_DIR)
    {
        Ok(dir) if !dir.is_empty() => format!("\n\nSession widget directory:\n{dir}"),
        _ => String::new(),
    };
    let user = format!(
        "{}{widgets_hint}\n\nPending tool:\nTool: {tool}\nArgs summary:\n{args_summary}",
        ctx.format_for_prompt()
    );
    let messages = [Message {
        role: Role::User,
        content: Content::Text { text: user },
    }];
    let mut sink = NullSink;
    let Ok(turn) = provider
        .complete(model, AUTO_REVIEW_SYSTEM_PROMPT, &messages, &[], &mut sink)
        .await
    else {
        return AutoReviewResult::AskUser;
    };
    let Some(text) = turn.text else {
        return AutoReviewResult::AskUser;
    };
    parse_auto_review_decision(&text)
}

fn parse_auto_review_decision(text: &str) -> AutoReviewResult {
    let trimmed = text.trim();
    let parsed: serde_json::Value = serde_json::from_str(trimmed)
        .or_else(|_| {
            let start = trimmed.find('{').unwrap_or(0);
            let end = trimmed.rfind('}').map(|i| i + 1).unwrap_or(trimmed.len());
            serde_json::from_str(&trimmed[start..end])
        })
        .unwrap_or(serde_json::Value::Null);
    match parsed.get("decision").and_then(|v| v.as_str()) {
        Some("approve") => AutoReviewResult::Approve,
        Some("deny") => AutoReviewResult::Deny,
        _ => AutoReviewResult::AskUser,
    }
}

fn is_auto_review_routine_shell_command(tool: &str, input: &serde_json::Value) -> bool {
    if tool != "shell" {
        return false;
    }
    if input
        .get("interactive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        return false;
    }
    let Some(command) = input.get("command").and_then(|c| c.as_str()) else {
        return false;
    };
    if contains_unsafe_shell_syntax(command) {
        return false;
    }
    command
        .split("&&")
        .all(|segment| is_routine_dev_shell_segment(segment.trim()))
}

fn contains_unsafe_shell_syntax(command: &str) -> bool {
    command.contains('|')
        || command.contains(';')
        || command.contains('>')
        || command.contains('<')
        || command.contains("$(")
        || command.contains('`')
        || command.contains('\n')
}

fn is_routine_dev_shell_segment(segment: &str) -> bool {
    let tokens: Vec<&str> = segment
        .split_whitespace()
        .map(str::trim)
        .filter(|token| !token.is_empty())
        .collect();
    if tokens.is_empty() {
        return false;
    }

    let mut i = 0;
    while i < tokens.len() && is_env_assignment(tokens[i]) {
        i += 1;
    }
    if i >= tokens.len() {
        return false;
    }

    match tokens[i] {
        "cd" => true,
        "ls" | "rg" | "grep" | "head" | "tail" | "wc" => true,
        "git" => is_routine_git_segment(&tokens[i + 1..]),
        "cargo" => is_routine_cargo_segment(&tokens[i + 1..]),
        "sed" => is_routine_sed_segment(&tokens[i + 1..]),
        "cat" => true,
        _ => false,
    }
}

fn is_env_assignment(token: &str) -> bool {
    token.contains('=') && !token.starts_with('-')
}

fn is_routine_git_segment(rest: &[&str]) -> bool {
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--" {
            i += 1;
            break;
        }
        if rest[i].starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    matches!(
        rest.get(i),
        Some(&"status")
            | Some(&"log")
            | Some(&"diff")
            | Some(&"show")
            | Some(&"fetch")
            | Some(&"pull")
            | Some(&"branch")
            | Some(&"ls-files")
            | Some(&"ls-tree")
            | Some(&"rev-parse")
            | Some(&"remote")
    )
}

fn is_routine_cargo_segment(rest: &[&str]) -> bool {
    let mut i = 0;
    while i < rest.len() {
        if rest[i] == "--" {
            i += 1;
            break;
        }
        if rest[i].starts_with('-') {
            i += 1;
            continue;
        }
        break;
    }
    matches!(
        rest.get(i),
        Some(&"build")
            | Some(&"check")
            | Some(&"test")
            | Some(&"fmt")
            | Some(&"clippy")
            | Some(&"doc")
            | Some(&"metadata")
    )
}

fn is_routine_sed_segment(rest: &[&str]) -> bool {
    !rest.iter().any(|arg| *arg == "-i" || *arg == "--in-place")
}

/// Sink that pushes each assistant delta as a `SessionEvent::Message`.
/// Used by the headless agent loop.
pub struct MessageSink<'a> {
    pub emit: &'a EventEmitter,
}
impl<'a> TextSink for MessageSink<'a> {
    fn delta(&mut self, text: &str) {
        self.emit.emit(SessionEvent::Message {
            role: MessageRole::Assistant,
            text: text.to_string(),
        });
    }
    fn reasoning_delta(&mut self, text: &str) {
        self.emit.emit(SessionEvent::Reasoning {
            text: text.to_string(),
        });
    }
}

pub(crate) const SYSTEM_PROMPT_USER: &str = r#"You are smith, an AI agent embedded in agentd (a multi-session terminal agent fleet).

You have access to:
- Local tools: shell (run any command — read files with `cat`/`sed -n`, search with `rg`/`grep`, list with `ls`, run tests, git), edit_file (apply one or many find/replace hunks across files; also creates files), write_stdin (drive an interactive process started by `shell interactive:true`).
- Agentd-control tools (prefix `agentd_`) for inspecting and steering other agentd sessions running on this host.
- Subagent tools (`agentd_subagent_*`) for delegating bounded work to child agents nested under the current session when the user asks you to split or parallelize work.

When the user says "subagent", default to `agentd_subagent_create`: a child agent parented to the current session and shown nested under it. Use `agentd_create_session` only when the user asks for a "new session", a top-level/visible session, or otherwise wants an independent fleet session.

Read, search, and list through `shell` (`cat`/`sed -n`, `rg`/`grep`, `ls`); batch independent reads into a single command, or issue them as several parallel `shell` calls each with `read_only: true` so they run concurrently, rather than one round-trip per file. Set `read_only: true` ONLY on commands that are provably side-effect-free (no writes, redirects, `$(...)`, or chaining into a mutator); leave it off for anything that changes state. Change files with `edit_file` — prefer one call carrying several hunks (and across files) over many small single-hunk calls. The shell tool runs `bash -lc` with a default 30s timeout.

You are running with the user's permissions. The user must approve every Risky tool call unless the session is in `unsafe-auto`. When a tool is denied, do not retry it without revising the approach — explain what alternative you'd take instead, or ask the user a clarifying question.

LONG-RUNNING TOOLS: a tool result of exactly "(running in background; will report when complete)" means the tool exceeded the foreground time budget and is still running. Don't retry it. Don't poll. Continue with whatever you can do without that result. You'll receive an `OBSERVATION:` message with the real output later — react to that observation when it arrives, ideally with a short summary or a `noted` if no action is needed.

Dynamic session UI: when a task is long-running, multi-step, decision-heavy, or benefits from compact status/actions, call `agentd_context` to discover `session_widgets.dir`, `session_widgets.action_link_scheme`, and supported `widget_markdown_extensions`, then create/update concise `.md` widget files there with normal file tools. Widget creation, updates, and cleanup are mostly automated system behavior: use your best judgment about what to create and when to refresh/delete it, and ask the user first only when approval is absolutely required by normal safety/tool policy or the widget would make a significant product/user-facing decision. Prefer markdown with checklists, tables, callouts, supported widget_markdown_extensions from `agentd_context`, and action links such as `[Run checks](agentd:action/run-checks)` or `[Run checks](agentd:action/run-checks?key=r)` when a keyboard shortcut is desired. Treat action-link observations (`OBSERVATION: ui.action ...`) as user intent and respond through normal tools/policy; action links never bypass approvals.

Be concise. When you finish a turn, emit a short summary of what you did; the user will see your messages and tool calls in the transcript."#;

pub(crate) const SYSTEM_PROMPT_ORCHESTRATOR: &str = r#"You are the agentd orchestrator — a default smith session created by agentd itself, surfaced in the user's TUI minibuffer. You are the always-available control surface for the user's session fleet.

Your job is to help the user run, inspect, and reason about *other* sessions in agentd. Prefer agentd-control tools (prefix `agentd_`) over editing files or running ad-hoc shell commands yourself:
- `agentd_list_sessions` / `agentd_get_session` / `agentd_get_transcript` to inspect state.
- `agentd_send_input` / `agentd_interrupt_session` / `agentd_pin_session` / `agentd_rename_session` to steer.
- `agentd_create_session` to start new top-level/visible harness sessions.
- `agentd_subagent_create` / `agentd_subagent_list` / `agentd_subagent_peek` / `agentd_subagent_enqueue` / `agentd_subagent_cancel` / `agentd_subagent_delete` to run child agents nested under the current session as task-like helpers when delegation is useful.

When the user says "subagent", default to `agentd_subagent_create`: a child agent parented to the current session and shown nested under it. Use `agentd_create_session` only when the user asks for a "new session", a top-level/visible session, or otherwise wants an independent fleet session.

If the user asks about code in a specific session, suggest they `C-x o` into it, or surface relevant snippets via `agentd_get_diff` / `agentd_get_output`. Don't try to edit code in another session's worktree — talk to that session instead.

You also have local tools (shell, edit_file, write_stdin) for quick host-level questions, but use them sparingly. The user has dedicated sessions for real work; you are the dispatcher.

LONG-RUNNING TOOLS: a tool result of exactly "(running in background; will report when complete)" means the tool exceeded the foreground time budget and is still running. Don't retry it. Don't poll. Continue with whatever you can do without that result. You'll receive an `OBSERVATION:` message with the real output later — react to that observation when it arrives, ideally with a short summary or a `noted` if no action is needed.

EVENT OBSERVATIONS: messages starting with "OBSERVATION:" come from agentd, not the user. They can be fleet events or ambient loop ticks.

For fleet-event observations, decide whether the user benefits from being notified or whether action is helpful. If neither — most cases, especially routine awaiting_input transitions — reply with exactly the single word `noted` and nothing else. If something is genuinely worth surfacing (an unexpected error or a session done with notable output), surface it per SURFACING below. Never start a turn by re-stating the observation back at the user. Never invoke tools just to "check in" on a session whose state you already know from the observation.

For `OBSERVATION: ambient fleet monitor` findings, act as an ambient companion. You may inspect fleet state, project memory, widgets, transcripts, diffs, or outputs when that would help you notice blockers, stale work, workflow issues, or opportunities to reduce user effort. Surface anything worth the user's attention per SURFACING below — text for a passing FYI, a widget for anything that needs a response. If nothing is worth surfacing, reply exactly `noted`. Do not take risky/destructive/external actions without normal approval; ambient help is advisory and no critical user journey should rely on it.

Dynamic session UI: when a session/task benefits from compact status/actions, call `agentd_context` to discover `session_widgets.dir`, `session_widgets.action_link_scheme`, and supported `widget_markdown_extensions`, then create/update concise `.md` widget files there with normal file tools. Widget creation, updates, and cleanup are mostly automated system behavior: use best judgment and ask first only when normal safety/tool policy absolutely requires approval or the widget would make a significant product/user-facing decision. Use checklists, supported widget_markdown_extensions from `agentd_context`, and action links such as `[Open checks](agentd:action/open-checks)` or `[Open checks](agentd:action/open-checks?key=o)` when a keyboard shortcut is desired. Treat `OBSERVATION: ui.action ...` as user intent; actions still go through normal tools and approvals.

SURFACING — choose the channel by whether the user needs to respond. A short text reply is literally your *monolog*: it types out over your matrix animation, then fades, and the user has no way to reply to it. Use text ONLY for a low-stakes, transient FYI of what you noticed or did ("dogfood finished its build; fleet's quiet"). NEVER use a text monolog to ask a question, request a decision or action, or raise something important — the user may not be looking and cannot respond to it. Anything the user should act on, decide, or be reliably notified of MUST be a compact Operator widget instead — widgets persist and carry action links the user can act on (e.g. a session stuck at a trust prompt, an error needing a choice, "ready to merge?"). Reply exactly `noted` when nothing needs surfacing.

Be concise. The minibuffer panel is small; aim for one to three short lines per turn, longer only when the user explicitly asks for detail. Risky tool calls (delete / kill / send) still gate through approval unless the session is in unsafe-auto."#;

/// Pick the right system prompt for this session's kind. The daemon
/// sets `CONSTRUCT_SESSION_KIND` at spawn time; default is `user` so old
/// callers keep working.
pub(crate) fn system_prompt_for_env() -> &'static str {
    match std::env::var("CONSTRUCT_SESSION_KIND").as_deref() {
        Ok("orchestrator") => SYSTEM_PROMPT_ORCHESTRATOR,
        _ => SYSTEM_PROMPT_USER,
    }
}

/// Default truncation budget per tool result when feeding back to the
/// model. Full output always goes to the transcript.
const TOOL_OUTPUT_BUDGET: usize = 8_000;

/// Build a fresh [`ToolCtx`] that shares the daemon `Client` with `src`
/// when one has already been opened. Used to fan a turn's Safe tool
/// calls into parallel tasks without each one re-connecting.
pub(crate) fn clone_tool_ctx(src: &ToolCtx) -> ToolCtx {
    let new_ctx = ToolCtx {
        cwd: src.cwd.clone(),
        session_id: src.session_id.clone(),
        client: tokio::sync::OnceCell::new(),
        emit: src.emit.clone(),
        procs: src.procs.clone(),
        sandbox: src.sandbox.clone(),
        sandbox_policy: src.sandbox_policy.clone(),
    };
    if let Some(c) = src.client.get() {
        let _ = new_ctx.client.set(c.clone());
    }
    new_ctx
}

/// Select the OS sandbox backend for a session, log which one, and surface a
/// one-time "no OS backstop" notice to the user when a sandbox was *requested*
/// but can't enforce here (degraded to `Noop`). Shared by the headless
/// ([`run`]) and interactive session entry points so the behavior is identical.
pub(crate) fn announce_sandbox(emit: &EventEmitter) -> Arc<dyn crate::sandbox::Sandbox> {
    let sel = crate::sandbox::select();
    tracing::debug!(
        backend = sel.backend.name(),
        enforces = sel.backend.enforces(),
        "smith sandbox backend selected"
    );
    if let Some(msg) = sel.notice {
        emit.log(msg);
    }
    sel.backend.into()
}

/// Run one Safe tool call without going through the approval gate or
/// touching the inbox. Emits the same `ToolUse` / `ToolResult` events
/// `run_one_tool` does so the transcript reads the same. Used by the
/// parallel-safe batch path.
pub(crate) async fn run_safe_call(
    mut call: provider::ToolCall,
    registry: &ToolRegistry,
    ctx: &ToolCtx,
    emit: &EventEmitter,
    hooks: &crate::hooks::Hooks,
    base_hook_payload: &serde_json::Value,
) -> std::result::Result<ToolOutcome, String> {
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
                call_id: Some(call.id.clone()),
            });
            let msg = format!("unknown tool: {}", call.name);
            emit.emit(SessionEvent::ToolResult {
                tool: call.name.clone(),
                ok: false,
                output: msg.clone(),
                call_id: Some(call.id.clone()),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: msg,
            });
        }
    };
    let mutation = hooks
        .mutate(
            "pre_tool_use_mutate",
            &ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": tool.args_summary(&call.input),
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    if let Some(args) = mutation.get("args") {
        call.input = args.clone();
    }
    let args_summary = tool.args_summary(&call.input);
    hooks
        .run(
            "pre_tool_use",
            &ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": args_summary,
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
        call_id: Some(call.id.clone()),
    });
    let outcome = tool
        .run(call.input.clone(), ctx)
        .await
        .map_err(|e| format!("tool error: {e}"));
    match &outcome {
        Ok(o) => emit.emit(SessionEvent::ToolResult {
            tool: call.name.clone(),
            ok: o.ok,
            output: o.output.clone(),
            call_id: Some(call.id.clone()),
        }),
        Err(reason) => emit.emit(SessionEvent::ToolResult {
            tool: call.name.clone(),
            ok: false,
            output: format!("({reason})"),
            call_id: Some(call.id.clone()),
        }),
    }
    let (ok, output) = match &outcome {
        Ok(o) => (o.ok, o.output.clone()),
        Err(reason) => (false, format!("({reason})")),
    };
    hooks
        .run(
            "post_tool_use",
            &ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "ok": ok,
                    "output": truncate_for_model(&output, TOOL_OUTPUT_BUDGET),
                }),
            ),
        )
        .await;
    outcome
}

/// Push a `Message` to the in-memory vec and persist the same message
/// (best-effort) to `smith.jsonl` so a daemon restart can hydrate it.
macro_rules! push_msg {
    ($messages:expr, $persist:expr, $msg:expr) => {{
        let m = $msg;
        if let Some(p) = $persist.as_mut() {
            p.append(&m);
        }
        $messages.push(m);
    }};
}
pub(crate) use push_msg;

pub async fn run(
    params: SessionStartParams,
    ctx: AdapterContext,
    spec: ResolvedModel,
) -> Result<()> {
    let AdapterContext {
        session_id,
        emit,
        mut inbox,
    } = ctx;
    let cwd = PathBuf::from(&params.cwd);
    let hooks = crate::hooks::Hooks::load(&cwd, &emit);
    let base_hook_payload = crate::hooks::base_payload(&session_id, &cwd, "headless");
    let registry = Arc::new(ToolRegistry::with_defaults());
    let specs = registry.specs();

    // Session-local prompt sections are built once at session start;
    // resume re-enters this function and refreshes them.
    let system_prompt: String = {
        let mut prompt = system_prompt_for_env().to_string();
        if let Some(section) = crate::project_guide::format_section(&cwd) {
            prompt.push_str("\n\n");
            prompt.push_str(&section);
        }
        if let Some(section) = crate::skills::format_section(&cwd) {
            prompt.push_str("\n\n");
            prompt.push_str(&section);
        }
        prompt
    };

    let provider_name = spec.provider_name();
    // User-facing label (`@profile` when from config, else the wire name);
    // `provider_name` stays the wire name for limit/context keying.
    let display_name = spec.display_name();
    let model = spec.model.clone();
    let provider = spec.provider;
    // Per-model learned token limits — adapts on overflow errors
    // and bumps upward on successful probe calls. Shared across
    // all agentd sessions on this machine via state_dir.
    let mut limits = crate::model_limits::ModelLimits::load();
    // Initial status — tells the user which provider/model the session
    // actually resolved to.
    emit.emit(SessionEvent::Status {
        state: SessionState::Running,
        detail: Some(format!("{}:{}", display_name, model)),
    });
    hooks
        .run(
            "session_start",
            &cwd,
            &emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "provider": provider_name,
                    "model": model,
                }),
            ),
        )
        .await;

    // Per-session approval mode. Defaults to unsafe-auto when the legacy env override is set.
    let mut approval_mode = if std::env::var("CONSTRUCT_SMITH_AUTOMODE").as_deref() == Ok("1") {
        agentd_protocol::ApprovalMode::UnsafeAuto
    } else {
        agentd_protocol::ApprovalMode::Manual
    };

    let tool_ctx = ToolCtx {
        cwd: cwd.clone(),
        session_id: session_id.clone(),
        client: tokio::sync::OnceCell::new(),
        emit: Some(emit.clone()),
        procs: Arc::new(crate::tools::proc::ProcRegistry::default()),
        sandbox: announce_sandbox(&emit),
        sandbox_policy: crate::sandbox::SandboxPolicy::workspace_default(&cwd),
    };

    // Per-session message persistence (`smith.jsonl`). On resume,
    // hydrate `messages` from the file before the loop starts.
    let data_dir = persist::session_data_dir_from_env();
    let mut persist = Persist::open(data_dir.as_deref());
    let mut messages: Vec<Message> = if persist::is_resume() {
        if let Some(p) = persist.as_ref().map(|p| p.path().to_path_buf()) {
            Persist::load(&p).unwrap_or_default()
        } else {
            Vec::new()
        }
    } else {
        Vec::new()
    };
    // Heal any orphaned tool call / stray tool result in the loaded
    // history before the first provider request. A torn write (crash,
    // turn-timeout SIGKILL, or two adapters briefly sharing one
    // smith.jsonl) can persist an assistant tool-call line without its
    // result; replayed verbatim that one record 400s every codex/openai
    // request and wedges the session permanently. Rewrite the on-disk
    // copy too so the repair sticks across future resumes.
    {
        let repaired = context::sanitize_tool_pairing(&mut messages);
        if repaired > 0 {
            tracing::warn!(
                repaired,
                "smith resume: repaired orphaned tool-call pairing in loaded history"
            );
            if let Some(p) = persist.as_mut() {
                if let Err(e) = p.rewrite(&messages) {
                    tracing::warn!(error = ?e, "smith resume: persist rewrite after repair failed");
                }
            }
        }
    }
    let mut approval_history: VecDeque<ApprovalHistoryEntry> = VecDeque::new();
    let mut current_task: Option<String>;
    let mut pending: VecDeque<String> = VecDeque::new();
    // Skip the initial prompt on resume — it's already in the loaded
    // messages and re-running it would double-charge the model.
    if !persist::is_resume() {
        if let Some(p) = params.prompt.clone() {
            if !p.trim().is_empty() {
                pending.push_back(p);
            }
        }
    }

    loop {
        // Pull next user input (queued, or wait on inbox).
        let mut user_text = match pending.pop_front() {
            Some(t) => t,
            None => {
                emit.emit(SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                });
                match inbox.recv().await {
                    None => return Ok(()),
                    Some(AdapterInboxMsg::Input(t)) => t,
                    Some(AdapterInboxMsg::Stop) => {
                        hooks
                            .run("session_stop", &cwd, &emit, base_hook_payload.clone())
                            .await;
                        return Ok(());
                    }
                    Some(AdapterInboxMsg::Interrupt) => continue,
                    Some(AdapterInboxMsg::SetApprovalMode(mode)) => {
                        approval_mode = mode;
                        continue;
                    }
                    Some(_) => continue,
                }
            }
        };
        if user_text.trim().is_empty() {
            continue;
        }
        current_task = Some(user_text.chars().take(2_000).collect());
        let prompt_payload = hooks
            .mutate(
                "user_prompt_mutate",
                &cwd,
                &emit,
                crate::hooks::merge_payload(
                    base_hook_payload.clone(),
                    json!({ "prompt": user_text }),
                ),
            )
            .await;
        if let Some(prompt) = prompt_payload.get("prompt").and_then(|v| v.as_str()) {
            user_text = prompt.to_string();
        }
        if user_text.trim().is_empty() {
            continue;
        }
        hooks
            .run(
                "user_prompt_submit",
                &cwd,
                &emit,
                crate::hooks::merge_payload(
                    base_hook_payload.clone(),
                    json!({ "prompt": user_text }),
                ),
            )
            .await;
        push_msg!(
            messages,
            persist,
            Message {
                role: Role::User,
                content: Content::Text { text: user_text },
            }
        );

        emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });

        // Loop/thrash guard (model-agnostic): bound a runaway turn and nudge
        // the model when it stops making progress. The caps are opt-in via env
        // (0 = unlimited) so default behavior is unchanged; the non-progress
        // nudge is always on. `CONSTRUCT_SMITH_MAX_STEPS` caps model calls per
        // turn; `CONSTRUCT_SMITH_MAX_TURN_SECS` caps wall-clock per turn (lets a
        // session stop itself gracefully before an external timeout SIGKILL).
        let max_steps: usize = std::env::var("CONSTRUCT_SMITH_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let max_turn_secs: i64 = std::env::var("CONSTRUCT_SMITH_MAX_TURN_SECS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let turn_started_ms = chrono::Utc::now().timestamp_millis();
        let mut step: usize = 0;
        let mut nonproductive_streak: usize = 0;

        // Inner step loop: feed tool results back until the model
        // produces an end-of-turn response.
        loop {
            step += 1;
            // Stop a runaway turn gracefully (vs. an external timeout SIGKILL):
            // partial work stays in the tree and the session parks awaiting input.
            let over_steps = max_steps > 0 && step > max_steps;
            let over_time = max_turn_secs > 0
                && (chrono::Utc::now().timestamp_millis() - turn_started_ms)
                    >= max_turn_secs * 1000;
            if over_steps || over_time {
                let why = if over_steps {
                    format!("step budget reached ({max_steps} model calls in one turn)")
                } else {
                    format!("time budget reached ({max_turn_secs}s in one turn)")
                };
                emit.emit(SessionEvent::Error {
                    message: format!(
                        "{why}; stopping to avoid a runaway loop. Any partial work remains in the working tree."
                    ),
                });
                break;
            }
            // Compute the per-call token budget. Three states:
            //   1. We have a *learned* limit (from a prior overflow
            //      or probe) → use that * UTILIZATION.
            //   2. We don't have a learned limit AND this would
            //      otherwise be a probe → no-op, since there's no
            //      baseline to probe past.
            //   3. Probe: bump the budget to learned * PROBE_RATIO
            //      so the conversation can spill above the safe
            //      cap and exercise the actual model limit.
            let now_ms = chrono::Utc::now().timestamp_millis();
            let hardcoded_cap = context::context_window_tokens(provider_name, &model);
            let learned = limits.get(provider_name, &model);
            let est = context::estimate_tokens(&messages) as u64;
            let is_probe =
                learned.is_some() && limits.should_probe(provider_name, &model, est, now_ms);
            let effective_cap = match learned {
                Some(lim) => lim,
                None => hardcoded_cap as u64,
            };
            let budget = if is_probe {
                ((effective_cap as f64) * crate::model_limits::PROBE_OVERFLOW_RATIO) as usize
            } else {
                ((effective_cap as f64) * context::UTILIZATION) as usize
            };
            // Auto-compact pass before the destructive prune. Headless
            // sessions don't get a `/compact` UI, so this is the only
            // way summaries get generated outside of an interactive
            // session. Failures fall through to plain prune.
            if crate::compact::auto_compact_enabled() {
                match crate::compact::maybe_auto_compact(
                    &mut messages,
                    effective_cap,
                    provider.as_ref(),
                    &model,
                )
                .await
                {
                    Ok(Some(outcome)) => {
                        if let Some(p) = persist.as_mut() {
                            if let Err(e) = p.rewrite(&messages) {
                                tracing::warn!(error = ?e, "auto-compact: persist rewrite failed");
                            }
                        }
                        emit.emit(SessionEvent::ContextCompacted {
                            kept_turns: outcome.kept_turn_pairs,
                            dropped_turns: outcome.dropped_turn_pairs,
                            tokens_before: outcome.tokens_before,
                            tokens_after: outcome.tokens_after,
                            summary_preview: outcome.summary_preview,
                        });
                    }
                    Ok(None) => {}
                    Err(e) => {
                        tracing::warn!(error = ?e, "auto-compact failed; falling back to prune");
                    }
                }
            }
            let _pruned = context::prune_to_budget(&mut messages, budget);

            let mut sink = MessageSink { emit: &emit };
            let turn = match crate::provider_watchdog::complete(
                provider.as_ref(),
                &model,
                &system_prompt,
                &messages,
                &specs,
                &mut sink,
            )
            .await
            {
                Ok(t) => t,
                Err(e) => {
                    // Context overflow → learn the real limit and
                    // retry once. The provider wraps the typed
                    // sentinel in `anyhow::Error`; we downcast to
                    // pull out the extracted limit number (when
                    // the API reported one).
                    if let Some(ov) = e.downcast_ref::<crate::provider::ContextOverflow>() {
                        let new_limit = limits.record_overflow(
                            provider_name,
                            &model,
                            ov.extracted,
                            effective_cap,
                            now_ms,
                        );
                        let retry_budget = ((new_limit as f64) * context::UTILIZATION) as usize;
                        let _pruned = context::prune_to_budget(&mut messages, retry_budget);
                        emit.emit(SessionEvent::Status {
                            state: SessionState::Running,
                            detail: Some(format!(
                                "context overflow — relearning ({} tokens) and retrying",
                                new_limit
                            )),
                        });
                        let mut sink = MessageSink { emit: &emit };
                        match crate::provider_watchdog::complete(
                            provider.as_ref(),
                            &model,
                            &system_prompt,
                            &messages,
                            &specs,
                            &mut sink,
                        )
                        .await
                        {
                            Ok(t) => t,
                            Err(e2) => {
                                // `{:#}` renders the full anyhow cause chain
                                // so the underlying failure is diagnosable.
                                emit.emit(SessionEvent::Error {
                                    message: format!("still over budget after retry: {e2:#}"),
                                });
                                break;
                            }
                        }
                    } else {
                        // `{:#}` renders the full anyhow cause chain (e.g.
                        // "codex-oauth SSE stream: <transport cause>") rather
                        // than just the outermost context label.
                        emit.emit(SessionEvent::Error {
                            message: format!("{e:#}"),
                        });
                        break;
                    }
                }
            };

            // Record the successful call so probe state advances
            // (and the learned limit grows on a probe that pushed
            // past the prior cap).
            limits.record_call(
                provider_name,
                &model,
                turn.usage.input_tokens,
                is_probe,
                hardcoded_cap as u64,
                now_ms,
            );

            emit.emit(SessionEvent::Cost {
                usd: turn.usage.usd,
                tokens_in: turn.usage.input_tokens,
                tokens_out: turn.usage.output_tokens,
                tokens_cached: turn.usage.cached_tokens,
            });

            if turn.is_empty() {
                emit.emit(SessionEvent::Error {
                    message: format!(
                        "{} returned an empty response for model {}",
                        display_name, model
                    ),
                });
                break;
            }

            // Echo the model's reasoning items into history so the provider
            // replays them on the next request (prompt caching + reasoning
            // continuity). They precede the assistant message for this turn.
            for item in &turn.reasoning_items {
                push_msg!(
                    messages,
                    persist,
                    Message {
                        role: Role::Assistant,
                        content: Content::Reasoning(item.clone()),
                    }
                );
            }

            if turn.tool_calls.is_empty() {
                if let Some(text) = turn.text {
                    push_msg!(
                        messages,
                        persist,
                        Message {
                            role: Role::Assistant,
                            content: Content::Text { text },
                        }
                    );
                }
                break;
            }

            // Stash the assistant turn that issued the tool calls so
            // the next provider call has the matching `tool_call_id`s.
            push_msg!(
                messages,
                persist,
                Message {
                    role: Role::Assistant,
                    content: Content::AssistantToolCalls {
                        text: turn.text.clone(),
                        calls: turn.tool_calls.clone(),
                    },
                }
            );

            // Partition into Safe (fan out in parallel) and Risky
            // (serialize through the approval gate). Results are merged
            // by original index before being appended so the
            // tool_call_id ↔ tool_result pairing the model expects is
            // preserved. `effective_risk` applies the daemon-defined
            // auto-approval policy so a write into the widgets dir batches
            // with Safe instead of stalling on a gate it would skip.
            let mut safe_idx: Vec<usize> = Vec::new();
            let mut risky_idx: Vec<usize> = Vec::new();
            for (i, c) in turn.tool_calls.iter().enumerate() {
                let r = registry
                    .get(&c.name)
                    .map(|t| crate::tools::effective_risk(t, &c.input, &tool_ctx.cwd))
                    .unwrap_or(ToolRisk::Risky);
                if matches!(r, ToolRisk::Safe) {
                    safe_idx.push(i);
                } else {
                    risky_idx.push(i);
                }
            }

            let mut outcomes: std::collections::BTreeMap<
                usize,
                std::result::Result<ToolOutcome, String>,
            > = std::collections::BTreeMap::new();
            let mut early_stop = false;

            if !safe_idx.is_empty() {
                let registry_arc = registry.clone();
                let emit_for_safe = emit.clone();
                let tool_ctx_ref = &tool_ctx;
                let tasks: Vec<_> = safe_idx
                    .iter()
                    .map(|&i| {
                        let call = turn.tool_calls[i].clone();
                        let reg = registry_arc.clone();
                        let emit_c = emit_for_safe.clone();
                        let ctx_c = clone_tool_ctx(tool_ctx_ref);
                        let hooks_c = hooks.clone();
                        let hook_base = base_hook_payload.clone();
                        async move {
                            (
                                i,
                                run_safe_call(call, &reg, &ctx_c, &emit_c, &hooks_c, &hook_base)
                                    .await,
                            )
                        }
                    })
                    .collect();
                let join_fut = futures::future::join_all(tasks);
                let results: Vec<(usize, std::result::Result<ToolOutcome, String>)> = tokio::select! {
                    biased;
                    kind = wait_for_interrupt_or_stop(&mut inbox) => {
                        // Drop the batch (cancels all in-flight Safe tasks)
                        // and synthesize a uniform "interrupted" outcome.
                        let reason = match kind {
                            InterruptKind::Stop => {
                                early_stop = true;
                                "stop"
                            }
                            _ => "interrupt",
                        };
                        safe_idx.iter().map(|&i| (i, Err(reason.to_string()))).collect()
                    }
                    r = join_fut => r,
                };
                for (i, outcome) in results {
                    outcomes.insert(i, outcome);
                }
            }

            if !early_stop {
                for &i in &risky_idx {
                    let call = &turn.tool_calls[i];
                    let review_ctx = AutoReviewContext {
                        cwd: tool_ctx.cwd.display().to_string(),
                        current_task: current_task.clone(),
                        recent_approvals: approval_history.iter().cloned().collect(),
                    };
                    let outcome = run_one_tool(
                        call,
                        &registry,
                        &tool_ctx,
                        &emit,
                        &mut inbox,
                        &mut approval_mode,
                        provider.as_ref(),
                        &model,
                        &review_ctx,
                        &mut approval_history,
                        &hooks,
                        &base_hook_payload,
                    )
                    .await;
                    let stop_now = matches!(outcome.as_ref(), Err(r) if r == "stop");
                    outcomes.insert(i, outcome);
                    if stop_now {
                        early_stop = true;
                        break;
                    }
                }
            }

            // Append messages in the model-expected order.
            let mut any_ok = false;
            for i in 0..turn.tool_calls.len() {
                let call = &turn.tool_calls[i];
                let outcome = match outcomes.remove(&i) {
                    Some(o) => o,
                    None => {
                        // This call never ran (e.g. early stop before its
                        // serial slot). Synthesize an aborted result so
                        // the tool_call_id is still answered.
                        Err("turn aborted before this tool ran".to_string())
                    }
                };
                match outcome {
                    Ok(o) => {
                        any_ok |= o.ok;
                        let truncated = truncate_for_model(&o.output, TOOL_OUTPUT_BUDGET);
                        push_msg!(
                            messages,
                            persist,
                            Message {
                                role: Role::Tool,
                                content: Content::ToolResult {
                                    call_id: call.id.clone(),
                                    output: truncated,
                                    is_error: !o.ok,
                                },
                            }
                        );
                    }
                    Err(reason) => {
                        push_msg!(
                            messages,
                            persist,
                            Message {
                                role: Role::Tool,
                                content: Content::ToolResult {
                                    call_id: call.id.clone(),
                                    output: format!("(turn aborted: {reason})"),
                                    is_error: true,
                                },
                            }
                        );
                    }
                }
            }
            if early_stop {
                hooks
                    .run("session_stop", &cwd, &emit, base_hook_payload.clone())
                    .await;
                return Ok(());
            }

            // Non-progress guard: when several consecutive tool rounds produce
            // no successful result, the model is likely stuck repeating a
            // failing action. Inject a one-shot step-back nudge rather than let
            // it thrash; the model then decides — re-check state, change
            // approach, or stop.
            if any_ok {
                nonproductive_streak = 0;
            } else {
                nonproductive_streak += 1;
                if nonproductive_streak >= NONPRODUCTIVE_STREAK_LIMIT {
                    push_msg!(
                        messages,
                        persist,
                        Message {
                            role: Role::User,
                            content: Content::Text {
                                text: format!(
                                    "OBSERVATION (from the agentd harness, not the user): the last \
                                     {nonproductive_streak} tool rounds produced no successful result, so the \
                                     current approach appears stuck. Step back and reconsider — verify the actual \
                                     state (e.g. run the project's build or tests), try a different approach, or \
                                     stop and explain what's blocking. Do not repeat the same failing action."
                                ),
                            },
                        }
                    );
                    nonproductive_streak = 0;
                }
            }

            // If the provider explicitly said max_tokens, drop back to user.
            if matches!(turn.stop_reason, StopReason::MaxTokens) {
                break;
            }
        }
    }
}

/// Run a single tool call with approval gating + a synthetic-result
/// path for "user denied." Returns `Err(reason)` when the loop must
/// abandon the turn (stop / interrupt).
async fn run_one_tool(
    call: &ToolCall,
    registry: &ToolRegistry,
    tool_ctx: &ToolCtx,
    emit: &EventEmitter,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
    approval_mode: &mut agentd_protocol::ApprovalMode,
    provider: &dyn LlmProvider,
    model: &str,
    review_ctx: &AutoReviewContext,
    approval_history: &mut VecDeque<ApprovalHistoryEntry>,
    hooks: &crate::hooks::Hooks,
    base_hook_payload: &serde_json::Value,
) -> std::result::Result<ToolOutcome, String> {
    let mut call = call.clone();
    let tool = match registry.get(&call.name) {
        Some(t) => t,
        None => {
            emit.emit(SessionEvent::ToolUse {
                tool: call.name.clone(),
                args: call.input.clone(),
                call_id: Some(call.id.clone()),
            });
            emit.emit(SessionEvent::ToolResult {
                tool: call.name.clone(),
                ok: false,
                output: format!("unknown tool: {}", call.name),
                call_id: Some(call.id.clone()),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: format!("unknown tool: {}", call.name),
            });
        }
    };

    let mutation = hooks
        .mutate(
            "pre_tool_use_mutate",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": tool.args_summary(&call.input),
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;
    if let Some(args) = mutation.get("args") {
        call.input = args.clone();
    }
    let args_summary = tool.args_summary(&call.input);
    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
        call_id: Some(call.id.clone()),
    });
    hooks
        .run(
            "pre_tool_use",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "args": call.input,
                    "args_summary": args_summary,
                    "risk": tool.risk(),
                }),
            ),
        )
        .await;

    let is_risky = matches!(
        crate::tools::effective_risk(tool, &call.input, &tool_ctx.cwd),
        ToolRisk::Risky
    );
    let mut needs_approval =
        is_risky && matches!(*approval_mode, agentd_protocol::ApprovalMode::Manual);
    let mut allow_auto_review = true;
    if is_risky && matches!(*approval_mode, agentd_protocol::ApprovalMode::AutoReview) {
        match auto_review_for_adapter(
            provider,
            model,
            call.name.as_str(),
            &args_summary,
            &call.input,
            review_ctx,
        )
        .await
        {
            AutoReviewResult::Approve => {
                record_approval_history(
                    approval_history,
                    "auto_review:approve",
                    call.name.clone(),
                    args_summary.clone(),
                );
            }
            // Auto-review never denies on its own; a deny verdict is
            // surfaced to the user, who makes the final reject call.
            AutoReviewResult::Deny | AutoReviewResult::AskUser => {
                emit.log(format!(
                    "auto-review asked user for {}({})",
                    call.name, args_summary
                ));
                needs_approval = true;
                allow_auto_review = false;
            }
        }
    }
    if needs_approval {
        emit.emit(SessionEvent::ToolApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_summary: args_summary.clone(),
            risk: tool.risk(),
            allow_auto_review,
        });
        hooks
            .run(
                "tool_approval_request",
                &tool_ctx.cwd,
                emit,
                crate::hooks::merge_payload(
                    base_hook_payload.clone(),
                    json!({
                        "call_id": call.id,
                        "tool": call.name,
                        "args": call.input,
                        "args_summary": args_summary,
                        "risk": tool.risk(),
                    }),
                ),
            )
            .await;
        // Park on the inbox until we see a matching decision.
        let mut denied = false;
        loop {
            match inbox.recv().await {
                None => {
                    emit.emit(SessionEvent::ToolApprovalResolved {
                        call_id: call.id.clone(),
                    });
                    return Err("stop".into());
                }
                Some(AdapterInboxMsg::Stop) => {
                    emit.emit(SessionEvent::ToolApprovalResolved {
                        call_id: call.id.clone(),
                    });
                    return Err("stop".into());
                }
                Some(AdapterInboxMsg::Interrupt) => {
                    emit.emit(SessionEvent::ToolApprovalResolved {
                        call_id: call.id.clone(),
                    });
                    return Err("interrupt".into());
                }
                Some(AdapterInboxMsg::SetApprovalMode(mode)) => {
                    *approval_mode = mode;
                    if matches!(mode, agentd_protocol::ApprovalMode::UnsafeAuto) {
                        break;
                    }
                }
                Some(AdapterInboxMsg::ToolDecision { call_id, decision }) if call_id == call.id => {
                    match decision.as_str() {
                        "approve" => {
                            record_approval_history(
                                approval_history,
                                "user:approve",
                                call.name.clone(),
                                args_summary.clone(),
                            );
                            break;
                        }
                        "auto_review" => {
                            *approval_mode = agentd_protocol::ApprovalMode::AutoReview;
                            match auto_review_for_adapter(
                                provider,
                                model,
                                call.name.as_str(),
                                &args_summary,
                                &call.input,
                                review_ctx,
                            )
                            .await
                            {
                                AutoReviewResult::Approve => {
                                    record_approval_history(
                                        approval_history,
                                        "auto_review:approve",
                                        call.name.clone(),
                                        args_summary.clone(),
                                    );
                                    break;
                                }
                                // No outright deny: keep asking the user.
                                AutoReviewResult::Deny | AutoReviewResult::AskUser => continue,
                            }
                        }
                        "unsafe_auto" => {
                            *approval_mode = agentd_protocol::ApprovalMode::UnsafeAuto;
                            record_approval_history(
                                approval_history,
                                "user:unsafe_auto",
                                call.name.clone(),
                                args_summary.clone(),
                            );
                            break;
                        }
                        _ => {
                            // Denied — record it and break; the result is
                            // synthesized after the loop, next to the
                            // approval-resolved dismissal signal.
                            record_approval_history(
                                approval_history,
                                "user:deny",
                                call.name.clone(),
                                args_summary.clone(),
                            );
                            denied = true;
                            break;
                        }
                    }
                }
                Some(AdapterInboxMsg::Input(t)) => {
                    // Queue mid-prompt user input for the next turn.
                    emit.log(format!("queued input during approval prompt: {t}"));
                }
                Some(_) => {}
            }
        }
        // The pending approval is resolved (answered here or from another
        // client) — tell passive viewers (web dialog, TUI minibuffer) to
        // dismiss their prompt.
        emit.emit(SessionEvent::ToolApprovalResolved {
            call_id: call.id.clone(),
        });
        if denied {
            let msg = "user denied this action".to_string();
            emit.emit(SessionEvent::ToolResult {
                tool: call.name.clone(),
                ok: false,
                output: msg.clone(),
                call_id: Some(call.id.clone()),
            });
            return Ok(ToolOutcome {
                ok: false,
                output: msg,
            });
        }
    }

    // Run the tool — under a cancelable future so interrupts work.
    //
    // Sandbox escalation: a Risky (effective) call that reaches this point has
    // been *permitted* (user-approved, auto-review-approved, or an auto mode),
    // so it may legitimately cross the confined boundary — run it with the
    // policy relaxed. Safe calls keep the confined floor (writes only within
    // the worktree/widgets/tmp), EXCEPT read_only:true shell calls which get
    // network access because network reads are side-effect-free. See spec 0029.
    let escalated_ctx;
    let network_read_ctx;
    let run_ctx = if is_risky {
        escalated_ctx = tool_ctx.escalated();
        &escalated_ctx
    } else if crate::tools::shell_read_only_optin(&call.name, &call.input) {
        network_read_ctx = tool_ctx.with_network_read();
        &network_read_ctx
    } else {
        tool_ctx
    };
    let outcome = run_with_interrupt(tool, call.input.clone(), run_ctx, inbox).await;
    match &outcome {
        Ok(o) => emit.emit(SessionEvent::ToolResult {
            tool: call.name.clone(),
            ok: o.ok,
            output: o.output.clone(),
            call_id: Some(call.id.clone()),
        }),
        Err(reason) => emit.emit(SessionEvent::ToolResult {
            tool: call.name.clone(),
            ok: false,
            output: format!("({reason})"),
            call_id: Some(call.id.clone()),
        }),
    }
    let (ok, output) = match &outcome {
        Ok(o) => (o.ok, o.output.clone()),
        Err(reason) => (false, format!("({reason})")),
    };
    hooks
        .run(
            "post_tool_use",
            &tool_ctx.cwd,
            emit,
            crate::hooks::merge_payload(
                base_hook_payload.clone(),
                json!({
                    "call_id": call.id,
                    "tool": call.name,
                    "ok": ok,
                    "output": truncate_for_model(&output, TOOL_OUTPUT_BUDGET),
                }),
            ),
        )
        .await;
    outcome
}

async fn run_with_interrupt(
    tool: &dyn crate::tools::Tool,
    input: serde_json::Value,
    ctx: &ToolCtx,
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
) -> std::result::Result<ToolOutcome, String> {
    let (tx, rx) = oneshot::channel::<()>();
    let cwd = ctx.cwd.clone();
    let session_id = ctx.session_id.clone();
    let emit = ctx.emit.clone();
    let procs = ctx.procs.clone();
    let sandbox = ctx.sandbox.clone();
    let sandbox_policy = ctx.sandbox_policy.clone();
    let client_cell = std::sync::Mutex::new(None::<Arc<agentd_client::Client>>);
    if let Some(c) = ctx.client.get() {
        *client_cell.lock().unwrap() = Some(c.clone());
    }
    let tool_fut = async {
        let local_ctx = ToolCtx {
            cwd,
            session_id,
            client: tokio::sync::OnceCell::new(),
            emit,
            procs,
            sandbox,
            sandbox_policy,
        };
        if let Some(c) = client_cell.lock().unwrap().clone() {
            let _ = local_ctx.client.set(c);
        }
        tool.run(input, &local_ctx).await
    };
    tokio::select! {
        biased;
        msg = wait_for_interrupt_or_stop(inbox) => {
            drop(tx);
            match msg {
                InterruptKind::Stop => Err("stop".into()),
                InterruptKind::Interrupt => Err("interrupt".into()),
                InterruptKind::Channel => Err("interrupt".into()),
            }
        }
        res = tool_fut => {
            let _ = rx; // keep channel alive until tool returns
            res.map_err(|e| format!("tool error: {e}"))
        }
    }
}

enum InterruptKind {
    Stop,
    Interrupt,
    Channel,
}

async fn wait_for_interrupt_or_stop(
    inbox: &mut tokio::sync::mpsc::Receiver<AdapterInboxMsg>,
) -> InterruptKind {
    loop {
        match inbox.recv().await {
            None => return InterruptKind::Channel,
            Some(AdapterInboxMsg::Stop) => return InterruptKind::Stop,
            Some(AdapterInboxMsg::Interrupt) => return InterruptKind::Interrupt,
            // Drop other messages while waiting — input gets re-queued
            // by the outer caller's normal inbox handling on the next loop.
            Some(_) => {}
        }
    }
}

pub struct ResolvedModel {
    pub model: String,
    pub provider: Box<dyn LlmProvider>,
    pub kind: provider::routing::Provider,
    /// `Some("@<name>")` when this came from a `[smith.models.<name>]`
    /// config profile; `None` for a direct/prefixed spec. Only affects the
    /// user-facing label — `kind` still carries the real wire protocol so
    /// context-window and learned-limit lookups stay correct.
    pub profile: Option<String>,
}

impl ResolvedModel {
    /// The wire protocol name. Stable key for context-window heuristics and
    /// learned token limits — NOT a display label (a profile reports its
    /// underlying wire name here, e.g. `openai`).
    pub fn provider_name(&self) -> &'static str {
        match self.kind {
            provider::routing::Provider::OpenAI => "openai",
            provider::routing::Provider::Anthropic => "anthropic",
            provider::routing::Provider::Gemini => "gemini",
            provider::routing::Provider::Ollama => "ollama",
            provider::routing::Provider::Grok => "grok",
            provider::routing::Provider::GrokOauth => "grok-oauth",
            provider::routing::Provider::CodexOauth => "codex-oauth",
            provider::routing::Provider::ClaudeOauth => "claude-oauth",
        }
    }

    /// User-facing provider label for banners / status / notes: the
    /// `@profile` name when resolved from config, else the wire name.
    pub fn display_name(&self) -> String {
        self.profile
            .clone()
            .unwrap_or_else(|| self.provider_name().to_string())
    }
}

/// Resolve `--model` (or its absence) to a provider instance and a
/// model name. Order of precedence:
///   1. `params.model` if provided.
///   2. `CONSTRUCT_SMITH_MODEL`.
///   3. ANTHROPIC_API_KEY set → `claude-opus-4-8`.
///   4. OPENAI_API_KEY set → `gpt-5`.
///   5. GEMINI_API_KEY (or GOOGLE_API_KEY) set → `gemini-2.5-pro`.
///   6. fall through to Ollama with `llama3.1`.
pub fn resolve_model(params: &SessionStartParams) -> Result<ResolvedModel> {
    // `params.model` carries the session's active model. The daemon keeps it
    // current across a `/model` switch (it persists the `ModelChanged` event
    // into the session summary and re-injects it on resume), so a resumed
    // session comes back on the model it was last running, not the default.
    let spec_str = params
        .model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("CONSTRUCT_SMITH_MODEL").ok())
        .unwrap_or_else(|| {
            if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                "anthropic:claude-opus-4-8".to_string()
            } else if std::env::var("OPENAI_API_KEY").is_ok() {
                "openai:gpt-5".to_string()
            } else if std::env::var("GEMINI_API_KEY").is_ok()
                || std::env::var("GOOGLE_API_KEY").is_ok()
            {
                "gemini:gemini-2.5-pro".to_string()
            } else {
                "ollama:llama3.1".to_string()
            }
        });
    resolve_model_from_spec(&spec_str)
}

/// Build a [`ResolvedModel`] from a model spec string. Used by the
/// `/model <spec>` slash command to swap mid-session.
///
/// Three forms:
///   - `@<name>` / `@<name>:<model>` — a `[smith.models.<name>]` config
///     profile (its own endpoint + key + default model, optionally model-
///     overridden). Lets several distinct endpoints coexist in one session.
///   - `<provider>:<name>` — explicit provider prefix.
///   - bare name — auto-detected provider.
pub fn resolve_model_from_spec(spec_str: &str) -> Result<ResolvedModel> {
    if let Some(rest) = spec_str.trim().strip_prefix('@') {
        return resolve_profile(rest);
    }
    let spec = provider::routing::parse_model_spec(spec_str)
        .map_err(|e| anyhow::anyhow!("invalid model spec `{spec_str}`: {e}"))?;
    let provider: Box<dyn LlmProvider> = match spec.provider {
        provider::routing::Provider::OpenAI => Box::new(provider::openai::OpenAi::from_env()?),
        provider::routing::Provider::Anthropic => {
            Box::new(provider::anthropic::Anthropic::from_env()?)
        }
        provider::routing::Provider::Gemini => Box::new(provider::gemini::Gemini::from_env()?),
        provider::routing::Provider::Ollama => Box::new(provider::ollama::Ollama::from_env()?),
        provider::routing::Provider::Grok => {
            Box::new(provider::openai::OpenAi::with_config(
                Some(GROK_BASE_URL.to_string()),
                grok_api_key()?,
            )?)
        }
        provider::routing::Provider::GrokOauth => {
            Box::new(provider::openai::OpenAi::with_config(
                Some(GROK_BASE_URL.to_string()),
                grok_oauth_token()?,
            )?)
        }
        provider::routing::Provider::CodexOauth => {
            Box::new(provider::codex_oauth::CodexOauth::from_env()?)
        }
        provider::routing::Provider::ClaudeOauth => {
            Box::new(provider::claude_oauth::ClaudeOauth::from_env()?)
        }
    };
    Ok(ResolvedModel {
        model: spec.model,
        provider,
        kind: spec.provider,
        profile: None,
    })
}

/// Resolve a `@<name>` (or `@<name>:<model-override>`) reference against
/// the `[smith.models.<name>]` profiles in `config.toml`.
fn resolve_profile(rest: &str) -> Result<ResolvedModel> {
    let (name, override_model) = match rest.split_once(':') {
        Some((n, m)) => (n.trim(), Some(m.trim().to_string())),
        None => (rest.trim(), None),
    };
    if name.is_empty() {
        anyhow::bail!("empty profile name after `@` (try `@<name>`)");
    }
    let profile = provider::config::load_profile(name)?.ok_or_else(|| {
        anyhow::anyhow!(
            "no `[smith.models.{name}]` profile in config.toml — declare one \
             with `provider`, `base_url`, `api_key_env`, and `model`"
        )
    })?;
    build_profile_model(name, &profile, override_model)
}

/// Turn a loaded profile (+ optional model override) into a [`ResolvedModel`].
/// Split from the filesystem load so the validation rules are unit-testable.
fn build_profile_model(
    name: &str,
    profile: &provider::config::ModelProfile,
    override_model: Option<String>,
) -> Result<ResolvedModel> {
    // Map the declared wire protocol. OAuth-backed providers are excluded:
    // they have no base-URL/key surface and keep their explicit prefixes
    // (spec 0028).
    let kind = match profile.provider.as_str() {
        "openai" => provider::routing::Provider::OpenAI,
        "anthropic" => provider::routing::Provider::Anthropic,
        "gemini" => provider::routing::Provider::Gemini,
        "ollama" => provider::routing::Provider::Ollama,
        "grok" => provider::routing::Provider::Grok,
        "codex-oauth" | "claude-oauth" | "claude-code-oauth" | "grok-oauth" => anyhow::bail!(
            "profile `{name}`: provider `{}` is OAuth-backed and has no \
             configurable endpoint — use the `{}:` model prefix directly",
            profile.provider,
            profile.provider
        ),
        other => anyhow::bail!(
            "profile `{name}`: unknown provider `{other}` \
             (expected openai | anthropic | gemini | ollama | grok)"
        ),
    };

    let model = override_model
        .or_else(|| profile.model.clone())
        .ok_or_else(|| {
            anyhow::anyhow!(
                "profile `{name}`: no model — set `model = \"...\"` in the \
                 profile or reference it as `@{name}:<model>`"
            )
        })?;

    let base_url = profile.base_url.clone();
    let provider: Box<dyn LlmProvider> = match kind {
        provider::routing::Provider::OpenAI => Box::new(provider::openai::OpenAi::with_config(
            base_url,
            profile_api_key(profile, name, &["OPENAI_API_KEY"])?,
        )?),
        provider::routing::Provider::Anthropic => {
            Box::new(provider::anthropic::Anthropic::with_config(
                base_url,
                profile_api_key(profile, name, &["ANTHROPIC_API_KEY"])?,
            )?)
        }
        provider::routing::Provider::Gemini => Box::new(provider::gemini::Gemini::with_config(
            base_url,
            profile_api_key(profile, name, &["GEMINI_API_KEY", "GOOGLE_API_KEY"])?,
        )?),
        provider::routing::Provider::Ollama => {
            Box::new(provider::ollama::Ollama::with_config(base_url)?)
        }
        provider::routing::Provider::Grok => Box::new(provider::openai::OpenAi::with_config(
            base_url.or_else(|| Some(GROK_BASE_URL.to_string())),
            profile_api_key(profile, name, &["GROK_API_KEY", "XAI_API_KEY"])?,
        )?),
        // codex-oauth / claude-oauth / grok-oauth rejected above.
        _ => unreachable!("oauth providers rejected above"),
    };

    Ok(ResolvedModel {
        model,
        provider,
        kind,
        profile: Some(format!("@{name}")),
    })
}

/// Resolve a profile's API key: explicit `api_key_env` (read that var),
/// else inline `api_key`, else fall back to the wire protocol's standard
/// env var(s). Errors with actionable guidance when none is available.
fn profile_api_key(
    profile: &provider::config::ModelProfile,
    name: &str,
    default_envs: &[&str],
) -> Result<String> {
    if let Some(var) = &profile.api_key_env {
        return std::env::var(var).map_err(|_| {
            anyhow::anyhow!("profile `{name}`: api_key_env `{var}` is not set in the environment")
        });
    }
    if let Some(key) = &profile.api_key {
        return Ok(key.clone());
    }
    for var in default_envs {
        if let Ok(key) = std::env::var(var) {
            return Ok(key);
        }
    }
    anyhow::bail!(
        "profile `{name}`: no API key — set `api_key_env` (preferred) or \
         `api_key` in the profile, or export {}",
        default_envs.join(" / ")
    )
}

fn grok_api_key() -> Result<String> {
    std::env::var("GROK_API_KEY")
        .or_else(|_| std::env::var("XAI_API_KEY"))
        .map_err(|_| {
            anyhow::anyhow!(
                "grok provider requires GROK_API_KEY or XAI_API_KEY"
            )
        })
}

fn grok_auth_path() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("GROK_HOME") {
        if !home.trim().is_empty() {
            return Ok(PathBuf::from(home).join(".grok").join("auth.json"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow::anyhow!("$HOME is not set; cannot locate ~/.grok/auth.json"))?;
    Ok(PathBuf::from(home).join(".grok").join("auth.json"))
}

fn parse_expires_at(ts: &str) -> Option<chrono::DateTime<chrono::Utc>> {
    chrono::DateTime::parse_from_rfc3339(ts).ok().map(|v| v.with_timezone(&chrono::Utc))
}

pub(crate) fn grok_oauth_token() -> Result<String> {
    let path = grok_auth_path()?;
    let bytes = std::fs::read(&path)
        .with_context(|| format!("read {}", path.display()))
        .context("load grok auth token from auth.json")?;
    let entries: BTreeMap<String, serde_json::Value> =
        serde_json::from_slice(&bytes).with_context(|| format!("parse {}", path.display()))?;

    let now = chrono::Utc::now();
    let mut selected_token: Option<(Option<chrono::DateTime<chrono::Utc>>, String)> = None;
    let mut seen_expired = false;

    for entry in entries.values() {
        let Some(obj) = entry.as_object() else {
            continue;
        };
        let Some(token) = obj.get("key").and_then(|v| v.as_str()) else {
            continue;
        };
        let token = token.trim().to_string();
        if token.is_empty() {
            continue;
        }

        let expiry = obj.get("expires_at").and_then(|v| v.as_str()).and_then(parse_expires_at);
        if let Some(expiry) = expiry {
            if expiry > now {
                match selected_token {
                    Some((Some(current_expiry), _)) if expiry <= current_expiry => {}
                    _ => selected_token = Some((Some(expiry), token)),
                }
            } else {
                seen_expired = true;
            }
        } else if selected_token.is_none() {
            selected_token = Some((None, token));
        }
    }

    if let Some((_, token)) = selected_token {
        return Ok(token);
    }
    if seen_expired {
        return Err(anyhow!(
            "grok auth token in {} is expired; run `grok login` or set GROK_API_KEY/XAI_API_KEY.",
            path.display()
        ));
    }
    Err(anyhow!(
        "no usable `key` field found in {}. Run `grok login` or set GROK_API_KEY/XAI_API_KEY.",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use provider::config::ModelProfile;
    use std::env;
    use std::fs;
    use std::sync::Mutex;

    // Serialize tests that mutate GROK_HOME / HOME to prevent env var races
    // when the test harness runs them in parallel.
    static GROK_ENV_LOCK: Mutex<()> = Mutex::new(());

    fn profile(provider: &str, model: Option<&str>) -> ModelProfile {
        ModelProfile {
            provider: provider.to_string(),
            base_url: Some("https://example.invalid/v1".to_string()),
            // inline key so build doesn't depend on process env in tests
            api_key: Some("test-key".to_string()),
            api_key_env: None,
            model: model.map(str::to_string),
        }
    }

    #[test]
    fn profile_builds_with_label_and_wire_kind() {
        let r = build_profile_model("deepseek", &profile("openai", Some("deepseek-chat")), None)
            .expect("build");
        assert_eq!(r.model, "deepseek-chat");
        assert_eq!(r.kind, provider::routing::Provider::OpenAI);
        // wire name keeps internal keying correct; label shows the profile.
        assert_eq!(r.provider_name(), "openai");
        assert_eq!(r.display_name(), "@deepseek");
    }

    #[test]
    fn profile_model_override_wins() {
        let r = build_profile_model(
            "deepseek",
            &profile("openai", Some("deepseek-chat")),
            Some("deepseek-reasoner".to_string()),
        )
        .expect("build");
        assert_eq!(r.model, "deepseek-reasoner");
    }

    #[test]
    fn profile_rejects_oauth_providers() {
        let e = build_profile_model("x", &profile("codex-oauth", Some("gpt-5")), None)
            .err()
            .expect("expected error")
            .to_string();
        assert!(e.contains("OAuth-backed"), "got: {e}");
    }

    #[test]
    fn profile_supports_grok_provider() {
        let r = build_profile_model("x", &profile("grok", Some("grok-2-latest")), None)
            .expect("build");
        assert_eq!(r.provider_name(), "grok");
        assert_eq!(r.kind, provider::routing::Provider::Grok);
    }

    #[test]
    fn profile_rejects_grok_oauth_provider() {
        let e = build_profile_model("x", &profile("grok-oauth", Some("grok-2-latest")), None)
            .err()
            .expect("expected error")
            .to_string();
        assert!(e.contains("OAuth-backed"), "got: {e}");
    }

    #[test]
    fn profile_rejects_unknown_provider() {
        let e = build_profile_model("x", &profile("cohere", Some("command")), None)
            .err()
            .expect("expected error")
            .to_string();
        assert!(e.contains("unknown provider"), "got: {e}");
    }

    #[test]
    fn profile_requires_a_model() {
        let e = build_profile_model("x", &profile("openai", None), None)
            .err()
            .expect("expected error")
            .to_string();
        assert!(e.contains("no model"), "got: {e}");
    }

    #[test]
    fn empty_profile_name_after_at_errors() {
        assert!(resolve_profile("")
            .err()
            .expect("expected error")
            .to_string()
            .contains("empty profile name"));
        assert!(resolve_profile(":gpt-5")
            .err()
            .expect("expected error")
            .to_string()
            .contains("empty profile name"));
    }

    /// Both paths of the "no OS backstop" notice, driven through the real
    /// emit code: a requested-but-unavailable sandbox emits exactly one notice;
    /// off-by-default and an enforcing backend stay silent. One test owns the
    /// process-global `CONSTRUCT_SMITH_SANDBOX` var (no other test reads it) so
    /// the set/remove can't race a parallel reader.
    #[test]
    fn sandbox_notice_emits_only_on_degrade() {
        // Degrade: request a backend this OS can't provide (seatbelt is
        // macOS-only, bubblewrap Linux-only) → guaranteed Noop → one notice.
        let foreign = if cfg!(target_os = "macos") {
            "bwrap"
        } else {
            "seatbelt"
        };
        std::env::set_var("CONSTRUCT_SMITH_SANDBOX", foreign);
        let (emit, mut rx) = EventEmitter::channel("s");
        let sb = announce_sandbox(&emit);
        assert!(!sb.enforces(), "{foreign} must not enforce on this OS");
        let note = rx.try_recv().expect("degrade path must emit a notice");
        assert!(
            note.to_string().contains("no OS-enforced backstop"),
            "unexpected notice payload: {note}"
        );
        assert!(rx.try_recv().is_err(), "exactly one notice on degrade");

        // Off by default → silent.
        std::env::set_var("CONSTRUCT_SMITH_SANDBOX", "none");
        let (emit, mut rx) = EventEmitter::channel("s");
        let _ = announce_sandbox(&emit);
        assert!(rx.try_recv().is_err(), "`none` must be silent");

        // An actually-enforcing backend → silent (macOS host with Seatbelt).
        #[cfg(target_os = "macos")]
        {
            std::env::set_var("CONSTRUCT_SMITH_SANDBOX", "seatbelt");
            let (emit, mut rx) = EventEmitter::channel("s");
            let sb = announce_sandbox(&emit);
            if sb.enforces() {
                assert!(rx.try_recv().is_err(), "an enforcing backend needs no notice");
            }
        }

        std::env::remove_var("CONSTRUCT_SMITH_SANDBOX");
    }

    #[test]
    fn parse_auto_review_decision_accepts_json_with_surrounding_text() {
        assert_eq!(
            parse_auto_review_decision(r#"{"decision":"approve","reason":"routine"}"#),
            AutoReviewResult::Approve
        );
        assert_eq!(
            parse_auto_review_decision(
                "Decision:\n```json\n{\"decision\":\"deny\",\"reason\":\"dangerous\"}\n```"
            ),
            AutoReviewResult::Deny
        );
    }

    #[test]
    fn parse_auto_review_decision_falls_back_to_ask_user() {
        assert_eq!(
            parse_auto_review_decision(r#"{"decision":"ask_user","reason":"unclear"}"#),
            AutoReviewResult::AskUser
        );
        assert_eq!(
            parse_auto_review_decision("not json"),
            AutoReviewResult::AskUser
        );
        assert_eq!(
            parse_auto_review_decision(r#"{"decision":"maybe"}"#),
            AutoReviewResult::AskUser
        );
    }

    #[test]
    fn orchestrator_surfacing_guidance_is_current_and_unambiguous() {
        let p = SYSTEM_PROMPT_ORCHESTRATOR;
        // Stale observation string (renamed in #376) must not reappear — the
        // ambient guidance keyed on it would silently never fire.
        assert!(
            !p.contains("ambient operator loop tick"),
            "stale ambient observation string is back"
        );
        assert!(p.contains("ambient fleet monitor"), "ambient string not updated");
        // The text-vs-widget principle: monolog is FYI-only; anything needing a
        // response is a widget.
        assert!(p.contains("SURFACING"), "missing surfacing guidance");
        assert!(p.contains("monolog"), "missing monolog description");
        assert!(
            p.contains("MUST be a compact Operator widget"),
            "widget-for-response rule weakened"
        );
        // The old blunt contradiction is gone.
        assert!(
            !p.contains("Prefer updating/removing compact Operator widgets over chatting"),
            "contradictory 'prefer widgets over chatting' line is back"
        );
    }

    #[test]
    fn auto_review_prompt_guides_model_toward_routine_repo_work() {
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("active git worktree"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT
            .contains("git makes those changes inspectable and reversible"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("build, format, lint, and test loops"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("rg"), "grep-like discovery");
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("Do not ask the user merely because a shell command chains"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("broad/unscoped paths"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("secrets or credentials"));
    }

    #[test]
    fn routine_dev_shell_command_is_auto_approved_by_decision_logic() {
        let input = serde_json::json!({"command":"git status && cargo test --all-targets && rg TODO src"});
        assert!(is_auto_review_routine_shell_command("shell", &input));
        let noisy_input = serde_json::json!({
            "command":"git status && cargo test --all-targets && rg TODO src",
            "interactive": false,
            "read_only": false,
        });
        assert!(is_auto_review_routine_shell_command("shell", &noisy_input));
    }

    #[test]
    fn routine_dev_shell_command_rejects_risky_sequences() {
        assert!(!is_auto_review_routine_shell_command(
            "shell",
            &serde_json::json!({"command":"ls | rg TODO"})
        ));
        assert!(!is_auto_review_routine_shell_command(
            "shell",
            &serde_json::json!({"command":"sed -i \"s/a/b/\" file.txt"})
        ));
        assert!(!is_auto_review_routine_shell_command(
            "shell",
            &serde_json::json!({"command":"cargo publish"})
        ));
        assert!(!is_auto_review_routine_shell_command(
            "shell",
            &serde_json::json!({"command":"git status", "interactive": true})
        ));
        assert!(!is_auto_review_routine_shell_command(
            "shell",
            &serde_json::json!({"command":"rm -rf target && cargo test"})
        ));
    }

    #[test]
    fn grok_oauth_token_selects_latest_unexpired() {
        let _lock = GROK_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path().join("grok");
        fs::create_dir_all(grok_home.join(".grok")).expect("mkdir");
        let auth_path = grok_home.join(".grok").join("auth.json");
        fs::write(
            &auth_path,
            serde_json::json!({
                "expired": { "key": "old", "expires_at": "2020-01-01T00:00:00Z" },
                "older": { "key": "first", "expires_at": "2060-01-01T00:00:00Z" },
                "newer": { "key": "second", "expires_at": "2099-01-01T00:00:00Z" },
            })
            .to_string(),
        )
        .expect("write");

        let old_home = env::var_os("HOME");
        let old_grok_home = env::var_os("GROK_HOME");
        env::set_var("HOME", "/does/not/matter");
        env::set_var("GROK_HOME", grok_home);
        let token = grok_oauth_token().expect("token");
        if let Some(v) = old_grok_home {
            env::set_var("GROK_HOME", v);
        } else {
            env::remove_var("GROK_HOME");
        }
        if let Some(v) = old_home {
            env::set_var("HOME", v);
        } else {
            env::remove_var("HOME");
        }
        assert_eq!(token, "second");
    }

    #[test]
    fn grok_oauth_token_rejects_expired_only() {
        let _lock = GROK_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path().join("grok");
        fs::create_dir_all(grok_home.join(".grok")).expect("mkdir");
        let auth_path = grok_home.join(".grok").join("auth.json");
        fs::write(
            &auth_path,
            serde_json::json!({
                "expired": { "key": "old", "expires_at": "2020-01-01T00:00:00Z" },
            })
            .to_string(),
        )
        .expect("write");

        let old_home = env::var_os("HOME");
        let old_grok_home = env::var_os("GROK_HOME");
        env::set_var("HOME", "/does/not/matter");
        env::set_var("GROK_HOME", grok_home);
        let err = grok_oauth_token().unwrap_err().to_string();
        if let Some(v) = old_grok_home {
            env::set_var("GROK_HOME", v);
        } else {
            env::remove_var("GROK_HOME");
        }
        if let Some(v) = old_home {
            env::set_var("HOME", v);
        } else {
            env::remove_var("HOME");
        }

        assert!(err.contains("expired"));
    }

    #[test]
    fn resolve_model_from_spec_grok_oauth_reads_auth_json_token() {
        let _lock = GROK_ENV_LOCK.lock().unwrap();
        let tmp = tempfile::tempdir().expect("tempdir");
        let grok_home = tmp.path().join("grok");
        fs::create_dir_all(grok_home.join(".grok")).expect("mkdir");
        let auth_path = grok_home.join(".grok").join("auth.json");
        fs::write(
            &auth_path,
            serde_json::json!({
                "default": { "key": "runtime-token", "expires_at": "2099-01-01T00:00:00Z" },
            })
            .to_string(),
        )
        .expect("write");

        let old_home = env::var_os("HOME");
        let old_grok_home = env::var_os("GROK_HOME");
        env::set_var("HOME", "/does/not/matter");
        env::set_var("GROK_HOME", grok_home);

        let resolved =
            resolve_model_from_spec("grok-oauth:grok-2-latest").expect("resolve");
        if let Some(v) = old_grok_home {
            env::set_var("GROK_HOME", v);
        } else {
            env::remove_var("GROK_HOME");
        }
        if let Some(v) = old_home {
            env::set_var("HOME", v);
        } else {
            env::remove_var("HOME");
        }

        assert_eq!(resolved.model, "grok-2-latest");
        assert_eq!(resolved.provider_name(), "grok-oauth");
    }
}
