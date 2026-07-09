use super::*;

impl App {
    pub(super) async fn handle_minibuffer_key(&mut self, key: KeyEvent) {
        // Snapshot the data we'll need without holding a borrow on
        // self.minibuffer across the (possibly &self) lookups.
        let is_new_harness = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::NewSessionHarness)
        );
        let is_fork_harness = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::ForkSessionHarness { .. })
        );
        // Fork shares the new-session harness picker (completion + Enter
        // validation), but offers only real harnesses — no `project`/`group`.
        let is_harness_picker = is_new_harness || is_fork_harness;
        let available_harnesses: Vec<String> = if is_new_harness {
            let mut v: Vec<String> = self
                .harnesses
                .iter()
                .filter(|h| h.available)
                .map(|h| h.name.clone())
                .collect();
            v.push("project".to_string());
            v.push("group".to_string());
            v
        } else if is_fork_harness {
            self.harnesses
                .iter()
                .filter(|h| h.available)
                .map(|h| h.name.clone())
                .collect()
        } else {
            Vec::new()
        };

        // Restart confirmation: single-key dispatch (`y` confirms,
        // anything else cancels) so the user can press one key and
        // move on, matching the way they invoked the prompt with a
        // single Enter on the Done session.
        let restart_intent = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::RestartConfirm { .. })
        );
        if restart_intent {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            // Pull the session_id out so we can drop the minibuffer
            // borrow before we await the client call.
            let session_id = match self.minibuffer.as_ref().map(|m| &m.intent) {
                Some(MinibufferIntent::RestartConfirm { session_id }) => session_id.clone(),
                _ => return,
            };
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.minibuffer = None;
                    match self.client.restart(&session_id).await {
                        Ok(()) => {
                            self.editor_states.remove(&session_id);
                            self.agent_statuses.remove(&session_id);
                            self.browser_previews.remove(&session_id);
                            self.set_status(format!("restarted {}", short_id(&session_id)));
                        }
                        Err(e) => self.set_status(format!("restart failed: {e}")),
                    }
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.minibuffer = None;
                    self.set_status("restart cancelled".to_string());
                }
                KeyCode::Char('g') if ctrl => {
                    self.minibuffer = None;
                    self.set_status("restart cancelled".to_string());
                }
                _ => {
                    // Ignore other keys so a stray keystroke doesn't
                    // accidentally cancel the prompt mid-thought.
                }
            }
            return;
        }

        // Restart-daemon confirmation (clicked from the status-bar version
        // notice): same single-key dispatch as the per-session restart above.
        let restart_daemon_intent = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::RestartDaemonConfirm)
        );
        if restart_daemon_intent {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.minibuffer = None;
                    let result = self.client.daemon_restart(None, false).await;
                    self.set_status(daemon_restart_status_message(result, "daemon restart"));
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.minibuffer = None;
                    self.set_status("daemon restart cancelled".to_string());
                }
                KeyCode::Char('g') if ctrl => {
                    self.minibuffer = None;
                    self.set_status("daemon restart cancelled".to_string());
                }
                _ => {}
            }
            return;
        }

        // Upgrade confirmation (clicked from the status-bar "<version>
        // available" notice): same single-key dispatch pattern.
        let upgrade_intent = match self.minibuffer.as_ref().map(|m| &m.intent) {
            Some(MinibufferIntent::UpgradeConfirm { version }) => Some(version.clone()),
            _ => None,
        };
        if let Some(version) = upgrade_intent {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            match key.code {
                KeyCode::Char('y') | KeyCode::Char('Y') | KeyCode::Enter => {
                    self.minibuffer = None;
                    self.start_upgrade(version);
                }
                KeyCode::Esc | KeyCode::Char('n') | KeyCode::Char('N') => {
                    self.minibuffer = None;
                    self.set_status("upgrade cancelled".to_string());
                }
                KeyCode::Char('g') if ctrl => {
                    self.minibuffer = None;
                    self.set_status("upgrade cancelled".to_string());
                }
                _ => {}
            }
            return;
        }

        // Approval prompt has single-key shortcuts; bypass the normal
        // editing path so the user can hit y/n/a without typing + Enter.
        let approve_intent = matches!(
            self.minibuffer.as_ref().map(|m| &m.intent),
            Some(MinibufferIntent::ApproveTool { .. })
        );
        if approve_intent {
            let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
            let decision = match key.code {
                KeyCode::Char('y') | KeyCode::Enter => Some("approve"),
                KeyCode::Char('n') | KeyCode::Esc => Some("deny"),
                KeyCode::Char('a') => match self.minibuffer.as_ref().map(|m| &m.intent) {
                    Some(MinibufferIntent::ApproveTool {
                        allow_auto_review: true,
                        ..
                    }) => Some("auto_review"),
                    _ => None,
                },
                KeyCode::Char('f') => Some("unsafe_auto"),
                KeyCode::Char('g') if ctrl => Some("deny"),
                _ => None,
            };
            if let Some(d) = decision {
                if let Some(MinibufferIntent::ApproveTool {
                    session_id,
                    call_id,
                    ..
                }) = self.minibuffer.as_ref().map(|m| m.intent.clone())
                {
                    self.minibuffer = None;
                    match self.client.tool_decision(&session_id, call_id, d).await {
                        Ok(()) => {
                            self.matrix_rain.observe_tool_decision(
                                d,
                                self.matrix_rain_intensity,
                                &session_id,
                            );
                            self.set_status(format!("tool {d}"));
                        }
                        Err(e) => self.set_status(format!("tool_decision failed: {e}")),
                    }
                }
            }
            return;
        }

        let Some(mb) = self.minibuffer.as_mut() else {
            return;
        };
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);

        match key.code {
            KeyCode::Esc => {
                self.minibuffer = None;
                return;
            }
            KeyCode::Char('g') if ctrl => {
                self.minibuffer = None;
                return;
            }
            KeyCode::Tab => {
                if is_harness_picker {
                    apply_harness_completion(mb, &available_harnesses);
                }
                return;
            }
            KeyCode::Enter => {
                if is_harness_picker {
                    let trimmed = mb.input.trim().to_string();
                    if trimmed.is_empty() {
                        mb.error = Some("pick a harness".to_string());
                        return;
                    }
                    if !available_harnesses.iter().any(|h| h == &trimmed) {
                        mb.error = Some(format!("unknown: {trimmed} (Tab to complete)"));
                        return;
                    }
                }
                let intent = mb.intent.clone();
                let input = std::mem::take(&mut mb.input);
                self.minibuffer = None;
                self.run_minibuffer_submit(intent, input).await;
                return;
            }
            KeyCode::Backspace => {
                delete_back_char(mb);
            }
            KeyCode::Delete => {
                delete_forward_char(mb);
            }
            KeyCode::Left if alt => mb.cursor = word_back(&mb.input, mb.cursor),
            KeyCode::Right if alt => mb.cursor = word_forward(&mb.input, mb.cursor),
            KeyCode::Left => mb.cursor = mb.cursor.saturating_sub(1),
            KeyCode::Right => {
                if mb.cursor < mb.input.chars().count() {
                    mb.cursor += 1;
                }
            }
            KeyCode::Home => mb.cursor = 0,
            KeyCode::End => mb.cursor = mb.input.chars().count(),

            // Emacs editing chords on Ctrl.
            KeyCode::Char('a') if ctrl => mb.cursor = 0,
            KeyCode::Char('e') if ctrl => mb.cursor = mb.input.chars().count(),
            KeyCode::Char('b') if ctrl => mb.cursor = mb.cursor.saturating_sub(1),
            KeyCode::Char('f') if ctrl => {
                if mb.cursor < mb.input.chars().count() {
                    mb.cursor += 1;
                }
            }
            KeyCode::Char('d') if ctrl => delete_forward_char(mb),
            KeyCode::Char('h') if ctrl => delete_back_char(mb),
            KeyCode::Char('k') if ctrl => {
                let pos = byte_pos(&mb.input, mb.cursor);
                mb.input.truncate(pos);
                mb.error = None;
            }
            KeyCode::Char('u') if ctrl => {
                let pos = byte_pos(&mb.input, mb.cursor);
                mb.input.replace_range(..pos, "");
                mb.cursor = 0;
                mb.error = None;
            }
            KeyCode::Char('w') if ctrl => kill_word_back(mb),

            // Emacs editing chords on Meta.
            KeyCode::Char('b') if alt => mb.cursor = word_back(&mb.input, mb.cursor),
            KeyCode::Char('f') if alt => mb.cursor = word_forward(&mb.input, mb.cursor),
            KeyCode::Char('d') if alt => kill_word_forward(mb),

            // Plain printable insertion. Ignore anything with Ctrl/Alt that
            // wasn't handled above so stray modifier combos don't pollute
            // the input.
            KeyCode::Char(c) if !ctrl && !alt => {
                insert_minibuffer_text(mb, &c.to_string());
            }
            _ => {}
        }
    }

    pub(super) async fn run_minibuffer_submit(&mut self, intent: MinibufferIntent, input: String) {
        // Tutorial hook — spec 0077 step 3 ("say something to it"): a
        // headless (non-PTY) session sends input through this exact submit
        // path, which never resolves to a `KeyAction` or a distinguishable
        // notification. Kept thin; logic lives in `app/tutorial.rs`.
        self.tutorial_observe_minibuffer_submit(&intent, &input);
        match intent {
            MinibufferIntent::SendInput { session_id } => {
                if input.is_empty() {
                    return;
                }
                match self.client.send_input(&session_id, input).await {
                    Ok(()) => self.set_status("input sent".to_string()),
                    Err(e) => self.set_status(format!("send failed: {e}")),
                }
            }
            MinibufferIntent::NewSessionHarness => {
                let harness = input.trim().to_string();
                if harness.is_empty() {
                    return;
                }
                // `project` is a synthetic option in the harness picker that
                // redirects to the project-create flow. Keep `group` as a
                // compatibility alias for muscle memory.
                if harness == "project" || harness == "group" {
                    self.minibuffer = Some(Minibuffer {
                        prompt: "Project name: ".to_string(),
                        input: String::new(),
                        cursor: 0,
                        intent: MinibufferIntent::NewGroupName,
                        error: None,
                    });
                    return;
                }
                let cwd = std::env::current_dir()
                    .map(|p| p.to_string_lossy().to_string())
                    .unwrap_or_else(|_| ".".to_string());
                // Inherit the group context from the current selection
                // so creating a session "while inside" a group keeps
                // the new session in that same group.
                let group_id = match &self.selection {
                    Selection::Group(gid) => Some(gid.clone()),
                    Selection::Session(sid) => self
                        .sessions
                        .iter()
                        .find(|s| s.id == *sid)
                        .and_then(|s| s.group_id.clone()),
                    // Inherit the archived row's section so a new session
                    // created "inside" a project stays in it.
                    Selection::ArchivedRow(ArchiveSection::Group(gid)) => Some(gid.clone()),
                    Selection::ArchivedRow(
                        ArchiveSection::Ungrouped | ArchiveSection::Subagents(_),
                    )
                    | Selection::None => None,
                };
                let params = agentd_protocol::CreateSessionParams {
                    harness: harness.clone(),
                    cwd,
                    prompt: None,
                    model: None,
                    title: None,
                    mode: None,
                    pty_size: Some(agentd_protocol::PtySize {
                        cols: self.active_pane_size().0.max(20),
                        rows: self.active_pane_size().1.max(5),
                    }),
                    worktree: false,
                    env: HashMap::new(),
                    args: Vec::new(),
                    kind: agentd_protocol::SessionKind::User,
                    parent_session_id: None,
                    group_id,
                    position_after_session_id: None,
                    forked_from: None,
                };
                match self.client.create(params).await {
                    Ok(id) => {
                        self.set_status(format!("created {}", short_id(&id)));
                        self.refresh_sessions().await;
                        // Pre-insert an empty PTY parser so the subsequent
                        // `refresh_selected_transcript → bootstrap_terminal`
                        // short-circuits (parser already present). Our live
                        // subscription will deliver every byte the adapter
                        // emits; without this short-circuit, pty_replay
                        // would race the subscription and the banner ends
                        // up rendered twice (once from the ring, once from
                        // the live broadcast that was already in flight).
                        if !self.histories.contains_key(&id) {
                            self.histories
                                .insert(id.clone(), crate::pty_render::ItemHistory::new());
                        }
                        self.select_session(id);
                        self.sync_active_window_selection();
                        self.focus = PaneFocus::View;
                    }
                    Err(e) => self.set_status(format!("create failed: {e}")),
                }
            }
            MinibufferIntent::ForkSessionHarness { source_session_id } => {
                let harness = input.trim().to_string();
                if harness.is_empty() {
                    return;
                }
                // Default options: seed the fork with the full source
                // transcript (skipped for `shell` inside the client).
                let mut opts = agentd_client::ForkOptions::default();
                let (cols, rows) = self.active_pane_size();
                opts.pty_size = Some(agentd_protocol::PtySize {
                    cols: cols.max(20),
                    rows: rows.max(5),
                });
                match self
                    .client
                    .fork_session(&source_session_id, &harness, opts)
                    .await
                {
                    Ok(id) => {
                        self.set_status(format!(
                            "forked {} → {} ({harness})",
                            short_id(&source_session_id),
                            short_id(&id),
                        ));
                        self.refresh_sessions().await;
                        // Mirror the new-session path: pre-insert an empty PTY
                        // parser so the transcript bootstrap short-circuits and
                        // the live subscription isn't raced into a double banner.
                        if !self.histories.contains_key(&id) {
                            self.histories
                                .insert(id.clone(), crate::pty_render::ItemHistory::new());
                        }
                        self.select_session(id);
                        self.sync_active_window_selection();
                        self.focus = PaneFocus::View;
                    }
                    Err(e) => self.set_status(format!("fork failed: {e}")),
                }
            }
            MinibufferIntent::MergeMenu { session_id } => {
                let choice = input.trim().to_ascii_lowercase();
                let Some(fork) = self.sessions.iter().find(|s| s.id == session_id).cloned() else {
                    return;
                };
                let Some(parent) = fork.forked_from.as_ref().map(|f| f.session_id.clone()) else {
                    return;
                };
                let mode = match choice.as_str() {
                    "result" | "take result" => agentd_protocol::ForkMergeMode::Result,
                    "discard" | "d" => agentd_protocol::ForkMergeMode::Discard,
                    _ => {
                        self.set_status("merge: type result or discard".into());
                        return;
                    }
                };
                if mode == agentd_protocol::ForkMergeMode::Result {
                    match self.client.transcript(&session_id, 0, None).await {
                        Ok(tr) => {
                            if let Some(summary) =
                                agentd_client::render_fork_seed_for_merge(&tr.events, 6000)
                            {
                                let title = fork.title.as_deref().unwrap_or("fork");
                                if let Err(e) = self
                                    .client
                                    .send_input(
                                        &parent,
                                        format!("⑂ fork result ({title}): {summary}"),
                                    )
                                    .await
                                {
                                    self.set_status(format!("merge input failed: {e}"));
                                    return;
                                }
                            }
                        }
                        Err(e) => {
                            self.set_status(format!("merge transcript failed: {e}"));
                            return;
                        }
                    }
                }
                if let Err(e) = self.client.merge(&session_id, mode).await {
                    self.set_status(format!("merge failed: {e}"));
                    return;
                }
                if let Err(e) = self.client.archive(&session_id).await {
                    self.set_status(format!("archive failed: {e}"));
                    return;
                }
                self.refresh_sessions().await;
                self.select_session(parent);
                self.set_status("fork merged".into());
            }
            MinibufferIntent::GroupDeleteConfirm { group_id } => {
                let choice = parse_group_delete_choice(&input);
                let delete_members = match choice {
                    GroupDeleteChoice::Cancel => {
                        self.set_status("project delete cancelled".to_string());
                        return;
                    }
                    GroupDeleteChoice::OrphanMembers => false,
                    GroupDeleteChoice::DeleteMembers => true,
                };
                match self.client.delete_project(&group_id, delete_members).await {
                    Ok(()) => {
                        let msg = if delete_members {
                            "project + all sessions deleted"
                        } else {
                            "project deleted (members orphaned)"
                        };
                        self.set_status(msg.into());
                    }
                    Err(e) => self.set_status(format!("project delete failed: {e}")),
                }
            }
            MinibufferIntent::GroupRename { group_id } => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    self.set_status("project rename cancelled (empty)".into());
                    return;
                }
                match self.client.rename_project(&group_id, &trimmed).await {
                    Ok(()) => {
                        if let Some(g) = self.groups.iter_mut().find(|g| g.id == group_id) {
                            g.name = trimmed.clone();
                        }
                        self.set_status(format!("renamed project -> {trimmed}"));
                    }
                    Err(e) => self.set_status(format!("project rename failed: {e}")),
                }
            }
            MinibufferIntent::NewGroupName => {
                let trimmed = input.trim().to_string();
                if trimmed.is_empty() {
                    self.set_status("project name empty".into());
                    return;
                }
                match self.client.create_project(&trimmed).await {
                    Ok(id) => {
                        self.set_status(format!("created project '{trimmed}'"));
                        self.refresh_sessions().await; // also refreshes groups
                        self.select_group(id);
                    }
                    Err(e) => self.set_status(format!("project create failed: {e}")),
                }
            }
            MinibufferIntent::Rename { session_id } => {
                let trimmed = input.trim().to_string();
                let new_title = if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed)
                };
                match self.client.set_title(&session_id, new_title.clone()).await {
                    Ok(()) => {
                        // Optimistically reflect locally.
                        if let Some(i) = self.sessions.iter().position(|s| s.id == session_id) {
                            self.sessions[i].title = new_title.clone();
                        }
                        self.set_status(match &new_title {
                            Some(t) => format!("renamed → {t}"),
                            None => "title cleared".into(),
                        });
                    }
                    Err(e) => self.set_status(format!("rename failed: {e}")),
                }
            }
            MinibufferIntent::DeleteConfirm { session_id } => {
                match parse_session_end_choice(&input) {
                    SessionEndChoice::Delete => match self.client.delete(&session_id).await {
                        Ok(()) => self.set_status(format!("deleted {}", short_id(&session_id))),
                        Err(e) => self.set_status(format!("delete failed: {e}")),
                    },
                    SessionEndChoice::Archive => match self.client.archive(&session_id).await {
                        Ok(()) => self.set_status(format!("archived {}", short_id(&session_id))),
                        Err(e) => self.set_status(format!("archive failed: {e}")),
                    },
                    SessionEndChoice::Cancel => {
                        self.set_status("cancelled".to_string());
                    }
                }
            }
            MinibufferIntent::ArchivedDeleteConfirm { section } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("archived delete cancelled".to_string());
                    return;
                }
                // Re-resolve members at confirm time so a session that
                // un-archived or moved out of the section since the prompt
                // opened isn't deleted out from under the user.
                let ids = self.archived_sessions_in_section(&section);
                if ids.is_empty() {
                    self.set_status("no archived sessions to delete".to_string());
                    return;
                }
                let mut deleted = 0usize;
                let mut failed = 0usize;
                for id in &ids {
                    match self.client.delete(id).await {
                        Ok(()) => deleted += 1,
                        Err(e) => {
                            failed += 1;
                            tracing::warn!(session = %id, error = %e, "archived cascade-delete failed");
                        }
                    }
                }
                if failed == 0 {
                    self.set_status(format!("deleted {deleted} archived session(s)"));
                } else {
                    self.set_status(format!(
                        "deleted {deleted} archived session(s), {failed} failed"
                    ));
                }
            }
            MinibufferIntent::MenuArchiveConfirm { session_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("archive cancelled".to_string());
                    return;
                }
                match self.client.archive(&session_id).await {
                    Ok(()) => self.set_status(format!("archived {}", short_id(&session_id))),
                    Err(e) => self.set_status(format!("archive failed: {e}")),
                }
            }
            MinibufferIntent::MenuDeleteConfirm { session_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("delete cancelled".to_string());
                    return;
                }
                match self.client.delete(&session_id).await {
                    Ok(()) => self.set_status(format!("deleted {}", short_id(&session_id))),
                    Err(e) => self.set_status(format!("delete failed: {e}")),
                }
            }
            MinibufferIntent::MenuUnarchiveConfirm { session_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("unarchive cancelled".to_string());
                    return;
                }
                match self.client.restart(&session_id).await {
                    Ok(()) => self.set_status(format!("unarchived {}", short_id(&session_id))),
                    Err(e) => self.set_status(format!("unarchive failed: {e}")),
                }
            }
            MinibufferIntent::RestartConfirm { session_id } => {
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("restart cancelled".to_string());
                    return;
                }
                match self.client.restart(&session_id).await {
                    Ok(()) => {
                        // After restart, the new adapter will emit
                        // EditorState on first input — but the user
                        // expects the prompt to be ready right away.
                        // Drop any cached editor state from the dead
                        // adapter so the next render reserves the
                        // editor pane preemptively (the
                        // bootstrap-replay path I landed earlier
                        // will repopulate it from the resumed
                        // adapter's transcript).
                        self.editor_states.remove(&session_id);
                        self.agent_statuses.remove(&session_id);
                        self.browser_previews.remove(&session_id);
                        self.set_status(format!("restarted {}", short_id(&session_id)));
                    }
                    Err(e) => self.set_status(format!("restart failed: {e}")),
                }
            }
            MinibufferIntent::RestartDaemonConfirm => {
                // Reached only if the single-key fast path in
                // `handle_minibuffer_key` fell through (defensive — should
                // not happen in practice).
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("daemon restart cancelled".to_string());
                    return;
                }
                let result = self.client.daemon_restart(None, false).await;
                self.set_status(daemon_restart_status_message(result, "daemon restart"));
            }
            MinibufferIntent::UpgradeConfirm { version } => {
                // Defensive fallback, same as `RestartDaemonConfirm` above.
                let yes = matches!(input.trim().to_lowercase().as_str(), "y" | "yes");
                if !yes {
                    self.set_status("upgrade cancelled".to_string());
                    return;
                }
                self.start_upgrade(version);
            }
            MinibufferIntent::CommandPalette => {
                let cmd = input.trim();
                self.run_palette_command(cmd).await;
            }
            MinibufferIntent::Orchestrator => {
                // Unreachable in PTY-orchestrator mode — the
                // orchestrator panel's keys are handled in
                // handle_orchestrator_key and never reach the regular
                // submit path. Kept as a defensive fallback.
                let _ = input;
            }
            MinibufferIntent::ApproveTool {
                session_id,
                call_id,
                ..
            } => {
                // Reached only if the special-cased key handler in
                // handle_minibuffer_key fell through (defensive — should
                // not happen in practice). Treat any submit as approve.
                if let Err(e) = self
                    .client
                    .tool_decision(&session_id, call_id, "approve")
                    .await
                {
                    self.set_status(format!("tool_decision failed: {e}"));
                } else {
                    self.matrix_rain.observe_tool_decision(
                        "approve",
                        self.matrix_rain_intensity,
                        &session_id,
                    );
                }
            }
        }
    }

    async fn run_palette_command(&mut self, cmd: &str) {
        // Palette text is the same shape as a slash command without
        // the leading `/`; share the dispatch.
        self.run_slash_command(cmd).await;
    }

    /// Open the right minibuffer mode for the user's main "command"
    /// keybind (`M-x` / `C-x x` / click on the prompt). Prefers the
    /// orchestrator panel when an orchestrator session is available;
    /// falls back to the static command palette.
    pub fn open_minibuffer_for_command(&mut self) {
        if self.orchestrator_id.is_some() {
            self.orchestrator_scrollback = 0;
            self.minibuffer = Some(Minibuffer {
                prompt: "> ".to_string(),
                input: String::new(),
                cursor: 0,
                intent: MinibufferIntent::Orchestrator,
                error: None,
            });
        } else {
            self.minibuffer = Some(Minibuffer {
                prompt: "M-x ".to_string(),
                input: String::new(),
                cursor: 0,
                intent: MinibufferIntent::CommandPalette,
                error: None,
            });
        }
    }
}

pub(super) fn insert_minibuffer_text(mb: &mut Minibuffer, text: &str) {
    let pos = byte_pos(&mb.input, mb.cursor);
    mb.input.insert_str(pos, text);
    mb.cursor += text.chars().count();
    mb.error = None;
}

fn delete_back_char(mb: &mut Minibuffer) {
    if mb.cursor > 0 {
        let prev = mb.cursor - 1;
        let pos = byte_pos(&mb.input, prev);
        mb.input.remove(pos);
        mb.cursor = prev;
        mb.error = None;
    }
}

fn delete_forward_char(mb: &mut Minibuffer) {
    if mb.cursor < mb.input.chars().count() {
        let pos = byte_pos(&mb.input, mb.cursor);
        mb.input.remove(pos);
        mb.error = None;
    }
}

fn word_back(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut c = cursor.min(chars.len());
    while c > 0 && !chars[c - 1].is_alphanumeric() {
        c -= 1;
    }
    while c > 0 && chars[c - 1].is_alphanumeric() {
        c -= 1;
    }
    c
}

fn word_forward(s: &str, cursor: usize) -> usize {
    let chars: Vec<char> = s.chars().collect();
    let mut c = cursor.min(chars.len());
    while c < chars.len() && !chars[c].is_alphanumeric() {
        c += 1;
    }
    while c < chars.len() && chars[c].is_alphanumeric() {
        c += 1;
    }
    c
}

fn kill_word_back(mb: &mut Minibuffer) {
    let start = word_back(&mb.input, mb.cursor);
    let start_b = byte_pos(&mb.input, start);
    let end_b = byte_pos(&mb.input, mb.cursor);
    mb.input.drain(start_b..end_b);
    mb.cursor = start;
    mb.error = None;
}

fn kill_word_forward(mb: &mut Minibuffer) {
    let end = word_forward(&mb.input, mb.cursor);
    let start_b = byte_pos(&mb.input, mb.cursor);
    let end_b = byte_pos(&mb.input, end);
    mb.input.drain(start_b..end_b);
    mb.error = None;
}

fn apply_harness_completion(mb: &mut Minibuffer, options: &[String]) {
    let current = mb.input.clone();
    let matches: Vec<&String> = options.iter().filter(|o| o.starts_with(&current)).collect();
    if matches.is_empty() {
        mb.error = if options.is_empty() {
            Some("(no harnesses available)".to_string())
        } else {
            Some(format!("no match for {current}"))
        };
        return;
    }
    if matches.len() == 1 {
        mb.input = matches[0].clone();
        mb.cursor = mb.input.chars().count();
        mb.error = None;
        return;
    }
    let prefix = longest_common_prefix(&matches);
    if prefix.len() > mb.input.len() {
        mb.input = prefix;
        mb.cursor = mb.input.chars().count();
    }
    let listed: Vec<&str> = matches.iter().map(|s| s.as_str()).collect();
    mb.error = Some(format!("matches: {}", listed.join(", ")));
}

fn longest_common_prefix(strs: &[&String]) -> String {
    let mut out = String::new();
    let Some(first) = strs.first() else {
        return out;
    };
    'outer: for (i, c) in first.chars().enumerate() {
        for s in &strs[1..] {
            if s.chars().nth(i) != Some(c) {
                break 'outer;
            }
        }
        out.push(c);
    }
    out
}
