//! The zarvis agent loop. Pulls user input from the inbox, calls the
//! provider, runs any tool calls (gating Risky ones behind an approval
//! prompt unless unsafe-auto is on), feeds results back, and loops until
//! the model signals end-of-turn.

use crate::context;
use crate::persist::{self, Persist};
use crate::provider::{self, Content, LlmProvider, Message, Role, StopReason, TextSink, ToolCall};
use crate::tools::{truncate_for_model, ToolCtx, ToolOutcome, ToolRegistry};
use agentd_protocol::adapter::{AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{MessageRole, SessionEvent, SessionStartParams, SessionState, ToolRisk};
use anyhow::Result;
use serde_json::json;
use std::collections::VecDeque;
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
- build, format, lint, and test commands such as `cargo fmt --all`, `cargo build`, `cargo check`, `cargo clippy`, or `cargo test ...`
- inspection commands such as `git status`, `git diff`, `git log`, `rg`, `ls`, or `sed -n ...`
- ordinary repo hygiene commands that revert or clean a clearly scoped set of tracked files, especially when the command itself derives that set from `git diff --name-only` and filters it

Do not ask the user merely because a shell command chains routine steps with `&&` or pipes read-only output into a bounded follow-up command.
Do not ask the user merely because an edit changes files in the active git worktree; git makes those changes inspectable and reversible.

Never approve clearly destructive, secret-exfiltrating, unrelated, or user-hostile actions — ask the user, who makes the final call to reject.
Also ask the user when context is insufficient, prior decisions conflict, the action targets broad/unscoped paths, touches secrets or credentials, mutates outside the active git worktree, changes user/home/system files, removes large amounts of data, or is ambiguous enough that a reasonable reviewer could not tell what will change."#;

pub async fn auto_review_for_adapter(
    provider: &dyn LlmProvider,
    model: &str,
    tool: &str,
    args_summary: &str,
    ctx: &AutoReviewContext,
) -> AutoReviewResult {
    let user = format!(
        "{}\n\nPending tool:\nTool: {tool}\nArgs summary:\n{args_summary}",
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

pub(crate) const SYSTEM_PROMPT_USER: &str = r#"You are zarvis, an AI agent embedded in agentd (a multi-session terminal agent fleet).

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

pub(crate) const SYSTEM_PROMPT_ORCHESTRATOR: &str = r#"You are the agentd orchestrator — a default zarvis session created by agentd itself, surfaced in the user's TUI minibuffer. You are the always-available control surface for the user's session fleet.

Your job is to help the user run, inspect, and reason about *other* sessions in agentd. Prefer agentd-control tools (prefix `agentd_`) over editing files or running ad-hoc shell commands yourself:
- `agentd_list_sessions` / `agentd_get_session` / `agentd_get_transcript` to inspect state.
- `agentd_send_input` / `agentd_interrupt_session` / `agentd_pin_session` / `agentd_rename_session` to steer.
- `agentd_create_session` to start new top-level/visible harness sessions.
- `agentd_subagent_create` / `agentd_subagent_list` / `agentd_subagent_peek` / `agentd_subagent_enqueue` / `agentd_subagent_cancel` / `agentd_subagent_delete` to run child agents nested under the current session as task-like helpers when delegation is useful.

When the user says "subagent", default to `agentd_subagent_create`: a child agent parented to the current session and shown nested under it. Use `agentd_create_session` only when the user asks for a "new session", a top-level/visible session, or otherwise wants an independent fleet session.

If the user asks about code in a specific session, suggest they `C-x o` into it, or surface relevant snippets via `agentd_get_diff` / `agentd_get_output`. Don't try to edit code in another session's worktree — talk to that session instead.

You also have local tools (shell, edit_file, write_stdin) for quick host-level questions, but use them sparingly. The user has dedicated sessions for real work; you are the dispatcher.

LONG-RUNNING TOOLS: a tool result of exactly "(running in background; will report when complete)" means the tool exceeded the foreground time budget and is still running. Don't retry it. Don't poll. Continue with whatever you can do without that result. You'll receive an `OBSERVATION:` message with the real output later — react to that observation when it arrives, ideally with a short summary or a `noted` if no action is needed.

EVENT OBSERVATIONS: messages starting with "OBSERVATION:" come from the agentd event monitor, not the user. They tell you another session in the fleet changed state (entered awaiting_input, errored, finished, or is asking for approval). For each observation, decide whether the user benefits from being notified or whether action is helpful. If neither — most cases, especially routine awaiting_input transitions — reply with exactly the single word `noted` and nothing else. If something is genuinely worth surfacing (an unexpected error, a session done with notable output, an approval request the user may have missed), give one short sentence. Never start a turn by re-stating the observation back at the user. Never invoke tools just to "check in" on a session whose state you already know from the observation.

Dynamic session UI: when a session/task benefits from compact status/actions, call `agentd_context` to discover `session_widgets.dir`, `session_widgets.action_link_scheme`, and supported `widget_markdown_extensions`, then create/update concise `.md` widget files there with normal file tools. Widget creation, updates, and cleanup are mostly automated system behavior: use best judgment and ask first only when normal safety/tool policy absolutely requires approval or the widget would make a significant product/user-facing decision. Use checklists, supported widget_markdown_extensions from `agentd_context`, and action links such as `[Open checks](agentd:action/open-checks)` or `[Open checks](agentd:action/open-checks?key=o)` when a keyboard shortcut is desired. Treat `OBSERVATION: ui.action ...` as user intent; actions still go through normal tools and approvals.

Be concise. The minibuffer panel is small; aim for one to three short lines per turn, longer only when the user explicitly asks for detail. Risky tool calls (delete / kill / send) still gate through approval unless the session is in unsafe-auto."#;

/// Pick the right system prompt for this session's kind. The daemon
/// sets `AGENTD_SESSION_KIND` at spawn time; default is `user` so old
/// callers keep working.
pub(crate) fn system_prompt_for_env() -> &'static str {
    match std::env::var("AGENTD_SESSION_KIND").as_deref() {
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
    };
    if let Some(c) = src.client.get() {
        let _ = new_ctx.client.set(c.clone());
    }
    new_ctx
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
            });
            let msg = format!("unknown tool: {}", call.name);
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: msg.clone(),
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
    });
    let outcome = tool
        .run(call.input.clone(), ctx)
        .await
        .map_err(|e| format!("tool error: {e}"));
    match &outcome {
        Ok(o) => emit.emit(SessionEvent::ToolResult {
            tool: call.id.clone(),
            ok: o.ok,
            output: o.output.clone(),
        }),
        Err(reason) => emit.emit(SessionEvent::ToolResult {
            tool: call.id.clone(),
            ok: false,
            output: format!("({reason})"),
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
/// (best-effort) to `zarvis.jsonl` so a daemon restart can hydrate it.
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
        detail: Some(format!("{}:{}", provider_name, model)),
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
    let mut approval_mode = if std::env::var("AGENTD_ZARVIS_AUTOMODE").as_deref() == Ok("1") {
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
    };

    // Per-session message persistence (`zarvis.jsonl`). On resume,
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
        // nudge is always on. `AGENTD_ZARVIS_MAX_STEPS` caps model calls per
        // turn; `AGENTD_ZARVIS_MAX_TURN_SECS` caps wall-clock per turn (lets a
        // session stop itself gracefully before an external timeout SIGKILL).
        let max_steps: usize = std::env::var("AGENTD_ZARVIS_MAX_STEPS")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);
        let max_turn_secs: i64 = std::env::var("AGENTD_ZARVIS_MAX_TURN_SECS")
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
                        provider_name, model
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
            });
            emit.emit(SessionEvent::ToolResult {
                tool: call.id.clone(),
                ok: false,
                output: format!("unknown tool: {}", call.name),
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
        loop {
            match inbox.recv().await {
                None => return Err("stop".into()),
                Some(AdapterInboxMsg::Stop) => return Err("stop".into()),
                Some(AdapterInboxMsg::Interrupt) => return Err("interrupt".into()),
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
                            // Denied — synthesize a result and bail out
                            // of this tool without running it.
                            record_approval_history(
                                approval_history,
                                "user:deny",
                                call.name.clone(),
                                args_summary.clone(),
                            );
                            let msg = "user denied this action".to_string();
                            emit.emit(SessionEvent::ToolResult {
                                tool: call.id.clone(),
                                ok: false,
                                output: msg.clone(),
                            });
                            return Ok(ToolOutcome {
                                ok: false,
                                output: msg,
                            });
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
    }

    // Run the tool — under a cancelable future so interrupts work.
    let outcome = run_with_interrupt(tool, call.input.clone(), tool_ctx, inbox).await;
    match &outcome {
        Ok(o) => emit.emit(SessionEvent::ToolResult {
            tool: call.id.clone(),
            ok: o.ok,
            output: o.output.clone(),
        }),
        Err(reason) => emit.emit(SessionEvent::ToolResult {
            tool: call.id.clone(),
            ok: false,
            output: format!("({reason})"),
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
}

impl ResolvedModel {
    pub fn provider_name(&self) -> &'static str {
        match self.kind {
            provider::routing::Provider::OpenAI => "openai",
            provider::routing::Provider::Anthropic => "anthropic",
            provider::routing::Provider::Ollama => "ollama",
            provider::routing::Provider::CodexOauth => "codex-oauth",
        }
    }
}

/// Resolve `--model` (or its absence) to a provider instance and a
/// model name. Order of precedence:
///   1. `params.model` if provided.
///   2. `AGENTD_ZARVIS_MODEL`.
///   3. ANTHROPIC_API_KEY set → `claude-opus-4-8`.
///   4. OPENAI_API_KEY set → `gpt-5`.
///   5. fall through to Ollama with `llama3.1`.
pub fn resolve_model(params: &SessionStartParams) -> Result<ResolvedModel> {
    let spec_str = params
        .model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("AGENTD_ZARVIS_MODEL").ok())
        .unwrap_or_else(|| {
            if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                "anthropic:claude-opus-4-8".to_string()
            } else if std::env::var("OPENAI_API_KEY").is_ok() {
                "openai:gpt-5".to_string()
            } else {
                "ollama:llama3.1".to_string()
            }
        });
    resolve_model_from_spec(&spec_str)
}

/// Build a [`ResolvedModel`] from an explicit `<provider>:<name>` (or
/// auto-detect bare-name) spec string. Used by the `/model <spec>`
/// slash command to swap mid-session.
pub fn resolve_model_from_spec(spec_str: &str) -> Result<ResolvedModel> {
    let spec = provider::routing::parse_model_spec(spec_str)
        .map_err(|e| anyhow::anyhow!("invalid model spec `{spec_str}`: {e}"))?;
    let provider: Box<dyn LlmProvider> = match spec.provider {
        provider::routing::Provider::OpenAI => Box::new(provider::openai::OpenAi::from_env()?),
        provider::routing::Provider::Anthropic => {
            Box::new(provider::anthropic::Anthropic::from_env()?)
        }
        provider::routing::Provider::Ollama => Box::new(provider::ollama::Ollama::from_env()?),
        provider::routing::Provider::CodexOauth => {
            Box::new(provider::codex_oauth::CodexOauth::from_env()?)
        }
    };
    Ok(ResolvedModel {
        model: spec.model,
        provider,
        kind: spec.provider,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

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
    fn auto_review_prompt_guides_model_toward_routine_repo_work() {
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("active git worktree"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT
            .contains("git makes those changes inspectable and reversible"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("cargo fmt --all"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("cargo test"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("git diff --name-only"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("&&"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("pipes read-only output"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("broad/unscoped paths"));
        assert!(AUTO_REVIEW_SYSTEM_PROMPT.contains("secrets or credentials"));
    }
}
