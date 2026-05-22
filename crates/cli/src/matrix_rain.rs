//! Ambient Matrix-rain state for the empty portion of the session list.
//!
//! The renderer owns the per-cell animation math; this module keeps the
//! semantic part small: incoming session events enqueue words that the TUI
//! renderer reveals by pinning letters when rain columns pass their target row.

use agentd_protocol::{SessionEvent, SessionState};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

const MAX_ACTIVE_REVEALS: usize = 4;

/// Minimum gap between PTY-triggered reveal words for the same
/// session. PTY events arrive in bursts (many bytes per agent turn);
/// without throttling each chunk would queue a word and starve the
/// active-reveal cap.
const PTY_REVEAL_GAP: Duration = Duration::from_millis(3500);

/// Fallback activity words used only when the PTY byte stream
/// hasn't yielded any extractable word for a session yet (or the
/// pool was already drained by the last reveal). Zarvis already
/// emits structured tool events that map to richer words via
/// `word_for_event`; this list is just so the rain isn't silent
/// during the very first PTY chunk of a session.
const PTY_ACTIVITY_WORDS: &[&str] = &[
    "working", "thinking", "running", "writing", "reading", "typing",
];

/// Min/max characters for a word extracted from PTY content. Below
/// the min we'd surface English filler ("the", "and"); above the max
/// the matrix-reveal renderer struggles to fit it in the panel.
const PTY_WORD_MIN_LEN: usize = 4;
const PTY_WORD_MAX_LEN: usize = 12;

/// Cap on how many extracted words to retain per session. The reveal
/// path uses only the most recent one and then drains the pool, so a
/// modest cap is enough to absorb a burst between throttle ticks
/// without unbounded growth.
const PTY_WORD_POOL_MAX: usize = 32;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FlashTone {
    Work,
    Good,
    Warn,
    Bad,
}

/// How a reveal word is laid out across the matrix-rain panel.
///
/// - **Horizontal**: letters spread left-to-right at a single row.
///   Each letter pins when *its column's* drop head passes the row.
///   Needs many active columns to pin fully — best at high intensity.
/// - **Vertical**: letters stacked top-to-bottom in a single column.
///   *One* drop falling through the column pins the whole word in one
///   pass, so vertical reveals work even at low fleet activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RevealOrientation {
    Horizontal,
    Vertical,
}

#[derive(Debug, Clone)]
pub struct RevealWord {
    pub text: String,
    pub orientation: RevealOrientation,
    _tone: FlashTone,
    pub started: Instant,
    pub duration: Duration,
    pub x: f32,
    pub y: f32,
    priority: u8,
    /// Per-character pin time, in `start_instant`-relative ms. `None`
    /// means the matrix rain hasn't dropped a head through that
    /// letter's cell yet — the renderer leaves the cell empty until
    /// a real drop arrives. Length equals `text.chars().count()`.
    pin_state: Vec<Option<u64>>,
    /// Absolute starting column resolved on the first render frame.
    /// Locked from then on so already-pinned letters stay where they
    /// are across intensity changes or panel resizes. For vertical
    /// reveals this is the single column the word lives in; for
    /// horizontal reveals it's the leftmost letter's column.
    resolved_col: Option<u16>,
    /// Absolute starting row, locked alongside `resolved_col`.
    resolved_row: Option<u16>,
}

impl RevealWord {
    pub fn progress(&self, now: Instant) -> Option<f32> {
        let elapsed = now.checked_duration_since(self.started)?;
        if elapsed >= self.duration {
            return None;
        }
        Some(elapsed.as_secs_f32() / self.duration.as_secs_f32())
    }

    fn expired(&self, now: Instant) -> bool {
        now.checked_duration_since(self.started)
            .map(|elapsed| elapsed >= self.duration)
            .unwrap_or(false)
    }

    /// Read-only view of the per-letter pin timestamps. The renderer
    /// uses this to decide which letters are currently visible and
    /// when the "all letters pinned" hold/fade timer can start.
    pub fn pin_state(&self) -> &[Option<u64>] {
        &self.pin_state
    }

    /// Pin one letter. No-op if already pinned, so calling this on
    /// every frame is safe — the first pass wins. `at_elapsed_ms` is
    /// measured from the app's `start_instant`.
    pub fn pin_letter(&mut self, char_idx: usize, at_elapsed_ms: u64) {
        if let Some(slot) = self.pin_state.get_mut(char_idx) {
            if slot.is_none() {
                *slot = Some(at_elapsed_ms);
            }
        }
    }

    /// Lock the reveal's absolute geometry on the first render
    /// frame. Subsequent calls are no-ops — pin timestamps and
    /// rendered cells stay aligned regardless of later resizes or
    /// intensity changes.
    pub fn set_resolved_position(&mut self, col: u16, row: u16) {
        self.resolved_col.get_or_insert(col);
        self.resolved_row.get_or_insert(row);
    }

    /// `(col, row)` once `set_resolved_position` has been called,
    /// `None` until then.
    pub fn resolved_position(&self) -> Option<(u16, u16)> {
        Some((self.resolved_col?, self.resolved_row?))
    }
}

#[derive(Debug, Default, Clone)]
pub struct MatrixRain {
    queue: Vec<RevealWord>,
    /// Last PTY-triggered reveal per session — used to rate-limit
    /// the heartbeat path so a single agent turn doesn't flood the
    /// reveal queue.
    pty_throttle: HashMap<String, Instant>,
    /// Per-session FIFO of words harvested from the PTY byte stream
    /// (ANSI-stripped, alphabetic, 4–12 chars). Each reveal pops the
    /// newest one, so the matrix reflects what the harness most
    /// recently printed instead of cycling a hard-coded list.
    pty_word_pool: HashMap<String, VecDeque<String>>,
    /// Monotonic cursor for the fallback rotation used when the
    /// per-session word pool is empty.
    pty_word_cursor: u32,
}

impl MatrixRain {
    #[cfg(test)]
    pub fn active_reveal(&self, now: Instant) -> Option<&RevealWord> {
        self.active_reveals(now).max_by_key(|word| word.priority)
    }

    pub fn active_reveals(&self, now: Instant) -> impl Iterator<Item = &RevealWord> {
        self.queue
            .iter()
            .filter(move |word| word.progress(now).is_some())
    }

    /// Same as [`active_reveals`] but yields `&mut RevealWord` so the
    /// renderer can pin letters in place as drops pass them.
    pub fn active_reveals_mut(&mut self, now: Instant) -> impl Iterator<Item = &mut RevealWord> {
        self.queue
            .iter_mut()
            .filter(move |word| word.progress(now).is_some())
    }

    pub fn observe_event(&mut self, event: &SessionEvent, intensity: f32) {
        if let Some((text, tone, priority)) = word_for_event(event) {
            self.queue_random(text, tone, priority, intensity);
        }
    }

    /// Heartbeat from a PTY-only harness (codex / claude in
    /// interactive mode, shell). PTY adapters don't emit structured
    /// `ToolUse` / `Status` events while the agent is working, so
    /// without this the matrix rain reveals nothing for them. We
    /// always harvest words from `bytes` into the per-session pool
    /// (so the rain reflects current activity, not stale state) and
    /// then — at most once per `PTY_REVEAL_GAP` per session — queue
    /// the most recent extracted word. Falls back to a rotating
    /// generic word for the first chunk when the pool is empty.
    pub fn observe_pty_activity(
        &mut self,
        session_id: &str,
        bytes: &[u8],
        now: Instant,
        intensity: f32,
    ) {
        {
            let pool = self
                .pty_word_pool
                .entry(session_id.to_string())
                .or_default();
            extract_pty_words(bytes, pool);
        }
        if let Some(prev) = self.pty_throttle.get(session_id) {
            if now.duration_since(*prev) < PTY_REVEAL_GAP {
                return;
            }
        }
        self.pty_throttle.insert(session_id.to_string(), now);

        let extracted = self.pty_word_pool.get_mut(session_id).and_then(|pool| {
            let last = pool.pop_back();
            // Drain the rest so the next reveal reflects fresh
            // activity (or falls back to the rotation) instead of
            // re-surfacing stale words.
            pool.clear();
            last
        });
        let text = extracted.unwrap_or_else(|| {
            let idx = (self.pty_word_cursor as usize) % PTY_ACTIVITY_WORDS.len();
            self.pty_word_cursor = self.pty_word_cursor.wrapping_add(1);
            PTY_ACTIVITY_WORDS[idx].to_string()
        });
        let (x, y) = random_position(&text, self.queue.len());
        let orientation = pick_orientation(&text, intensity);
        self.queue_at(text, FlashTone::Work, x, y, 25, orientation, now);
    }

    /// Forget per-session throttle + extracted-word state. Call when
    /// a session is reset, ends, or is deleted so the maps don't
    /// grow unbounded and a future session reusing the id starts
    /// fresh.
    pub fn forget_session(&mut self, session_id: &str) {
        self.pty_throttle.remove(session_id);
        self.pty_word_pool.remove(session_id);
    }

    pub fn observe_tool_decision(&mut self, decision: &str, intensity: f32) {
        match decision {
            "approve" | "automode" => {
                self.queue_random("approved", FlashTone::Good, 95, intensity)
            }
            "deny" => self.queue_random("denied", FlashTone::Bad, 95, intensity),
            _ => {}
        }
    }

    fn queue_random(&mut self, text: &'static str, tone: FlashTone, priority: u8, intensity: f32) {
        self.queue_random_at(text, tone, priority, intensity, Instant::now());
    }

    fn queue_random_at(
        &mut self,
        text: &'static str,
        tone: FlashTone,
        priority: u8,
        intensity: f32,
        now: Instant,
    ) {
        let (x, y) = random_position(text, self.queue.len());
        let orientation = pick_orientation(text, intensity);
        self.queue_at(text, tone, x, y, priority, orientation, now);
    }

    pub fn queue(
        &mut self,
        text: impl Into<String>,
        tone: FlashTone,
        x: f32,
        y: f32,
        priority: u8,
        orientation: RevealOrientation,
    ) {
        self.queue_at(text, tone, x, y, priority, orientation, Instant::now());
    }

    fn queue_at(
        &mut self,
        text: impl Into<String>,
        tone: FlashTone,
        x: f32,
        y: f32,
        priority: u8,
        orientation: RevealOrientation,
        now: Instant,
    ) {
        self.queue.retain(|word| !word.expired(now));
        // Horizontal reveals need *every* column under the word to
        // fire a drop at the same row, so they need a longer window
        // to have a fair shot at completing. Vertical reveals can
        // pin the whole word in a single drop pass, so 12 s is
        // plenty.
        let duration = match orientation {
            RevealOrientation::Horizontal => Duration::from_millis(27_000),
            RevealOrientation::Vertical => Duration::from_millis(12_000),
        };
        let text: String = text.into();
        let pin_state = vec![None; text.chars().count()];
        self.queue.push(RevealWord {
            text,
            orientation,
            _tone: tone,
            started: now,
            duration,
            x: x.clamp(0.0, 1.0),
            y: y.clamp(0.0, 1.0),
            priority,
            pin_state,
            resolved_col: None,
            resolved_row: None,
        });
        while self.queue.len() > MAX_ACTIVE_REVEALS {
            if let Some((idx, _)) = self
                .queue
                .iter()
                .enumerate()
                .min_by_key(|(_, word)| (word.priority, word.started))
            {
                self.queue.remove(idx);
            } else {
                break;
            }
        }
    }
}

/// Orientation policy:
/// - `intensity < 0.5`: every reveal is vertical. Horizontal needs
///   one drop per column to pass the same row — at low fleet
///   activity most attempts never finish pinning, leaving the word
///   half-written.
/// - `intensity ≥ 0.5`: 50/50 horizontal/vertical, seeded by
///   `(text, wall-clock nanos)` so consecutive reveals don't
///   collapse onto the same orientation.
fn pick_orientation(text: &str, intensity: f32) -> RevealOrientation {
    if intensity < 0.5 {
        return RevealOrientation::Vertical;
    }
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0);
    if hash64(nanos ^ hash_text(text)) & 1 == 0 {
        RevealOrientation::Horizontal
    } else {
        RevealOrientation::Vertical
    }
}

/// Walk `bytes`, skip ANSI escape sequences, and push each run of
/// ASCII-alphabetic characters of length `[PTY_WORD_MIN_LEN,
/// PTY_WORD_MAX_LEN]` onto `pool`. Multi-byte UTF-8 codepoints are
/// treated as word boundaries — keeping the extracted words ASCII
/// so they always render cleanly in the matrix panel.
///
/// The escape skipper handles the two sequence families codex /
/// claude TUIs produce in bulk:
/// - **CSI** (`ESC [ … <final 0x40–0x7E>`) — colors, cursor moves.
/// - **OSC** (`ESC ] … (BEL | ESC \)`) — title sets, hyperlinks.
///
/// Everything else after `ESC` is treated as a 2-byte sequence
/// (charset switches, simple escapes) — enough to avoid accidentally
/// harvesting the escape's terminator as a word character.
fn extract_pty_words(bytes: &[u8], pool: &mut VecDeque<String>) {
    let mut i = 0;
    let mut current = String::new();
    // True when the current alphabetic run already exceeded
    // PTY_WORD_MAX_LEN — we drop the rest of the run rather than
    // letting its tail re-enter as a separate "word".
    let mut poisoned = false;
    while i < bytes.len() {
        let b = bytes[i];
        if b == 0x1b {
            push_word(&mut current, pool);
            poisoned = false;
            i = skip_escape(bytes, i + 1);
        } else if b.is_ascii_alphabetic() {
            if poisoned {
                i += 1;
                continue;
            }
            current.push(b as char);
            if current.len() > PTY_WORD_MAX_LEN {
                current.clear();
                poisoned = true;
            }
            i += 1;
        } else {
            push_word(&mut current, pool);
            poisoned = false;
            i += 1;
        }
    }
    push_word(&mut current, pool);
}

fn push_word(current: &mut String, pool: &mut VecDeque<String>) {
    let len = current.chars().count();
    if (PTY_WORD_MIN_LEN..=PTY_WORD_MAX_LEN).contains(&len) {
        let word = std::mem::take(current);
        // Drop exact consecutive duplicates so a long "Thinking…"
        // stretch doesn't fill the pool with one word.
        if pool.back().map(|prev| prev != &word).unwrap_or(true) {
            pool.push_back(word);
            while pool.len() > PTY_WORD_POOL_MAX {
                pool.pop_front();
            }
        }
    } else {
        current.clear();
    }
}

fn skip_escape(bytes: &[u8], mut i: usize) -> usize {
    if i >= bytes.len() {
        return i;
    }
    let b = bytes[i];
    i += 1;
    match b {
        b'[' => {
            while i < bytes.len() {
                let c = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&c) {
                    break;
                }
            }
        }
        b']' => {
            while i < bytes.len() {
                let c = bytes[i];
                if c == 0x07 {
                    i += 1;
                    break;
                }
                if c == 0x1b && i + 1 < bytes.len() && bytes[i + 1] == b'\\' {
                    i += 2;
                    break;
                }
                i += 1;
            }
        }
        _ => { /* 2-byte escape: ESC + one char already consumed. */ }
    }
    i
}

fn random_position(text: &str, salt: usize) -> (f32, f32) {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_nanos() as u64)
        .unwrap_or(0);
    let seed = hash64(nanos ^ hash_text(text) ^ ((salt as u64) << 32));
    let x = 0.08 + unit_f32(seed) * 0.78;
    let y = 0.22 + unit_f32(hash64(seed)) * 0.66;
    (x, y)
}

fn unit_f32(seed: u64) -> f32 {
    ((seed >> 11) as f64 / ((1u64 << 53) as f64)) as f32
}

fn hash_text(text: &str) -> u64 {
    text.bytes().fold(0xcbf29ce484222325, |acc, b| {
        (acc ^ b as u64).wrapping_mul(0x100000001b3)
    })
}

fn hash64(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e3779b97f4a7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d049bb133111eb);
    x ^ (x >> 31)
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
        SessionEvent::ContextCompacted { .. } => Some(("compact", FlashTone::Work, 50)),
        SessionEvent::Reasoning { .. } => Some(("thinking", FlashTone::Work, 30)),
        SessionEvent::Message { .. }
        | SessionEvent::ToolResult { .. }
        | SessionEvent::Cost { .. }
        | SessionEvent::Diff { .. }
        | SessionEvent::Pty { .. }
        | SessionEvent::EditorState { .. }
        | SessionEvent::BrowserPreview(_)
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
    fn maps_reasoning_event_to_thinking_word() {
        let ev = SessionEvent::Reasoning {
            text: "deciding which file to edit".into(),
        };
        assert_eq!(word_for_event(&ev).map(|w| w.0), Some("thinking"));
    }

    #[test]
    fn higher_priority_flash_wins() {
        let mut rain = MatrixRain::default();
        rain.observe_event(
            &SessionEvent::Status {
                state: SessionState::Running,
                detail: None,
            },
            1.0,
        );
        rain.observe_event(
            &SessionEvent::ToolApprovalRequest {
                call_id: "c".into(),
                tool: "shell".into(),
                args_summary: "x".into(),
                risk: ToolRisk::Risky,
            },
            1.0,
        );
        rain.observe_event(
            &SessionEvent::Message {
                role: MessageRole::Assistant,
                text: "low signal".into(),
            },
            1.0,
        );
        assert_eq!(
            rain.active_reveal(Instant::now()).map(|f| f.text.as_str()),
            Some("auth")
        );
    }

    #[test]
    fn queue_sets_target_position() {
        let mut rain = MatrixRain::default();
        rain.queue("matrix", FlashTone::Work, 0.2, 0.8, 10, RevealOrientation::Horizontal);
        let reveal = rain.active_reveal(Instant::now()).expect("reveal word");
        assert_eq!(reveal.text, "matrix");
        assert_eq!(reveal.x, 0.2);
        assert_eq!(reveal.y, 0.8);
    }

    #[test]
    fn multiple_reveals_can_be_active_together() {
        let mut rain = MatrixRain::default();
        rain.queue("working", FlashTone::Work, 0.2, 0.4, 10, RevealOrientation::Horizontal);
        rain.queue("worked", FlashTone::Good, 0.6, 0.7, 20, RevealOrientation::Horizontal);

        let active: Vec<_> = rain
            .active_reveals(Instant::now())
            .map(|word| word.text.as_str())
            .collect();
        assert_eq!(active, vec!["working", "worked"]);
    }

    #[test]
    fn active_reveal_reports_highest_priority_word() {
        let mut rain = MatrixRain::default();
        rain.queue("working", FlashTone::Work, 0.2, 0.4, 10, RevealOrientation::Horizontal);
        rain.queue("failed", FlashTone::Bad, 0.6, 0.7, 100, RevealOrientation::Horizontal);

        assert_eq!(
            rain.active_reveal(Instant::now())
                .map(|word| word.text.as_str()),
            Some("failed")
        );
    }

    #[test]
    fn random_position_stays_inside_comfortable_band() {
        for salt in 0..256 {
            let (x, y) = random_position("matrix", salt);
            assert!((0.08..=0.86).contains(&x));
            assert!((0.22..=0.88).contains(&y));
        }
    }

    #[test]
    fn pty_activity_falls_back_to_rotation_when_pool_empty() {
        // Codex / claude / shell emit only PTY events while the
        // agent is working — when the very first chunk has no
        // extractable words, the rain still reveals a generic
        // activity word so the panel isn't silent.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", b"\x1b[31m\x1b[0m", now, 1.0);
        let word = rain.active_reveal(now).map(|w| w.text.clone());
        assert!(
            word.as_deref().map(|w| PTY_ACTIVITY_WORDS.contains(&w)).unwrap_or(false),
            "expected fallback rotation word, got {word:?}"
        );
    }

    #[test]
    fn pty_activity_prefers_extracted_word_over_rotation() {
        // When the PTY chunk carries real text, the matrix should
        // reveal a word from it instead of the rotation fallback —
        // that's the whole "reflect actual work" point.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", b"Editing src/foo.rs", now, 1.0);
        let word = rain
            .active_reveal(now)
            .map(|w| w.text.clone())
            .expect("reveal");
        assert!(
            !PTY_ACTIVITY_WORDS.contains(&word.as_str()),
            "expected extracted word, got fallback {word:?}"
        );
        assert_eq!(word, "Editing");
    }

    #[test]
    fn pty_activity_throttles_repeated_calls_within_gap() {
        // A burst of PTY events from a single session should produce
        // exactly one reveal, not one per byte chunk.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", b"Reading", now, 1.0);
        rain.observe_pty_activity("sess-a", b"more", now + Duration::from_millis(100), 1.0);
        rain.observe_pty_activity("sess-a", b"bytes", now + Duration::from_millis(1000), 1.0);
        let count = rain.active_reveals(now + Duration::from_millis(1100)).count();
        assert_eq!(count, 1);
    }

    #[test]
    fn pty_activity_uses_latest_word_after_burst() {
        // Words harvested while throttled still go into the pool, so
        // when the gate next opens the reveal reflects the most
        // recent chunk — not the one that originally tripped the
        // throttle.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", b"Reading", now, 1.0);
        let first = rain
            .active_reveal(now)
            .map(|w| w.text.clone())
            .expect("first reveal");
        assert_eq!(first, "Reading");
        // More bytes arrive while throttled — pool keeps growing.
        rain.observe_pty_activity(
            "sess-a",
            b"Editing files",
            now + Duration::from_millis(500),
            1.0,
        );
        let later = now + PTY_REVEAL_GAP + Duration::from_millis(10);
        rain.observe_pty_activity("sess-a", b"", later, 1.0);
        let texts: Vec<_> = rain
            .active_reveals(later)
            .map(|w| w.text.clone())
            .collect();
        assert_eq!(texts.len(), 2);
        assert_eq!(texts[1], "files"); // most recent word from the pool
    }

    #[test]
    fn pty_activity_per_session_throttle_is_independent() {
        // Two different sessions can each get their own reveal
        // within the gap window — the throttle is per-session.
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", b"Editing", now, 1.0);
        rain.observe_pty_activity("sess-b", b"Reading", now, 1.0);
        assert_eq!(rain.active_reveals(now).count(), 2);
    }

    #[test]
    fn forget_session_clears_throttle_and_pool() {
        let mut rain = MatrixRain::default();
        let now = Instant::now();
        rain.observe_pty_activity("sess-a", b"Editing", now, 1.0);
        rain.forget_session("sess-a");
        assert!(rain.pty_word_pool.get("sess-a").is_none());
        rain.observe_pty_activity("sess-a", b"Reading", now + Duration::from_millis(50), 1.0);
        assert_eq!(rain.active_reveals(now + Duration::from_millis(50)).count(), 2);
    }

    #[test]
    fn pick_orientation_locks_to_vertical_below_half_intensity() {
        // At low fleet activity horizontal reveals would mostly
        // never finish pinning, so the policy forces vertical there.
        for variant in 0..64u32 {
            let text = format!("word{variant}");
            assert_eq!(pick_orientation(&text, 0.0), RevealOrientation::Vertical);
            assert_eq!(pick_orientation(&text, 0.49), RevealOrientation::Vertical);
        }
    }

    #[test]
    fn pick_orientation_returns_both_at_or_above_half_intensity() {
        let mut saw_vertical = false;
        let mut saw_horizontal = false;
        for _ in 0..400 {
            std::thread::sleep(Duration::from_nanos(50));
            match pick_orientation("rotate", 0.8) {
                RevealOrientation::Vertical => saw_vertical = true,
                RevealOrientation::Horizontal => saw_horizontal = true,
            }
            if saw_vertical && saw_horizontal {
                break;
            }
        }
        assert!(
            saw_vertical && saw_horizontal,
            "expected both orientations at intensity 0.8, saw v={saw_vertical} h={saw_horizontal}"
        );
    }

    #[test]
    fn extract_pty_words_strips_csi_and_keeps_alpha_runs() {
        let mut pool = VecDeque::new();
        // Typical codex/claude output: ANSI color around words, file
        // paths with non-alpha separators.
        extract_pty_words(
            b"\x1b[1mEditing\x1b[0m src/main.rs and tests/foo_bar.rs",
            &mut pool,
        );
        let words: Vec<&str> = pool.iter().map(|s| s.as_str()).collect();
        // "src", "rs", "and", "foo", "bar" all fall outside 4..=12 — only
        // these qualify:
        assert_eq!(words, vec!["Editing", "main", "tests"]);
    }

    #[test]
    fn extract_pty_words_strips_osc_and_drops_long_runs() {
        let mut pool = VecDeque::new();
        // OSC sequence terminated by BEL, then a too-long word, then a fine one.
        extract_pty_words(
            b"\x1b]0;title\x07Supercalifragilisticexpialidocious\nReading",
            &mut pool,
        );
        let words: Vec<&str> = pool.iter().map(|s| s.as_str()).collect();
        // OSC payload contains "title" (5 chars) before BEL — it should be
        // skipped along with the rest of the escape, so only "Reading"
        // (the long word is past the 12-char cap) makes it in.
        assert_eq!(words, vec!["Reading"]);
    }

    #[test]
    fn extract_pty_words_deduplicates_consecutive_repeats() {
        let mut pool = VecDeque::new();
        // A spinner that repeatedly prints "Thinking" should leave
        // only one entry, not flood the pool with duplicates.
        for _ in 0..5 {
            extract_pty_words(b"Thinking ", &mut pool);
        }
        assert_eq!(pool.iter().collect::<Vec<_>>(), vec![&"Thinking".to_string()]);
    }

    #[test]
    fn extract_pty_words_caps_pool_at_max() {
        let mut pool = VecDeque::new();
        // Generate `PTY_WORD_POOL_MAX + 8` distinct alphabetic
        // words (digits would split them mid-run and trip the
        // dedupe path, masking the cap behavior we want to test).
        let mut input = String::new();
        for i in 0..(PTY_WORD_POOL_MAX + 8) {
            let a = (b'a' + (i / 26) as u8) as char;
            let b = (b'a' + (i % 26) as u8) as char;
            input.push_str(&format!("word{a}{b} "));
        }
        extract_pty_words(input.as_bytes(), &mut pool);
        assert_eq!(pool.len(), PTY_WORD_POOL_MAX);
    }
}
