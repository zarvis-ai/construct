//! Ambient Matrix-rain state for the empty portion of the session list.
//!
//! The renderer owns the per-cell animation math; this module keeps the
//! semantic part small: incoming session events occasionally resolve the rain
//! into a single highlighted word.

use agentd_protocol::{SessionEvent, SessionState};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashTone {
    Work,
    Good,
    Warn,
    Bad,
}

#[derive(Debug, Clone)]
pub struct FlashWord {
    pub text: &'static str,
    pub tone: FlashTone,
    pub started: Instant,
    pub duration: Duration,
    priority: u8,
}

impl FlashWord {
    pub fn progress(&self, now: Instant) -> Option<f32> {
        let elapsed = now.checked_duration_since(self.started)?;
        if elapsed >= self.duration {
            return None;
        }
        Some(elapsed.as_secs_f32() / self.duration.as_secs_f32())
    }
}

#[derive(Debug, Default, Clone)]
pub struct MatrixRain {
    flash: Option<FlashWord>,
}

impl MatrixRain {
    pub fn active_flash(&self, now: Instant) -> Option<&FlashWord> {
        self.flash.as_ref().filter(|f| f.progress(now).is_some())
    }

    pub fn observe_event(&mut self, event: &SessionEvent) {
        if let Some((text, tone, priority)) = word_for_event(event) {
            self.flash(text, tone, priority);
        }
    }

    pub fn observe_tool_decision(&mut self, decision: &str) {
        match decision {
            "approve" | "automode" => self.flash("approved", FlashTone::Good, 95),
            "deny" => self.flash("denied", FlashTone::Bad, 95),
            _ => {}
        }
    }

    fn flash(&mut self, text: &'static str, tone: FlashTone, priority: u8) {
        let now = Instant::now();
        if let Some(current) = self.active_flash(now) {
            if current.priority > priority {
                return;
            }
        }
        self.flash = Some(FlashWord {
            text,
            tone,
            started: now,
            duration: Duration::from_millis(1_150),
            priority,
        });
    }
}

fn word_for_event(event: &SessionEvent) -> Option<(&'static str, FlashTone, u8)> {
    match event {
        SessionEvent::ToolApprovalRequest { .. } => Some(("auth", FlashTone::Warn, 90)),
        SessionEvent::Error { .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::Done { exit_code } if *exit_code == 0 => {
            Some(("worked", FlashTone::Good, 45))
        }
        SessionEvent::Done { .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::Status { state, .. } => match state {
            SessionState::Running => Some(("working", FlashTone::Work, 20)),
            SessionState::AwaitingInput => Some(("waiting", FlashTone::Warn, 35)),
            SessionState::Done => Some(("worked", FlashTone::Good, 45)),
            SessionState::Errored => Some(("failed", FlashTone::Bad, 100)),
            SessionState::Pending | SessionState::Paused => None,
        },
        SessionEvent::ToolUse { tool, .. } => word_for_tool(tool),
        SessionEvent::TaskStart { tool, .. } => word_for_tool(tool),
        SessionEvent::TaskBackgrounded { .. } => Some(("background", FlashTone::Work, 40)),
        SessionEvent::TaskEnd { ok, .. } if *ok => Some(("worked", FlashTone::Good, 45)),
        SessionEvent::TaskEnd { .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::ToolResult { ok: false, .. } => Some(("failed", FlashTone::Bad, 100)),
        SessionEvent::AwaitingInput { .. } => Some(("waiting", FlashTone::Warn, 35)),
        SessionEvent::AgentStatus(status) if status.active => word_for_status(&status.status),
        SessionEvent::Reset => Some(("reset", FlashTone::Warn, 50)),
        SessionEvent::Message { .. }
        | SessionEvent::ToolResult { .. }
        | SessionEvent::Cost { .. }
        | SessionEvent::Diff { .. }
        | SessionEvent::Pty { .. }
        | SessionEvent::EditorState { .. }
        | SessionEvent::AgentStatus(_) => None,
    }
}

fn word_for_tool(tool: &str) -> Option<(&'static str, FlashTone, u8)> {
    if tool == agentd_protocol::TUI_DISPATCH_TOOL {
        return Some(("command", FlashTone::Work, 30));
    }
    match tool {
        "read_file"
        | "list_dir"
        | "find_files"
        | "agentd_get_session"
        | "agentd_get_transcript"
        | "agentd_get_output"
        | "agentd_get_diff"
        | "agentd_list_sessions"
        | "agentd_list_harnesses"
        | "agentd_get_tasks" => Some(("reading", FlashTone::Work, 55)),
        "write_file" | "edit_file" => Some(("editing", FlashTone::Work, 70)),
        "shell" => Some(("running", FlashTone::Work, 60)),
        "agentd_send_input" | "agentd_send_keys" => Some(("sending", FlashTone::Work, 65)),
        "agentd_create_session" => Some(("forking", FlashTone::Work, 60)),
        "agentd_pin_session"
        | "agentd_rename_session"
        | "agentd_set_session_group"
        | "agentd_move_session" => Some(("routing", FlashTone::Work, 45)),
        "agentd_interrupt_session"
        | "agentd_stop_session"
        | "agentd_kill_session"
        | "agentd_delete_session" => Some(("blocked", FlashTone::Warn, 85)),
        "agentd_loop_create" | "agentd_loop_update" | "agentd_loop_remove" => {
            Some(("looping", FlashTone::Work, 55))
        }
        _ => Some(("working", FlashTone::Work, 20)),
    }
}

fn word_for_status(status: &str) -> Option<(&'static str, FlashTone, u8)> {
    let s = status.to_ascii_lowercase();
    if s.contains("edit") || s.contains("patch") || s.contains("write") {
        Some(("editing", FlashTone::Work, 70))
    } else if s.contains("read") || s.contains("scan") || s.contains("search") {
        Some(("reading", FlashTone::Work, 55))
    } else if s.contains("test") || s.contains("run") || s.contains("shell") {
        Some(("running", FlashTone::Work, 60))
    } else if s.contains("wait") {
        Some(("waiting", FlashTone::Warn, 35))
    } else if s.contains("plan") || s.contains("think") {
        Some(("thinking", FlashTone::Work, 30))
    } else if s.trim().is_empty() {
        None
    } else {
        Some(("working", FlashTone::Work, 20))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agentd_protocol::{MessageRole, ToolRisk};

    #[test]
    fn maps_tool_events_to_words() {
        let ev = SessionEvent::ToolUse {
            tool: "edit_file".to_string(),
            args: serde_json::json!({}),
        };
        assert_eq!(word_for_event(&ev).map(|w| w.0), Some("editing"));
    }

    #[test]
    fn higher_priority_flash_wins() {
        let mut rain = MatrixRain::default();
        rain.observe_event(&SessionEvent::Status {
            state: SessionState::Running,
            detail: None,
        });
        rain.observe_event(&SessionEvent::ToolApprovalRequest {
            call_id: "c".into(),
            tool: "shell".into(),
            args_summary: "x".into(),
            risk: ToolRisk::Risky,
        });
        rain.observe_event(&SessionEvent::Message {
            role: MessageRole::Assistant,
            text: "low signal".into(),
        });
        assert_eq!(
            rain.active_flash(Instant::now()).map(|f| f.text),
            Some("auth")
        );
    }
}
