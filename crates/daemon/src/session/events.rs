use super::*;

impl SessionManager {
    pub(crate) async fn handle_event(&self, entry: &Arc<SessionEntry>, event: SessionEvent) {
        // Skip everything once the session has been deleted — the drain task
        // and the adapter can still feed us events for a beat.
        if entry.is_deleted() {
            return;
        }
        // Operator-initiated shutdown: the adapter exiting may still
        // flush a `Done` / `Error` event (e.g. the shell adapter's
        // PTY emits `Done` when the wrapped process dies). Letting
        // those land would transition the session to terminal and
        // make `resume_running_sessions` skip it on the next boot,
        // defeating the whole point of the reconnectable-adapters
        // shutdown path. Drop all events during shutdown.
        if self.is_shutting_down.load(Ordering::Acquire) {
            return;
        }
        if matches!(event, SessionEvent::Reset) {
            if let Err(e) = self.storage.truncate_transcript(&entry.id) {
                tracing::warn!(session = %entry.id, error = ?e, "truncate_transcript on reset failed");
            }
            if let Err(e) = self.storage.truncate_pty_log(&entry.id) {
                tracing::warn!(session = %entry.id, error = ?e, "truncate_pty_log on reset failed");
            }
            entry.transcript_count.store(0, Ordering::Relaxed);
            entry.tasks.lock().await.clear();
            let now = Utc::now();
            let snapshot = {
                let mut s = entry.summary.write().await;
                s.last_event_at = Some(now);
                s.event_count = 0;
                s.last_pty_at_ms = None;
                s.state = SessionState::AwaitingInput;
                s.pending_input = true;
                s.clone()
            };
            let _ = self.storage.save_summary(&snapshot);
            let _ = self
                .broadcast
                .send(BroadcastMsg::State(StateNotificationPayload {
                    session: snapshot,
                }));
            let _ = self
                .broadcast
                .send(BroadcastMsg::Event(EventNotificationPayload {
                    session_id: entry.id.clone(),
                    at: now,
                    event,
                    seq: 0,
                }));
            return;
        }
        // OSC 11 background probes (spec 0073): when a connected client
        // paints the frame background, the daemon — the single authority in
        // front of the child PTY — answers the probe with that color,
        // writing the reply straight into the child's stdin. The query is
        // stripped from the downstream stream (transcript marker, pty.log,
        // broadcast) so no attached terminal emulator answers a second
        // time. Live adapter output only: replay reads pty.log, which never
        // contains the stripped probes.
        let mut event = event;
        if matches!(&event, SessionEvent::Pty { .. }) {
            if let Some(rgb) = self.effective_terminal_background() {
                let bytes = event.pty_bytes().unwrap_or_default();
                let (passthrough, count) = {
                    let mut tail = entry.osc11_tail.lock().expect("osc11_tail mutex poisoned");
                    agentd_protocol::osc11::scan_and_strip_queries(&mut tail, &bytes)
                };
                if count > 0 {
                    let response = agentd_protocol::osc11::response_bytes((rgb[0], rgb[1], rgb[2]));
                    // Boxed: pty_input can re-enter handle_event (captured
                    // input echo), which would otherwise make this future's
                    // type infinitely recursive.
                    if let Err(e) =
                        Box::pin(self.pty_input_without_capture(
                            &entry.id,
                            response.as_slice().repeat(count),
                        ))
                        .await
                    {
                        tracing::debug!(
                            session = %entry.id,
                            error = %e,
                            "osc11 background response failed",
                        );
                    }
                }
                if passthrough.is_empty() {
                    // The whole chunk was probe bytes (or a withheld query
                    // prefix) — nothing to persist or broadcast.
                    return;
                }
                if passthrough.len() != bytes.len() {
                    event = SessionEvent::pty(&passthrough);
                }
            }
        }
        // Persist smith/chat PTY bytes in the transcript as lightweight
        // ordering markers. PTY replay still comes from pty.log, but these
        // markers let a fresh TUI interleave transcript-only items (tool
        // blocks) with the raw byte stream at the right point after restart.
        if let SessionEvent::Pty { .. } = &event {
            let seq = entry.transcript_count.fetch_add(1, Ordering::Relaxed) + 1;
            let now = Utc::now();
            let ts = TimestampedEvent {
                seq,
                at: now,
                event: event.clone(),
            };
            if let Err(e) = self.storage.append_event(&entry.id, &ts) {
                tracing::warn!(session = %entry.id, error = ?e, "append PTY marker failed");
            }
        }

        // AgentStatus is ephemeral live UI state. The CLI may render
        // inactive statuses as display-only history rows, but they
        // should not enter the structured transcript or PTY log.
        if let SessionEvent::AgentStatus(_) = &event {
            let now = Utc::now();
            let seq = entry.transcript_count.load(Ordering::Relaxed);
            let _ = self
                .broadcast
                .send(BroadcastMsg::Event(EventNotificationPayload {
                    session_id: entry.id.clone(),
                    at: now,
                    event,
                    seq,
                }));
            return;
        }
        // BrowserPreview is ephemeral, live-only UI: a base64 PNG that
        // clients render as an overlay/wallpaper but never replay from the
        // transcript. Persisting it would bloat transcript.jsonl with
        // full-size screenshots (slowing every load, since `read_transcript`
        // parses every line) for no consumer, and leak the image into the
        // model via `agentd_get_transcript`. So broadcast to live clients
        // and return before `append_event` — same treatment as AgentStatus.
        if let SessionEvent::BrowserPreview(_) = &event {
            let now = Utc::now();
            let seq = entry.transcript_count.load(Ordering::Relaxed);
            let _ = self
                .broadcast
                .send(BroadcastMsg::Event(EventNotificationPayload {
                    session_id: entry.id.clone(),
                    at: now,
                    event,
                    seq,
                }));
            return;
        }
        // ToolApprovalResolved is a transient UI dismissal signal: it tells
        // passive viewers (web approval dialog, TUI minibuffer) that a
        // pending approval was answered — by any client — so they can close
        // their prompt. Like AgentStatus/BrowserPreview, broadcast it live
        // but never persist it to the transcript.
        if let SessionEvent::ToolApprovalResolved { .. } = &event {
            let now = Utc::now();
            let seq = entry.transcript_count.load(Ordering::Relaxed);
            let _ = self
                .broadcast
                .send(BroadcastMsg::Event(EventNotificationPayload {
                    session_id: entry.id.clone(),
                    at: now,
                    event,
                    seq,
                }));
            return;
        }
        // ApprovalModeChanged updates durable per-session state. The state
        // notification is enough for clients; do not record a transcript row.
        if let SessionEvent::ApprovalModeChanged { mode } = &event {
            if let Err(e) = self.persist_approval_mode(entry, *mode).await {
                tracing::warn!(
                    session = %entry.id,
                    error = ?e,
                    "persist approval mode from adapter event failed"
                );
            }
            return;
        }
        if let SessionEvent::OperatorLoopChanged { enabled } = &event {
            if let Err(e) = self.persist_operator_loop(entry, *enabled).await {
                tracing::warn!(
                    session = %entry.id,
                    error = ?e,
                    "persist operator loop from adapter event failed"
                );
            }
            return;
        }
        // ModelChanged updates the session's recorded model (durable
        // per-session state). The state notification carries the new label to
        // clients; like ApprovalModeChanged it is not a transcript row.
        if let SessionEvent::ModelChanged { model } = &event {
            if let Err(e) = self.persist_model(entry, model.clone()).await {
                tracing::warn!(
                    session = %entry.id,
                    error = ?e,
                    "persist model from adapter event failed"
                );
            }
            return;
        }
        // PTY events take a fast path: append to the on-disk pty.log + a
        // live broadcast. A copy was also appended to the transcript above
        // as an ordering marker. Replay reads back from `pty.log` directly
        // when a TUI attaches, so we no longer keep a parallel in-memory
        // ring of bytes.
        if let SessionEvent::Pty { .. } = &event {
            let mut is_active = true;
            if let Some(bytes) = event.pty_bytes() {
                if !agentd_protocol::is_pty_active_payload(&bytes) {
                    is_active = false;
                }
                if let Err(e) = self.storage.append_pty_bytes(&entry.id, &bytes) {
                    tracing::warn!(
                        session = %entry.id,
                        error = ?e,
                        "pty_log append failed",
                    );
                }
            }
            let now = Utc::now();
            let is_focused = self.focused_sessions.lock().unwrap().contains(&entry.id);
            // Track activity for the "session looks busy" signal, and undo a
            // quiescence-driven AwaitingInput when output resumes — so the
            // session reads as Running again and its marker clears.
            let resumed = if is_active {
                let now_ms = now.timestamp_millis();
                let mut s = entry.summary.write().await;
                let prev_pty_at_ms = s.last_pty_at_ms;
                s.last_pty_at_ms = Some(now_ms);
                // Quiescence-detected harnesses repaint status-line housekeeping
                // while idle (claude paints "Checking for updates" every 30
                // minutes and erases it half a second later). Byte-wise that is
                // real output; what distinguishes it is that it doesn't persist.
                // Only a burst that has kept producing for PTY_BLIP_WINDOW counts
                // as genuine activity — shorter blips must neither mark unseen
                // activity nor undo an AwaitingInput, or every idle unfocused
                // session re-raises its needs_attention dot on each repaint.
                // See spec 0054.
                let genuine = if harness_uses_quiescence(&s) {
                    let (burst_start, sustained) = pty_burst_advance(
                        entry.pty_burst_start_ms.load(Ordering::Relaxed),
                        prev_pty_at_ms,
                        now_ms,
                    );
                    entry
                        .pty_burst_start_ms
                        .store(burst_start, Ordering::Relaxed);
                    sustained
                } else {
                    true
                };
                // PTY output the operator isn't looking at is unseen activity — it's
                // what makes a later idle "need you". Output in the focused session
                // (their own keystrokes echoing) must not count. See spec 0054.
                if genuine && !is_focused {
                    entry.unseen_activity.store(true, Ordering::Relaxed);
                }
                if genuine && harness_uses_quiescence(&s) && s.state == SessionState::AwaitingInput
                {
                    s.state = SessionState::Running;
                    s.pending_input = false;
                    s.needs_attention = false;
                    Some(s.clone())
                } else {
                    None
                }
            } else {
                None
            };
            if let Some(snapshot) = resumed {
                let _ = self.storage.save_summary(&snapshot);
                let _ = self
                    .broadcast
                    .send(BroadcastMsg::State(StateNotificationPayload {
                        session: snapshot,
                    }));
            }
            // Latest seq for ordering only; not persisted.
            let seq = entry.transcript_count.load(Ordering::Relaxed);
            let _ = self
                .broadcast
                .send(BroadcastMsg::Event(EventNotificationPayload {
                    session_id: entry.id.clone(),
                    at: now,
                    event,
                    seq,
                }));
            return;
        }

        let seq = entry.transcript_count.fetch_add(1, Ordering::Relaxed) + 1;
        let now = Utc::now();
        let ts = TimestampedEvent {
            seq,
            at: now,
            event: event.clone(),
        };
        if let Err(e) = self.storage.append_event(&entry.id, &ts) {
            tracing::warn!(session = %entry.id, error = ?e, "append_event failed");
        }
        // Update summary based on event semantics.
        let is_focused = self.focused_sessions.lock().unwrap().contains(&entry.id);
        // Genuine activity in an unfocused session is what makes a later stop
        // "need you" — record it so the marker logic below can require it.
        if !is_focused && event_is_unseen_activity(&event) {
            entry.unseen_activity.store(true, Ordering::Relaxed);
        }
        {
            let mut s = entry.summary.write().await;
            s.last_event_at = Some(now);
            s.event_count = seq;
            let prev_state = s.state;
            match &event {
                SessionEvent::Status { state, .. } => {
                    s.state = *state;
                    s.pending_input = matches!(state, SessionState::AwaitingInput);
                }
                SessionEvent::AgentStatus(_) => {}
                SessionEvent::AwaitingInput { prompt } => {
                    s.state = SessionState::AwaitingInput;
                    s.pending_input = true;
                    if let Some(p) = prompt {
                        s.last_prompt = Some(p.clone());
                    }
                }
                SessionEvent::Cost { usd, .. } => {
                    s.cost_usd = Some(s.cost_usd.unwrap_or(0.0) + *usd);
                }
                SessionEvent::Done { exit_code } => {
                    s.state = if *exit_code == 0 {
                        SessionState::Done
                    } else {
                        SessionState::Errored
                    };
                    s.pending_input = false;
                }
                SessionEvent::Error { .. } => {
                    s.state = SessionState::Errored;
                    s.pending_input = false;
                }
                SessionEvent::Reset
                | SessionEvent::Message { .. }
                | SessionEvent::Reasoning { .. }
                | SessionEvent::ToolUse { .. }
                | SessionEvent::ToolResult { .. }
                | SessionEvent::Diff { .. }
                | SessionEvent::Pty { .. }
                | SessionEvent::PtyResize { .. }
                | SessionEvent::ToolApprovalRequest { .. }
                // Transient; handled by the broadcast-only fast path above.
                | SessionEvent::ToolApprovalResolved { .. }
                | SessionEvent::ApprovalModeChanged { .. }
                | SessionEvent::OperatorLoopChanged { .. }
                | SessionEvent::ModelChanged { .. }
                | SessionEvent::TaskStart { .. }
                | SessionEvent::TaskBackgrounded { .. }
                | SessionEvent::TaskEnd { .. }
                | SessionEvent::ContextCompacted { .. }
                | SessionEvent::BrowserPreview(_)
                | SessionEvent::UiPanel(_)
                | SessionEvent::UiDelete { .. }
                | SessionEvent::EditorState { .. }
                // ClientCommand is a UI-control action; it never moves the
                // session's top-level state. (Prototype: persistence still
                // goes through the default append above — the policy-driven
                // gate on `slash::TranscriptPolicy` is the follow-up wiring.)
                | SessionEvent::ClientCommand { .. } => {
                    // Task-lifecycle, editor-state, and compaction
                    // events are recorded by other handlers — they
                    // don't move the session's top-level state.
                }
            }
            // Maintain the sticky "needs you" marker off state transitions:
            // raise it when the session stops being Running (unless the operator
            // is already viewing it), clear it when it resumes. See spec 0054.
            if s.state != prev_state {
                match s.state {
                    SessionState::Running => s.needs_attention = false,
                    SessionState::AwaitingInput | SessionState::Done | SessionState::Errored => {
                        // Only flag if something happened while the operator
                        // wasn't looking — not their own input echo in a focused
                        // session they then switched away from. See spec 0054.
                        if !is_focused && entry.unseen_activity.load(Ordering::Relaxed) {
                            s.needs_attention = true;
                        }
                    }
                    SessionState::Pending | SessionState::Paused => {}
                }
            }
            let snapshot = s.clone();
            drop(s);
            let _ = self.storage.save_summary(&snapshot);
        }
        let new_state = {
            let s = entry.summary.read().await;
            s.state
        };
        self.note_session_state_for_program_run(&entry.id, new_state);

        if session_event_is_program_output(&event) {
            self.mark_program_run_output_seen(&entry.id);
        }
        // Update the per-session task registry from lifecycle events
        // so `session.list_tasks` has live state to return.
        match &event {
            SessionEvent::TaskStart {
                call_id,
                tool,
                args_summary,
            } => {
                let mut tasks = entry.tasks.lock().await;
                tasks.upsert_start(
                    call_id.clone(),
                    tool.clone(),
                    args_summary.clone(),
                    now.timestamp_millis(),
                );
            }
            SessionEvent::TaskBackgrounded { call_id } => {
                let mut tasks = entry.tasks.lock().await;
                tasks.mark_backgrounded(call_id, now.timestamp_millis());
            }
            SessionEvent::TaskEnd {
                call_id,
                ok,
                output_preview,
            } => {
                let mut tasks = entry.tasks.lock().await;
                tasks.mark_end(call_id, *ok, output_preview.clone(), now.timestamp_millis());
            }
            _ => {}
        }

        // Auto-title hook: feed every User message we record to
        // maybe_spawn_auto_title, regardless of where it came from (the
        // daemon's create() prompt-as-event, send_input, or an adapter
        // that re-emits the user's typed prompt — smith interactive
        // does this). Generation itself only fires on the first
        // non-slash-command message (leading `/model ...`-style
        // messages are accumulated as context, not treated as the
        // trigger); the `title_gen_attempted` AtomicBool inside
        // maybe_spawn_auto_title ensures only the first firing wins.
        if let SessionEvent::Message {
            role: MessageRole::User,
            text,
        } = &event
        {
            self.maybe_spawn_auto_title(entry.clone(), text.clone());
        }

        let _ = self
            .broadcast
            .send(BroadcastMsg::Event(EventNotificationPayload {
                session_id: entry.id.clone(),
                at: now,
                event,
                seq,
            }));

        // Also push a state snapshot so list views update without explicit refresh.
        let summary = entry.summary().await;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: summary,
            }));
    }
}

// Events that represent genuine session activity the operator would want to
// see, used to gate the `needs_attention` marker (spec 0054): a session going
// idle only flags when one of these arrived while it wasn't the focused one.
pub(super) fn event_is_unseen_activity(e: &SessionEvent) -> bool {
    matches!(
        e,
        SessionEvent::Pty { .. }
            | SessionEvent::Message { .. }
            | SessionEvent::Reasoning { .. }
            | SessionEvent::ToolUse { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::Diff { .. }
            | SessionEvent::Done { .. }
            | SessionEvent::Error { .. }
    )
}

fn session_event_is_program_output(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::Reasoning { .. }
            | SessionEvent::ToolUse { .. }
            | SessionEvent::ToolResult { .. }
            | SessionEvent::TaskStart { .. }
            | SessionEvent::Diff { .. }
            | SessionEvent::Message {
                role: MessageRole::Assistant,
                ..
            }
    )
}
