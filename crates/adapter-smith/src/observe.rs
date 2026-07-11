//! Orchestrator-only event observer.
//!
//! When the smith adapter is running as the daemon's orchestrator
//! session (`CONSTRUCT_SESSION_KIND=orchestrator`), it opens a second
//! IPC connection to the daemon and subscribes to events from every
//! other session. Filtered, those events flow back to the interactive
//! agent loop as [`Observation`]s — the orchestrator surfaces them as
//! pseudo-user messages with a leading `OBSERVATION:` marker so the
//! model can react ("session foo finished — should I tell the user?")
//! without the user having to ask.
//!
//! Filtering keeps cost bounded:
//!
//! - **Self-loop guard**: events whose `session_id` matches the
//!   orchestrator's own id are dropped before they reach the channel.
//! - **Type allow-list**: only `Status{AwaitingInput|Errored|Done}`
//!   and `Done{...}` get through. Approval requests stay in the
//!   requesting session's PTY so the orchestrator/minibuffer does not
//!   duplicate or steal focus from the inline prompt. The
//!   high-volume `Message`/`ToolUse`/`Cost`/`Pty` traffic is dropped.
//!
//! The interactive loop applies a separate sliding-window rate limit
//! ([`RateLimiter`]) so an unexpected burst doesn't fire a turn per
//! event.

use construct_client::Client;
use construct_protocol::{
    ipc_notif, paths::Paths, EventNotificationPayload, SessionEvent, SessionState,
};
use std::collections::VecDeque;
use std::time::{Duration, Instant};
use tokio::sync::mpsc;

/// A single observation forwarded from the daemon to the orchestrator
/// agent loop. Always references a *different* session — the orchestrator
/// never observes its own state changes.
#[derive(Debug, Clone)]
pub struct Observation {
    pub session_id: String,
    /// First ~10 chars of the id, for human-readable log messages.
    pub session_short: String,
    /// Short, single-line description of what happened.
    pub message: String,
}

impl Observation {
    /// Format as the pseudo-user message body the agent loop pushes
    /// into the conversation. The `OBSERVATION:` prefix is what the
    /// orchestrator's system prompt keys on to know it's a monitor
    /// notification rather than a real user turn.
    pub fn as_synthetic_user_message(&self) -> String {
        format!(
            "OBSERVATION: session {} {}.",
            self.session_short, self.message
        )
    }
}

/// Spawn a background task that opens its own IPC connection to the
/// daemon, subscribes to all events, filters out non-interesting ones
/// and self-loops, and pushes the survivors onto the returned channel.
///
/// If the connection or subscribe fails the task exits silently and
/// the channel stays empty — the agent loop is fine running without
/// observations.
pub fn spawn(self_id: String) -> mpsc::UnboundedReceiver<Observation> {
    let (tx, rx) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        let socket = Paths::discover().socket();
        let client = match Client::connect(&socket).await {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "orchestrator observe: connect failed");
                return;
            }
        };
        let mut notif_rx = match client.take_notifications().await {
            Some(rx) => rx,
            None => {
                tracing::warn!("orchestrator observe: notif channel already taken");
                return;
            }
        };
        if let Err(e) = client.subscribe(None).await {
            tracing::warn!(error = %e, "orchestrator observe: subscribe failed");
            return;
        }
        while let Some(n) = notif_rx.recv().await {
            if n.method != ipc_notif::EVENT {
                continue;
            }
            let Some(params) = n.params else { continue };
            let payload: EventNotificationPayload = match serde_json::from_value(params) {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "orchestrator observe: bad payload");
                    continue;
                }
            };
            if payload.session_id == self_id {
                continue;
            }
            let Some(msg) = format_observation(&payload.event) else {
                continue;
            };
            let short = payload.session_id.chars().take(10).collect::<String>();
            if tx
                .send(Observation {
                    session_id: payload.session_id,
                    session_short: short,
                    message: msg,
                })
                .is_err()
            {
                // Receiver dropped — agent loop exited; stop work.
                break;
            }
        }
    });
    rx
}

/// Map an event to a one-line "this happened" string, or `None` if
/// the event isn't interesting enough to forward.
fn format_observation(ev: &SessionEvent) -> Option<String> {
    match ev {
        SessionEvent::Status { state, detail } => {
            let label = match state {
                SessionState::AwaitingInput => "entered awaiting_input",
                SessionState::Errored => "entered errored",
                SessionState::Done => "is done",
                // Pending / Running / Paused are mid-flight; skip.
                _ => return None,
            };
            let suffix = detail
                .as_ref()
                .map(|d| format!(" ({d})"))
                .unwrap_or_default();
            Some(format!("{label}{suffix}"))
        }
        SessionEvent::Done { exit_code } => Some(format!("ended (exit={exit_code})")),
        _ => None,
    }
}

/// Sliding-window rate limiter for observation-triggered turns. The
/// orchestrator can react to at most `cap` observations in any
/// `window` (5 per minute by default) — enough to feel responsive to
/// real fleet activity without firing a turn on every burst.
pub struct RateLimiter {
    window: Duration,
    cap: usize,
    timestamps: VecDeque<Instant>,
}

impl RateLimiter {
    pub fn new(cap: usize, window: Duration) -> Self {
        Self {
            window,
            cap,
            timestamps: VecDeque::new(),
        }
    }

    /// Try to record a new event. Returns `true` if it fits in the
    /// window (the caller should proceed), `false` if it's
    /// rate-limited (drop the event).
    pub fn try_consume(&mut self) -> bool {
        let now = Instant::now();
        while let Some(t) = self.timestamps.front() {
            if now.duration_since(*t) > self.window {
                self.timestamps.pop_front();
            } else {
                break;
            }
        }
        if self.timestamps.len() >= self.cap {
            return false;
        }
        self.timestamps.push_back(now);
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn status_states_filter_correctly() {
        let s = SessionEvent::Status {
            state: SessionState::AwaitingInput,
            detail: None,
        };
        assert!(format_observation(&s).is_some());

        let s = SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        };
        assert!(format_observation(&s).is_none());

        let s = SessionEvent::Status {
            state: SessionState::Errored,
            detail: Some("boom".into()),
        };
        let msg = format_observation(&s).unwrap();
        assert!(msg.contains("errored"));
        assert!(msg.contains("boom"));
    }

    #[test]
    fn high_volume_events_drop() {
        for ev in [
            SessionEvent::Message {
                role: construct_protocol::MessageRole::User,
                text: "hi".into(),
            },
            SessionEvent::ToolUse {
                tool: "x".into(),
                args: serde_json::Value::Null,
                call_id: None,
            },
            SessionEvent::Cost {
                usd: 0.0,
                tokens_in: 0,
                tokens_out: 0,
                tokens_cached: 0,
            },
            SessionEvent::Pty { data: "".into() },
            SessionEvent::ToolApprovalRequest {
                call_id: "call-1".into(),
                tool: "shell".into(),
                args_summary: "echo hi".into(),
                risk: construct_protocol::ToolRisk::Risky,
                allow_auto_review: true,
            },
        ] {
            assert!(format_observation(&ev).is_none());
        }
    }

    #[test]
    fn rate_limit_enforces_cap() {
        let mut rl = RateLimiter::new(3, Duration::from_secs(60));
        assert!(rl.try_consume());
        assert!(rl.try_consume());
        assert!(rl.try_consume());
        assert!(!rl.try_consume());
        assert!(!rl.try_consume());
    }

    #[test]
    fn rate_limit_recovers_after_window() {
        let mut rl = RateLimiter::new(2, Duration::from_millis(50));
        assert!(rl.try_consume());
        assert!(rl.try_consume());
        assert!(!rl.try_consume());
        std::thread::sleep(Duration::from_millis(70));
        assert!(rl.try_consume());
    }
}
