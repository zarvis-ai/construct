//! Per-session task registry for in-flight + backgrounded tool calls.
//!
//! Every tool invocation goes through a [`Supervisor`] that races the
//! tool's `tokio::spawn` handle against three signals:
//!
//!   - **Auto-bg timer** (60 s by default; `AGENTD_TOOL_BG_AFTER_MS`):
//!     the agent decides the tool's been running long enough that the
//!     conversation shouldn't keep blocking on it. The handle is
//!     detached, a placeholder `ToolResult` is synthesized, and the
//!     agent loop moves on. A watcher task fires the real result —
//!     plus an `OBSERVATION:` message to trigger a follow-up turn —
//!     when the tool eventually completes.
//!   - **Kill control message**: user clicked `[kill]` or sent the
//!     `session.tool_action { action: "kill" }` IPC. Aborts the
//!     spawned handle; the supervisor returns `Killed`.
//!   - **Background control message**: user clicked `[bg]` or sent
//!     `session.tool_action { action: "background" }`. Same path
//!     as the auto-bg timer, just triggered earlier.
//!
//! The agent loop's main inbox handler (in [`crate::interactive`])
//! forwards `AdapterInboxMsg::ToolAction` events to the matching
//! supervisor via its control channel. The supervisor in turn
//! reports completions back through the per-session
//! [`BackgroundCompletion`] channel so the agent can synthesize an
//! `OBSERVATION:` for the LLM at the next turn boundary.

use crate::tools::{ToolCtx, ToolOutcome};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Mutex};

/// Placeholder output written to the LLM's conversation when a tool
/// auto-backgrounds. The real result lands later as an
/// `OBSERVATION:` injection. Keep stable — the system prompt
/// references this exact phrasing so the model knows what to expect.
pub const BG_PLACEHOLDER_OUTPUT: &str =
    "(running in background; will report when complete)";

/// Default auto-background threshold. Overridable via
/// `AGENTD_TOOL_BG_AFTER_MS`.
pub const DEFAULT_BG_AFTER_MS: u64 = 60_000;
/// Default time before the `[bg]` / `[kill]` buttons appear on a
/// running tool block. Overridable via `AGENTD_TOOL_BUTTONS_AFTER_MS`.
/// Read by the TUI's `synth_block` — the adapter doesn't act on
/// this directly, it's exported here for a single source of truth.
pub const DEFAULT_BUTTONS_AFTER_MS: u64 = 15_000;

/// One-shot control signal a supervisor accepts from outside.
#[derive(Debug, Clone, Copy)]
pub enum ToolControl {
    Kill,
    Background,
}

/// Live entry for a tool currently being supervised.
pub struct RunningEntry {
    pub name: String,
    pub args_summary: String,
    pub started_at: Instant,
    /// Sender side of the supervisor's control channel. The
    /// adapter's inbox dispatcher uses this to forward
    /// `ToolAction` events from the daemon.
    pub control_tx: mpsc::UnboundedSender<ToolControl>,
}

/// Live entry for a tool that has been auto-backgrounded. The
/// supervisor task that owned its handle hands ownership of the
/// completion to a [`Watcher`] before exiting; this struct retains
/// the metadata so the TUI / `/tasks` / `agentd_get_tasks` can
/// describe what's running.
pub struct BackgroundEntry {
    pub name: String,
    pub args_summary: String,
    pub started_at: Instant,
    pub call_id: String,
}

/// Per-session shared task registry. Both the running and the
/// background maps are keyed by `call_id`. A given call_id appears
/// in exactly one of them at any moment — supervisors atomically
/// move themselves from running → background on auto-bg.
#[derive(Default)]
pub struct Tasks {
    pub running: Mutex<HashMap<String, RunningEntry>>,
    pub backgrounded: Mutex<HashMap<String, BackgroundEntry>>,
}

impl Tasks {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }
}

/// Outcome of a supervisor. Distinct from `ToolOutcome` because
/// `ToolOutcome` is the value fed back to the agent's conversation;
/// `SupervisorOutcome` is the supervisor's report to whoever
/// awaited it.
#[derive(Debug)]
pub enum SupervisorOutcome {
    /// Tool completed within the foreground window.
    Done(Result<ToolOutcome, String>),
    /// User clicked `[kill]` (or sent the equivalent IPC). The
    /// task handle was aborted.
    Killed,
    /// Auto-bg timer fired or user clicked `[bg]`. The handle was
    /// detached and is now owned by a [`Watcher`] in the
    /// `Tasks::backgrounded` map.
    Backgrounded,
}

/// Final state of a backgrounded tool. Pushed onto the per-session
/// [`BgCompletionRx`] channel by the watcher; the agent loop drains
/// it and synthesizes an `OBSERVATION:` for the next turn.
#[derive(Debug)]
pub struct BackgroundCompletion {
    pub call_id: String,
    pub tool_name: String,
    pub args_summary: String,
    pub duration: Duration,
    pub outcome: Result<ToolOutcome, String>,
}

pub type BgCompletionTx = mpsc::UnboundedSender<BackgroundCompletion>;
pub type BgCompletionRx = mpsc::UnboundedReceiver<BackgroundCompletion>;

/// Read the auto-bg threshold from env or fall back to the default.
pub fn bg_after_duration() -> Duration {
    let ms = std::env::var("AGENTD_TOOL_BG_AFTER_MS")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_BG_AFTER_MS);
    Duration::from_millis(ms)
}

/// Run a tool through the supervisor: spawn it, race the join
/// handle against the auto-bg timer + control-channel signals.
/// Registers the call in `tasks.running` for the duration; on
/// auto-bg / explicit `Background`, moves it to `tasks.backgrounded`
/// and spawns a watcher that reports the eventual completion on
/// `completion_tx`.
///
/// `tool_runner` is the closure that does the actual tool work —
/// we accept it as a closure so callers can clone the tool's
/// shared dependencies (the tool registry Arc, a cloned `ToolCtx`,
/// the call's input JSON) at the call site without this module
/// having to know about them.
pub async fn supervise<F>(
    call_id: String,
    name: String,
    args_summary: String,
    tasks: Arc<Tasks>,
    completion_tx: BgCompletionTx,
    bg_after: Duration,
    tool_runner: F,
) -> SupervisorOutcome
where
    F: std::future::Future<Output = Result<ToolOutcome, String>> + Send + 'static,
{
    let (control_tx, mut control_rx) = mpsc::unbounded_channel::<ToolControl>();
    let started_at = Instant::now();
    {
        let mut g = tasks.running.lock().await;
        g.insert(
            call_id.clone(),
            RunningEntry {
                name: name.clone(),
                args_summary: args_summary.clone(),
                started_at,
                control_tx,
            },
        );
    }

    let mut handle = tokio::spawn(tool_runner);

    let outcome = loop {
        let elapsed = started_at.elapsed();
        let remaining = bg_after.saturating_sub(elapsed);
        let sleep_fut = tokio::time::sleep(remaining);

        tokio::select! {
            biased;
            res = &mut handle => {
                break match res {
                    Ok(r) => SupervisorOutcome::Done(r),
                    Err(e) if e.is_cancelled() => SupervisorOutcome::Killed,
                    Err(e) => SupervisorOutcome::Done(Err(format!("join error: {e}"))),
                };
            }
            ctrl = control_rx.recv() => {
                match ctrl {
                    Some(ToolControl::Kill) | None => {
                        handle.abort();
                        // Wait for the abort to land so we can
                        // report Killed deterministically.
                        let _ = (&mut handle).await;
                        break SupervisorOutcome::Killed;
                    }
                    Some(ToolControl::Background) => {
                        let entry = BackgroundEntry {
                            name: name.clone(),
                            args_summary: args_summary.clone(),
                            started_at,
                            call_id: call_id.clone(),
                        };
                        spawn_background_watcher(
                            handle,
                            tasks.clone(),
                            entry,
                            completion_tx.clone(),
                        );
                        break SupervisorOutcome::Backgrounded;
                    }
                }
            }
            _ = sleep_fut => {
                let entry = BackgroundEntry {
                    name: name.clone(),
                    args_summary: args_summary.clone(),
                    started_at,
                    call_id: call_id.clone(),
                };
                spawn_background_watcher(
                    handle,
                    tasks.clone(),
                    entry,
                    completion_tx.clone(),
                );
                break SupervisorOutcome::Backgrounded;
            }
        }
    };

    // Remove the running entry. (Background path replaces it with
    // a `backgrounded` entry; see spawn_background_watcher.)
    {
        let mut g = tasks.running.lock().await;
        g.remove(&call_id);
    }

    outcome
}

fn spawn_background_watcher(
    handle: tokio::task::JoinHandle<Result<ToolOutcome, String>>,
    tasks: Arc<Tasks>,
    entry: BackgroundEntry,
    completion_tx: BgCompletionTx,
) {
    let call_id = entry.call_id.clone();
    let tool_name = entry.name.clone();
    let args_summary = entry.args_summary.clone();
    let started_at = entry.started_at;
    // Move metadata into the backgrounded map immediately so
    // `agentd_get_tasks` and `/tasks` can see it before completion.
    tokio::spawn(async move {
        {
            let mut g = tasks.backgrounded.lock().await;
            g.insert(call_id.clone(), entry);
        }
        let outcome = match handle.await {
            Ok(r) => r,
            Err(e) if e.is_cancelled() => Err("interrupt".into()),
            Err(e) => Err(format!("join error: {e}")),
        };
        let duration = started_at.elapsed();
        {
            let mut g = tasks.backgrounded.lock().await;
            g.remove(&call_id);
        }
        let _ = completion_tx.send(BackgroundCompletion {
            call_id,
            tool_name,
            args_summary,
            duration,
            outcome,
        });
    });
}

/// Borrow-free helper invoked from the inbox dispatcher: looks up
/// `call_id` in the running map and forwards the control message
/// via its `control_tx`. Returns `true` if a matching supervisor
/// was found.
pub async fn forward_control(
    tasks: &Tasks,
    call_id: &str,
    control: ToolControl,
) -> bool {
    let g = tasks.running.lock().await;
    match g.get(call_id) {
        Some(entry) => entry.control_tx.send(control).is_ok(),
        None => false,
    }
}

/// Used by the daemon-restart cleanup path: drain every still-running
/// or still-backgrounded entry, abort their handles. Called nowhere
/// today — kept as an explicit cleanup point for future use.
pub async fn _drain_all(_tasks: &Tasks) {
    // No-op for now; backgrounded watchers will receive abort signals
    // when the agent loop exits and the channel closes.
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn supervisor_returns_done_for_fast_tool() {
        let tasks = Tasks::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let outcome = supervise(
            "c1".into(),
            "fast".into(),
            "".into(),
            tasks.clone(),
            tx,
            Duration::from_secs(60),
            async {
                Ok(ToolOutcome {
                    ok: true,
                    output: "hi".into(),
                })
            },
        )
        .await;
        match outcome {
            SupervisorOutcome::Done(Ok(o)) => assert_eq!(o.output, "hi"),
            other => panic!("expected Done(Ok), got {other:?}"),
        }
        assert!(tasks.running.lock().await.is_empty());
    }

    #[tokio::test]
    async fn supervisor_kills_on_kill_control() {
        let tasks = Tasks::new();
        let (tx, _rx) = mpsc::unbounded_channel();
        let tasks_for_signal = tasks.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            forward_control(&tasks_for_signal, "c1", ToolControl::Kill).await;
        });
        let outcome = supervise(
            "c1".into(),
            "slow".into(),
            "".into(),
            tasks.clone(),
            tx,
            Duration::from_secs(60),
            async {
                tokio::time::sleep(Duration::from_secs(10)).await;
                Ok(ToolOutcome { ok: true, output: "never".into() })
            },
        )
        .await;
        assert!(matches!(outcome, SupervisorOutcome::Killed));
    }

    #[tokio::test]
    async fn supervisor_auto_backgrounds_on_timeout() {
        let tasks = Tasks::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let outcome = supervise(
            "c1".into(),
            "slow".into(),
            "".into(),
            tasks.clone(),
            tx,
            Duration::from_millis(50),
            async {
                tokio::time::sleep(Duration::from_millis(200)).await;
                Ok(ToolOutcome { ok: true, output: "delayed".into() })
            },
        )
        .await;
        assert!(matches!(outcome, SupervisorOutcome::Backgrounded));
        let completion = tokio::time::timeout(
            Duration::from_millis(500),
            rx.recv(),
        )
        .await
        .expect("watcher should report")
        .expect("channel should not close");
        assert_eq!(completion.call_id, "c1");
        match completion.outcome {
            Ok(o) => assert_eq!(o.output, "delayed"),
            Err(e) => panic!("expected ok, got {e}"),
        }
    }

    #[tokio::test]
    async fn supervisor_manual_background_signal() {
        let tasks = Tasks::new();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let tasks_for_signal = tasks.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(20)).await;
            forward_control(&tasks_for_signal, "c1", ToolControl::Background).await;
        });
        let outcome = supervise(
            "c1".into(),
            "slow".into(),
            "".into(),
            tasks.clone(),
            tx,
            Duration::from_secs(60), // wouldn't auto-bg in this window
            async {
                tokio::time::sleep(Duration::from_millis(80)).await;
                Ok(ToolOutcome { ok: true, output: "delayed".into() })
            },
        )
        .await;
        assert!(matches!(outcome, SupervisorOutcome::Backgrounded));
        let completion = tokio::time::timeout(
            Duration::from_millis(500),
            rx.recv(),
        )
        .await
        .expect("watcher reports")
        .unwrap();
        match completion.outcome {
            Ok(o) => assert_eq!(o.output, "delayed"),
            Err(e) => panic!("expected ok, got {e}"),
        }
    }
}
