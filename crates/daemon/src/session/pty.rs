use super::*;
use std::sync::Weak;

/// Capacity of a session's ordered PTY-input delivery queue, counted in
/// input batches (one keystroke burst or one paste each), not bytes.
/// Interactive typing never comes close; the queue only fills when an
/// adapter stops ACKing `session.pty_input` for a long stretch, and then
/// failing the enqueue with a visible error beats silently buffering
/// unbounded input into a wedged session.
const PTY_INPUT_QUEUE_CAP: usize = 256;

/// One queued PTY-input batch (spec 0087). `ack` is present when the
/// enqueuer needs delivery confirmation — daemon-internal callers that
/// pace themselves against the adapter (bracketed-paste-then-Enter
/// submission, OSC 11 responses). The interactive typing path leaves it
/// `None`: "accepted into the ordered queue" is its whole contract.
pub(crate) struct PtyInputJob {
    bytes: Vec<u8>,
    ack: Option<tokio::sync::oneshot::Sender<Result<()>>>,
}

impl SessionManager {
    /// Interactive typing path (`server::dispatch`'s `SESSION_PTY_INPUT`).
    /// Returns once the bytes are accepted into the session's ordered
    /// delivery queue — NOT once the adapter ACKs delivery (spec 0087).
    /// The dispatch loop serves each connection's requests serially, and
    /// clients pump keystrokes one request at a time, so awaiting the
    /// adapter round-trip here let a single slow/starved adapter stall
    /// typing into every session plus everything else queued on the
    /// connection. Delivery failures after enqueue are logged, not
    /// returned; typing into a session with no live adapter still fails
    /// synchronously, while also closing it so the client can restart it.
    pub async fn pty_input(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, true, false).await
    }

    /// Like [`Self::pty_input`] (transcript capture included) but waits
    /// for the adapter to ACK delivery. For daemon-internal prompt
    /// submission (program runs) whose follow-up bookkeeping should only
    /// happen once the input has actually reached the harness.
    pub(crate) async fn pty_input_delivered(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, true, true).await
    }

    /// Delivery-ACKed input without transcript capture, for daemon-internal
    /// byte streams that must not pollute the user transcript (OSC 11
    /// responses, bracketed-paste submission). Waits for the adapter ACK:
    /// its callers sequence real-time behavior against delivery (e.g. the
    /// paste → settle-delay → Enter submission dance).
    pub(crate) async fn pty_input_without_capture(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, false, true).await
    }

    async fn pty_input_inner(
        &self,
        id: &str,
        bytes: Vec<u8>,
        capture: bool,
        await_delivery: bool,
    ) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        // Capture submitted PTY lines before forwarding them. Some interactive
        // harnesses do not echo user text as structured `Message` events, so
        // chat-mode transcript history otherwise loses those turns.
        if capture {
            let input_lines = self.capture_pty_input_lines(&entry, &bytes).await;
            let harness = entry.summary.read().await.harness.clone();
            for line in input_lines {
                if should_record_pty_user_message(&harness) {
                    self.handle_event(
                        &entry,
                        SessionEvent::Message {
                            role: MessageRole::User,
                            text: line,
                        },
                    )
                    .await;
                } else if !entry.title_gen_attempted.load(Ordering::SeqCst)
                    && line.chars().count() >= 2
                {
                    self.maybe_spawn_auto_title(entry.clone(), line);
                }
            }
        }
        // Typing into a session whose adapter is gone must still fail
        // synchronously: once the enqueue-ACK path returns Ok there is no
        // response channel left to report a delivery error through. Best
        // effort — the adapter can still die between this check and
        // delivery, which the writer then logs.
        self.live_adapter_or_mark_closed(&entry).await?;
        if await_delivery {
            let (tx, rx) = tokio::sync::oneshot::channel();
            self.enqueue_pty_input(
                &entry,
                PtyInputJob {
                    bytes,
                    ack: Some(tx),
                },
            )?;
            rx.await
                .map_err(|_| anyhow!("session closed before pty input was delivered"))?
        } else {
            self.enqueue_pty_input(&entry, PtyInputJob { bytes, ack: None })
        }
    }

    /// Accept `job` into `entry`'s ordered delivery queue, lazily spawning
    /// the per-session writer task on first use. All input producers —
    /// interactive typing, program submission, OSC 11 responses — funnel
    /// through this one queue, so per-session byte order is preserved no
    /// matter which mix of paths is active. Sync (never awaits): callers
    /// hold no locks and the dispatch loop is never delayed here.
    fn enqueue_pty_input(&self, entry: &Arc<SessionEntry>, job: PtyInputJob) -> Result<()> {
        let mut slot = entry
            .pty_input_queue
            .lock()
            .expect("pty_input_queue mutex poisoned");
        if slot.is_none() {
            let (tx, rx) = mpsc::channel::<PtyInputJob>(PTY_INPUT_QUEUE_CAP);
            tokio::spawn(pty_input_writer(Arc::downgrade(entry), rx));
            *slot = Some(tx);
        }
        slot.as_ref()
            .expect("sender installed above")
            .try_send(job)
            .map_err(|e| match e {
                mpsc::error::TrySendError::Full(_) => anyhow!(
                    "pty input backlogged: the session's adapter is not consuming input"
                ),
                mpsc::error::TrySendError::Closed(_) => {
                    anyhow!("session closed before pty input could be queued")
                }
            })
    }

    /// Feed PTY-input bytes through a minimal terminal-input parser (printable
    /// ASCII + backspace + CR/LF; CSI/SS3 sequences skipped) and return every
    /// submitted non-empty line. The parser is intentionally small: it is for
    /// transcript/user-title capture, not full terminal editing semantics.
    async fn capture_pty_input_lines(
        &self,
        entry: &Arc<SessionEntry>,
        bytes: &[u8],
    ) -> Vec<String> {
        let mut cap = entry.pty_input_capture.lock().await;
        let mut lines = Vec::new();
        for &b in bytes {
            match cap.esc {
                0 => match b {
                    b'\n' if cap.last_was_cr => {
                        cap.last_was_cr = false;
                    }
                    b'\r' | b'\n' => {
                        let s = cap.buf.trim().to_string();
                        cap.last_was_cr = b == b'\r';
                        cap.buf.clear();
                        if s.chars().count() >= 2 {
                            lines.push(s);
                        }
                    }
                    0x1b => cap.esc = 1,
                    0x08 | 0x7f => {
                        cap.last_was_cr = false;
                        cap.buf.pop();
                    }
                    _ if (0x20..0x7f).contains(&b) => {
                        cap.last_was_cr = false;
                        cap.buf.push(b as char);
                    }
                    _ => {
                        cap.last_was_cr = false;
                    }
                },
                1 => match b {
                    b'[' => cap.esc = 2,
                    b'O' => cap.esc = 3,
                    _ => cap.esc = 0,
                },
                2 => {
                    // CSI: parameter bytes + final byte in `@`..=`~`.
                    if (0x40..=0x7e).contains(&b) {
                        cap.esc = 0;
                    }
                }
                3 => {
                    // SS3: one byte.
                    cap.esc = 0;
                }
                _ => cap.esc = 0,
            }
        }
        lines
    }

    /// Record that a given client kind just acted on a session's
    /// PTY (typed input or sent a resize). Updates the kind's
    /// last-known viewport (if `resize_to` was supplied), flips
    /// `last_active` to that kind, and — if the kind switched
    /// since last time — issues a `pty_resize` to match the kind's
    /// stored viewport. No-op when only one kind is attached.
    ///
    /// This is the daemon-side half of the "active client wins"
    /// PTY-size policy. The complementary half lives in
    /// `server::dispatch`'s `SESSION_PTY_INPUT` and
    /// `SESSION_PTY_RESIZE` arms, which call this method before
    /// forwarding the actual request to the PTY.
    pub async fn note_pty_activity(
        self: &Arc<Self>,
        id: &str,
        kind: crate::server::ClientKind,
        resize_to: Option<(u16, u16)>,
    ) {
        let Some(entry) = self.get_entry(id).await else {
            return;
        };
        let to_apply = {
            let mut policy = entry
                .pty_client_policy
                .lock()
                .expect("pty_client_policy mutex poisoned");
            if let Some(sz) = resize_to {
                match kind {
                    crate::server::ClientKind::Tui => policy.tui_size = Some(sz),
                    crate::server::ClientKind::Remote => policy.remote_size = Some(sz),
                }
            }
            let switched = policy.last_active != Some(kind);
            policy.last_active = Some(kind);
            // Only re-resize on a *switch*, or when this call was
            // itself a pty_resize. Plain pty_input from the same
            // kind that's already active is a no-op for the size
            // policy (the per-call pty_resize handler still runs
            // separately).
            if switched || resize_to.is_some() {
                match kind {
                    crate::server::ClientKind::Tui => policy.tui_size,
                    crate::server::ClientKind::Remote => policy.remote_size,
                }
            } else {
                None
            }
        };
        if let Some((cols, rows)) = to_apply {
            // Best-effort. The pty_resize dedup inside
            // `SessionManager::pty_resize` handles the case where
            // the OS PTY is already at this size.
            if let Err(e) = self.pty_resize(id, cols, rows).await {
                tracing::debug!(session = %id, error = %e, "policy-driven pty_resize failed");
            }
        }
    }

    pub async fn pty_resize(&self, id: &str, cols: u16, rows: u16) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let size = PtySize { cols, rows };
        // Dedup: if the adapter's PTY is already at this size, skip
        // the SIGWINCH. A no-op resize on a normal-screen TUI like
        // codex still causes the child to redraw its viewport (which
        // for codex means re-emitting its full transcript), so every
        // spurious resize looks like a "history replay" to the user.
        // Sources of spurious resizes: TUI bootstrap calling
        // `pty_resize` with the same dims it already sent, and
        // multiple SIGWINCH'd frames during a terminal-window drag
        // that all land on the same final size.
        {
            let mut pty = entry.pty.lock().await;
            if pty.size == Some(size) {
                return Ok(());
            }
            pty.size = Some(size);
        }
        // Cache the size so the next daemon respawn can re-spawn the
        // adapter's PTY at the right dimensions from the start.
        if let Err(e) = self.storage.save_pty_size(id, size) {
            tracing::warn!(session = %id, error = ?e, "save_pty_size failed");
        }
        // Tell other attached clients the new geometry (transient, not
        // persisted) so a passive viewer (e.g. a narrower web terminal) can
        // render at the real width instead of wrapping. Only fires on an
        // actual change — the dedup above already returned for a no-op.
        self.broadcast_widget_event(id, SessionEvent::PtyResize { cols, rows });
        let adapter = self.live_adapter_or_mark_closed(&entry).await?;
        let params = serde_json::to_value(&construct_protocol::SessionPtyResizeParams {
            session_id: id.to_string(),
            cols,
            rows,
        })?;
        adapter
            .request(ahp_method::SESSION_PTY_RESIZE, params)
            .await?;
        Ok(())
    }

    pub async fn pty_replay(&self, id: &str) -> Result<PtyReplayResult> {
        self.pty_replay_range(id, None, None).await
    }

    pub async fn pty_replay_range(
        &self,
        id: &str,
        max_bytes: Option<usize>,
        before_offset: Option<u64>,
    ) -> Result<PtyReplayResult> {
        use base64::Engine;
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        let size = entry.pty.lock().await.size;
        // Pull scrollback from the on-disk `pty.log`, not the (now-removed)
        // in-memory ring. Requests are capped by `PTY_REPLAY_CAP`; clients can
        // ask for older adjacent ranges and replay their local chunks in order.
        let requested = max_bytes.unwrap_or(PTY_REPLAY_CAP).min(PTY_REPLAY_CAP);
        let (bytes, start_offset, end_offset, total_bytes) = self
            .storage
            .read_pty_range_before(id, requested, before_offset)
            .unwrap_or_else(|e| {
                tracing::warn!(session = %id, error = ?e, "pty_log range read failed");
                (Vec::new(), 0, 0, 0)
            });
        Ok(PtyReplayResult {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
            start_offset,
            end_offset,
            total_bytes,
            size,
        })
    }

    /// Deliver a prompt as a bracketed paste (`ESC[200~` … `ESC[201~`) when
    /// submitting to external PTY-backed agents.
    pub(super) async fn program_submit_typed_prompt(&self, id: &str, prompt: &str) -> Result<()> {
        // Deliver the prompt as a bracketed paste (`ESC[200~` … `ESC[201~`)
        // rather than raw keystrokes. External agent TUIs (claude/codex/
        // antigravity) enable DEC mode 2004 and only run their multiline
        // guard on a real bracketed paste: framed this way they buffer the
        // whole multi-line body as one input instead of submitting on the
        // first embedded newline. Crucially the `ESC[201~` end marker tells
        // the harness exactly where the paste stops, so the Enter we send
        // afterward is read as a submit keypress — without it the prompt
        // landed in the input box but never submitted.
        self.pty_input_without_capture(id, program_bracketed_paste_bytes(prompt))
            .await?;
        tokio::time::sleep(PROGRAM_EXTERNAL_PTY_SUBMIT_DELAY).await;
        self.pty_input_without_capture(id, vec![b'\r']).await?;
        Ok(())
    }
}

/// Per-session writer task (spec 0087): drains the ordered input queue and
/// performs the adapter `session.pty_input` round-trips that used to run
/// inline in the IPC dispatch loop. One writer per session, spawned on the
/// first input; exits on its own when the owning `SessionEntry` is dropped
/// (the queue's sender lives on the entry, so the channel closes and
/// `recv` returns `None`) — delete/restart need no explicit teardown.
/// Holds only a `Weak` to the entry so a torn-down session's memory isn't
/// kept alive by its own input queue.
async fn pty_input_writer(entry: Weak<SessionEntry>, mut rx: mpsc::Receiver<PtyInputJob>) {
    while let Some(job) = rx.recv().await {
        let Some(entry) = entry.upgrade() else { break };
        let result = deliver_pty_input(&entry, &job.bytes).await;
        match job.ack {
            // A dropped receiver means the awaiting caller was cancelled;
            // delivery already happened, so there is nothing to report.
            Some(ack) => {
                let _ = ack.send(result);
            }
            None => {
                if let Err(e) = result {
                    tracing::warn!(session = %entry.id, error = %e, "pty input delivery failed");
                }
            }
        }
    }
}

/// One adapter round-trip for one queued batch. Fetches the adapter at
/// delivery time — not enqueue time — so input queued across an adapter
/// respawn reaches the new adapter instead of erroring on the dead one.
async fn deliver_pty_input(entry: &Arc<SessionEntry>, bytes: &[u8]) -> Result<()> {
    let adapter = entry
        .adapter
        .lock()
        .await
        .clone()
        .ok_or_else(|| anyhow!("session has no live adapter"))?;
    let params = serde_json::to_value(&construct_protocol::SessionPtyInputParams::from_bytes(
        &entry.id, bytes,
    ))?;
    adapter
        .request(ahp_method::SESSION_PTY_INPUT, params)
        .await?;
    Ok(())
}

#[cfg(test)]
pub(super) fn pty_caps() -> construct_protocol::Capabilities {
    construct_protocol::Capabilities {
        supports_pty: true,
        supports_silent_resume: false,
        ..Default::default()
    }
}
