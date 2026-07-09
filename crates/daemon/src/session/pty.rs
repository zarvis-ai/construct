use super::*;

impl SessionManager {
    pub async fn pty_input(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, true).await
    }

    pub(crate) async fn pty_input_without_capture(&self, id: &str, bytes: Vec<u8>) -> Result<()> {
        self.pty_input_inner(id, bytes, false).await
    }

    async fn pty_input_inner(&self, id: &str, bytes: Vec<u8>, capture: bool) -> Result<()> {
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
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        let params = serde_json::to_value(&agentd_protocol::SessionPtyInputParams::from_bytes(
            id, &bytes,
        ))?;
        adapter
            .request(ahp_method::SESSION_PTY_INPUT, params)
            .await?;
        Ok(())
    }

    /// Feed PTY-input bytes through a minimal terminal-input parser (printable
    /// ASCII + backspace + CR/LF; CSI/SS3 sequences skipped) and return every
    /// submitted non-empty line. The parser is intentionally small: it is for
    /// transcript/user-title capture, not full terminal editing semantics.
    async fn capture_pty_input_lines(&self, entry: &Arc<SessionEntry>, bytes: &[u8]) -> Vec<String> {
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
        let adapter = entry
            .adapter
            .lock()
            .await
            .clone()
            .ok_or_else(|| anyhow!("session has no live adapter"))?;
        let params = serde_json::to_value(&agentd_protocol::SessionPtyResizeParams {
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

#[cfg(test)]
pub(super) fn pty_caps() -> agentd_protocol::Capabilities {
    agentd_protocol::Capabilities {
        supports_pty: true,
        supports_silent_resume: false,
        ..Default::default()
    }
}
