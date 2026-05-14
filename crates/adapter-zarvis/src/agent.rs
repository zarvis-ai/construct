//! The zarvis agent loop. Pulls user input from the inbox, calls the
//! provider, runs any tool calls (gating Risky ones behind an approval
//! prompt unless automode is on), feeds results back, and loops until
//! the model signals end-of-turn.

use crate::context;
use crate::persist::{self, Persist};
use crate::provider::{
    self, Content, LlmProvider, Message, Role, StopReason, TextSink, ToolCall, ToolSpec,
};
use crate::tools::{truncate_for_model, ToolCtx, ToolOutcome, ToolRegistry};
use agentd_protocol::adapter::{AdapterContext, AdapterInboxMsg, EventEmitter};
use agentd_protocol::{MessageRole, SessionEvent, SessionStartParams, SessionState, ToolRisk};
use anyhow::Result;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::oneshot;

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
}

pub(crate) const SYSTEM_PROMPT: &str = r#"You are zarvis, an AI agent embedded in agentd (a multi-session terminal agent fleet).

You have access to:
- Local tools: shell, read_file, write_file, edit_file, list_dir, find_files.
- Agentd-control tools (prefix `agentd_`) for inspecting and steering other agentd sessions running on this host.

Prefer the most specific tool: `read_file` over `shell cat`, `list_dir` over `shell ls`, etc. The shell tool runs `bash -lc` with a default 30s timeout.

You are running with the user's permissions. The user must approve every Risky tool call unless they've enabled `automode`. When a tool is denied, do not retry it without revising the approach — explain what alternative you'd take instead, or ask the user a clarifying question.

Be concise. When you finish a turn, emit a short summary of what you did; the user will see your messages and tool calls in the transcript."#;

/// Default truncation budget per tool result when feeding back to the
/// model. Full output always goes to the transcript.
const TOOL_OUTPUT_BUDGET: usize = 8_000;

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
    let AdapterContext { session_id, emit, mut inbox } = ctx;
    let cwd = PathBuf::from(&params.cwd);
    let registry = ToolRegistry::with_defaults();
    let specs = registry.specs();

    let provider_name = spec.provider_name();
    let model = spec.model.clone();
    let provider = spec.provider;
    // Initial status — tells the user which provider/model the session
    // actually resolved to.
    emit.emit(SessionEvent::Status {
        state: SessionState::Running,
        detail: Some(format!("{}:{}", provider_name, model)),
    });

    // Per-session automode state. Defaults to env override if set.
    let mut automode = std::env::var("AGENTD_ZARVIS_AUTOMODE").as_deref() == Ok("1");

    let tool_ctx = ToolCtx {
        cwd: cwd.clone(),
        session_id: session_id.clone(),
        client: tokio::sync::OnceCell::new(),
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
        let user_text = match pending.pop_front() {
            Some(t) => t,
            None => {
                emit.emit(SessionEvent::Status {
                    state: SessionState::AwaitingInput,
                    detail: None,
                });
                match inbox.recv().await {
                    None => return Ok(()),
                    Some(AdapterInboxMsg::Input(t)) => t,
                    Some(AdapterInboxMsg::Stop) => return Ok(()),
                    Some(AdapterInboxMsg::Interrupt) => continue,
                    Some(AdapterInboxMsg::SetAutoMode(on)) => {
                        automode = on;
                        continue;
                    }
                    Some(_) => continue,
                }
            }
        };
        if user_text.trim().is_empty() {
            continue;
        }
        push_msg!(messages, persist, Message {
            role: Role::User,
            content: Content::Text { text: user_text },
        });

        emit.emit(SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });

        // Inner step loop: feed tool results back until the model
        // produces an end-of-turn response.
        loop {
            let _pruned = context::prune(&mut messages, provider_name, &model);

            let mut sink = MessageSink { emit: &emit };
            let turn = match provider
                .complete(&model, SYSTEM_PROMPT, &messages, &specs, &mut sink)
                .await
            {
                Ok(t) => t,
                Err(e) => {
                    emit.emit(SessionEvent::Error { message: format!("{e}") });
                    break;
                }
            };

            emit.emit(SessionEvent::Cost {
                usd: turn.usage.usd,
                tokens_in: turn.usage.input_tokens,
                tokens_out: turn.usage.output_tokens,
            });

            if turn.tool_calls.is_empty() {
                if let Some(text) = turn.text {
                    push_msg!(messages, persist, Message {
                        role: Role::Assistant,
                        content: Content::Text { text },
                    });
                }
                break;
            }

            // Stash the assistant turn that issued the tool calls so
            // the next provider call has the matching `tool_call_id`s.
            push_msg!(messages, persist, Message {
                role: Role::Assistant,
                content: Content::AssistantToolCalls {
                    text: turn.text.clone(),
                    calls: turn.tool_calls.clone(),
                },
            });

            // Run each call (gated by approval), append the result.
            for call in turn.tool_calls.iter() {
                let outcome = run_one_tool(
                    call,
                    &registry,
                    &tool_ctx,
                    &emit,
                    &mut inbox,
                    &mut automode,
                )
                .await;
                let outcome = match outcome {
                    Ok(o) => o,
                    Err(reason) => {
                        // Stop / interrupt during approval — synthesize an
                        // error result, abandon the turn.
                        push_msg!(messages, persist, Message {
                            role: Role::Tool,
                            content: Content::ToolResult {
                                call_id: call.id.clone(),
                                output: format!("(turn aborted: {reason})"),
                                is_error: true,
                            },
                        });
                        if reason == "stop" {
                            return Ok(());
                        }
                        break;
                    }
                };
                let truncated = truncate_for_model(&outcome.output, TOOL_OUTPUT_BUDGET);
                push_msg!(messages, persist, Message {
                    role: Role::Tool,
                    content: Content::ToolResult {
                        call_id: call.id.clone(),
                        output: truncated,
                        is_error: !outcome.ok,
                    },
                });
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
    automode: &mut bool,
) -> std::result::Result<ToolOutcome, String> {
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

    emit.emit(SessionEvent::ToolUse {
        tool: call.name.clone(),
        args: call.input.clone(),
    });

    let needs_approval = !*automode && matches!(tool.risk(), ToolRisk::Risky);
    if needs_approval {
        emit.emit(SessionEvent::ToolApprovalRequest {
            call_id: call.id.clone(),
            tool: call.name.clone(),
            args_summary: tool.args_summary(&call.input),
            risk: tool.risk(),
        });
        // Park on the inbox until we see a matching decision.
        loop {
            match inbox.recv().await {
                None => return Err("stop".into()),
                Some(AdapterInboxMsg::Stop) => return Err("stop".into()),
                Some(AdapterInboxMsg::Interrupt) => return Err("interrupt".into()),
                Some(AdapterInboxMsg::SetAutoMode(on)) => {
                    *automode = on;
                    // If the user just turned automode on, treat it as approval.
                    if on {
                        break;
                    }
                }
                Some(AdapterInboxMsg::ToolDecision { call_id, decision })
                    if call_id == call.id =>
                {
                    match decision.as_str() {
                        "approve" => break,
                        "automode" => {
                            *automode = true;
                            break;
                        }
                        _ => {
                            // Denied — synthesize a result and bail out
                            // of this tool without running it.
                            let msg = "user denied this action".to_string();
                            emit.emit(SessionEvent::ToolResult {
                                tool: call.id.clone(),
                                ok: false,
                                output: msg.clone(),
                            });
                            return Ok(ToolOutcome { ok: false, output: msg });
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
    let client_cell = std::sync::Mutex::new(None::<Arc<agentd_client::Client>>);
    if let Some(c) = ctx.client.get() {
        *client_cell.lock().unwrap() = Some(c.clone());
    }
    let tool_fut = async {
        let local_ctx = ToolCtx {
            cwd,
            session_id,
            client: tokio::sync::OnceCell::new(),
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
        }
    }
}

/// Resolve `--model` (or its absence) to a provider instance and a
/// model name. Order of precedence:
///   1. `params.model` if provided.
///   2. `AGENTD_ZARVIS_MODEL`.
///   3. ANTHROPIC_API_KEY set → `claude-haiku-4-5`.
///   4. OPENAI_API_KEY set → `gpt-5-mini`.
///   5. fall through to Ollama with `llama3.1`.
pub fn resolve_model(params: &SessionStartParams) -> Result<ResolvedModel> {
    let spec_str = params
        .model
        .clone()
        .filter(|s| !s.trim().is_empty())
        .or_else(|| std::env::var("AGENTD_ZARVIS_MODEL").ok())
        .unwrap_or_else(|| {
            if std::env::var("ANTHROPIC_API_KEY").is_ok() {
                "anthropic:claude-haiku-4-5".to_string()
            } else if std::env::var("OPENAI_API_KEY").is_ok() {
                "openai:gpt-5-mini".to_string()
            } else {
                "ollama:llama3.1".to_string()
            }
        });
    let spec = provider::routing::parse_model_spec(&spec_str)
        .map_err(|e| anyhow::anyhow!("invalid model spec `{spec_str}`: {e}"))?;
    let provider: Box<dyn LlmProvider> = match spec.provider {
        provider::routing::Provider::OpenAI => Box::new(provider::openai::OpenAi::from_env()?),
        provider::routing::Provider::Anthropic => Box::new(provider::anthropic::Anthropic::from_env()?),
        provider::routing::Provider::Ollama => Box::new(provider::ollama::Ollama::from_env()?),
    };
    Ok(ResolvedModel {
        model: spec.model,
        provider,
        kind: spec.provider,
    })
}
