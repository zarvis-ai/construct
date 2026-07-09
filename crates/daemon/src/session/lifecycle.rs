use super::*;

impl SessionManager {
    pub async fn create(self: &Arc<Self>, params: CreateSessionParams) -> Result<String> {
        let harness = params.harness.as_str();
        let adapter_cfg = self
            .config
            .adapters
            .get(harness)
            .ok_or_else(|| anyhow!("unknown harness: {}", params.harness))?
            .clone();
        let binary_spec = adapter_cfg
            .binary
            .clone()
            .unwrap_or_else(|| harness.to_string());
        let binary = locate_binary(&binary_spec)
            .ok_or_else(|| anyhow!("adapter binary not found: {}", binary_spec))?;

        let id = format!("s{}", uuid::Uuid::new_v4().simple());
        let now = Utc::now();

        // Worktree setup (best effort).
        let want_worktree = params.worktree || self.config.defaults.worktree.unwrap_or(false);
        let cwd_path = PathBuf::from(&params.cwd);
        let worktree_path = if want_worktree && worktree::is_git_repo(&cwd_path).await {
            let dest = self.storage.worktree_path(&id);
            let branch = format!("agentd/{}", id);
            match worktree::create_worktree(&cwd_path, &dest, &branch).await {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(%id, error = %e, "worktree creation failed; using original cwd");
                    None
                }
            }
        } else {
            None
        };
        let effective_cwd = worktree_path.clone().unwrap_or_else(|| cwd_path.clone());
        let mut position = -now.timestamp_millis();
        if let Some(after_id) = params.position_after_session_id.as_deref() {
            let all_sessions = self.list().await;
            if let Some(placement) =
                position_after_visible_session(after_id, &params.group_id, &all_sessions)
            {
                for (session_id, next_position) in placement.updates {
                    if let Some(entry) = self.get_entry(&session_id).await {
                        let snapshot = {
                            let mut s = entry.summary.write().await;
                            s.position = next_position;
                            s.clone()
                        };
                        self.storage.save_summary(&snapshot)?;
                        let _ = self.broadcast.send(BroadcastMsg::State(StateNotificationPayload {
                            session: snapshot,
                        }));
                    }
                }
                position = placement.position;
            }
        }

        let mut summary = SessionSummary {
            id: id.clone(),
            harness: harness.to_string(),
            cwd: effective_cwd.to_string_lossy().to_string(),
            title: params.title.clone(),
            state: SessionState::Pending,
            created_at: now,
            last_event_at: None,
            cost_usd: None,
            model: params.model.clone(),
            worktree: worktree_path
                .as_ref()
                .map(|p| p.to_string_lossy().to_string()),
            pending_input: false,
            last_prompt: params.prompt.clone(),
            event_count: 0,
            has_pty: false,
            mode: Some(effective_mode(&params)),
            pinned: false,
            position,
            group_id: params.group_id.clone(),
            parent_session_id: params
                .parent_session_id
                .clone()
                .or_else(|| params.env.get("CONSTRUCT_PARENT_SESSION_ID").cloned()),
            last_pty_at_ms: None,
            approval_mode: agentd_protocol::ApprovalMode::Manual,
            kind: params.kind,
            archived: false,
            operator_loop_disabled: params.kind == agentd_protocol::SessionKind::Orchestrator,
            needs_attention: false,
        };
        self.storage.save_summary(&summary)?;

        let (msg_tx, msg_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);
        let combined_args = {
            let mut a = adapter_cfg.args.clone();
            a.extend(params.args.clone());
            a
        };

        // Build the full env (adapter-config + user-provided + daemon
        // meta) BEFORE spawn so the adapter process inherits
        // CONSTRUCT_SESSION_DATA_DIR / CONSTRUCT_SESSION_KIND — not just
        // the session.start params.env. The codex adapter (and
        // claude) reads these via std::env::var, so leaving them only in
        // session.start meant their first-spawn bookkeeping
        // (originator-tagged rollout capture, session-id minting)
        // silently no-op'd; respawn already merged them in time, so
        // the bug only surfaced on initial create.
        //
        // Precedence: `[adapters.<name>].env` is the per-harness
        // baseline (operator-set default model, etc.), overridden
        // by the per-session `params.env` (explicit `construct new
        // --env KEY=VAL`), overridden in turn by daemon-meta. So a
        // CLI flag always wins over config.toml, and daemon meta
        // always wins over both.
        let mut env_with_meta = adapter_cfg.env.clone();
        for (k, v) in &params.env {
            env_with_meta.insert(k.clone(), v.clone());
        }
        let session_dir = self.storage.session_dir(&id);
        let widgets_dir = self.storage.ensure_widgets_dir(&id).unwrap_or_else(|e| {
            tracing::warn!(session = %id, error = ?e, "ensure widgets dir failed");
            self.storage.widgets_dir(&id)
        });
        env_with_meta.insert(
            "CONSTRUCT_SESSION_DATA_DIR".to_string(),
            session_dir.to_string_lossy().to_string(),
        );
        env_with_meta.insert(
            "CONSTRUCT_SESSION_WIDGETS_DIR".to_string(),
            widgets_dir.to_string_lossy().to_string(),
        );
        // Single auto-approval policy the daemon defines once; each adapter
        // translates it into its harness's native permission mechanism. See
        // `agentd_protocol::adapter::policy`.
        env_with_meta.insert(
            agentd_protocol::adapter::policy::ENV_AUTO_APPROVE_PATHS.to_string(),
            widgets_dir.to_string_lossy().to_string(),
        );
        self.install_program_run_context_env(&mut env_with_meta, &id);
        env_with_meta.insert(
            "CONSTRUCT_SESSION_KIND".to_string(),
            match params.kind {
                agentd_protocol::SessionKind::User => "user",
                agentd_protocol::SessionKind::Orchestrator => "orchestrator",
                agentd_protocol::SessionKind::Subagent => "subagent",
            }
            .to_string(),
        );
        self.install_memory_env(&mut env_with_meta, params.group_id.as_deref());

        let (adapter, info) = Adapter::spawn_reconnectable(
            harness.to_string(),
            binary,
            combined_args,
            env_with_meta.clone(),
            self.adapter_socket_path(&id),
            msg_tx.clone(),
        )
        .await
        .with_context(|| format!("spawn adapter for {}", harness))?;

        // Apply capability-derived info.
        if summary.model.is_none() {
            summary.model = info.capabilities.models.first().cloned();
        }
        summary.has_pty = info.capabilities.supports_pty;
        self.storage.save_summary(&summary)?;
        let start_params = start_params_for_create(
            id.clone(),
            summary.cwd.clone(),
            params.prompt.clone(),
            summary.model.clone(),
            params.mode.clone(),
            params.pty_size,
            env_with_meta,
            params.args.clone(),
        );
        // Persist so a daemon restart can re-spawn with the same shape.
        let _ = self.storage.save_start_params(&id, &start_params);
        // Reflect Pending → Running on start (the adapter may also emit a status).
        summary.state = SessionState::Running;
        self.storage.save_summary(&summary)?;

        let entry = Arc::new(SessionEntry {
            id: id.clone(),
            summary: RwLock::new(summary.clone()),
            transcript_count: AtomicU64::new(0),
            adapter: tokio::sync::Mutex::new(Some(adapter.clone())),
            pty: tokio::sync::Mutex::new(PtyState {
                size: params.pty_size,
            }),
            deleted: AtomicBool::new(false),
            archived: AtomicBool::new(summary.archived),
            title_gen_attempted: AtomicBool::new(summary.title.is_some()),
            pending_title_prompts: std::sync::Mutex::new(Vec::new()),
            pty_input_capture: tokio::sync::Mutex::new(PtyInputCapture::default()),
            tasks: tokio::sync::Mutex::new(TaskRegistry::default()),
            pty_client_policy: std::sync::Mutex::new(PtyClientPolicy::default()),
            unseen_activity: AtomicBool::new(false),
            pty_burst_start_ms: AtomicI64::new(0),
            osc11_tail: std::sync::Mutex::new(Vec::new()),
        });

        // Record the user's initial prompt as the first transcript event so
        // the transcript reads coherently (user → assistant) for every adapter.
        // Auto-title is triggered inside handle_event for any User message.
        if let Some(p) = params.prompt.as_ref().filter(|s| !s.trim().is_empty()) {
            self.handle_event(
                &entry,
                SessionEvent::Message {
                    role: MessageRole::User,
                    text: p.clone(),
                },
            )
            .await;
        }

        adapter
            .request(ahp_method::SESSION_START, serde_json::to_value(&start_params)?)
            .await
            .context("adapter session.start failed")?;

        self.sessions
            .write()
            .await
            .insert(id.clone(), entry.clone());

        // Spawn drain task for adapter messages.
        let manager = self.clone();
        let entry_for_drain = entry.clone();
        tokio::spawn(async move {
            manager.drain_adapter(entry_for_drain, msg_rx).await;
        });

        // Broadcast initial state.
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: summary,
            }));

        Ok(id)
    }

    /// Ensure the daemon-owned orchestrator session exists. Called once
    /// at startup, after `resume_running_sessions`. Three outcomes:
    ///
    /// 1. Orchestrator disabled in config → no-op.
    /// 2. Orchestrator session already exists (created on a previous
    ///    run and rehydrated) → no-op; `resume_running_sessions`
    ///    already brought it back online.
    /// 3. No orchestrator session yet → create one with the configured
    ///    harness. Failures (binary missing, capability negotiation,
    ///    initial prompt rejected) are logged and the daemon proceeds
    ///    without an orchestrator — clients see palette mode.
    pub async fn ensure_orchestrator(self: Arc<Self>) {
        let harness = match self.config.orchestrator.effective_harness() {
            Some(h) => h.to_string(),
            None => {
                tracing::info!("orchestrator disabled in config");
                return;
            }
        };
        // Already have a *live* one? Persisted summaries with
        // `kind: Orchestrator` in any non-terminal state are reused.
        // Terminal orchestrators (Errored / Done — usually from a
        // previous run when no API key was set) are left in place
        // for forensics but a fresh one is created so the user gets
        // a working panel.
        {
            let guard = self.sessions.read().await;
            for entry in guard.values() {
                let s = entry.summary.read().await;
                if s.kind == agentd_protocol::SessionKind::Orchestrator && !s.state.is_terminal() {
                    tracing::info!(
                        id = %s.id,
                        harness = %s.harness,
                        state = ?s.state,
                        "orchestrator session already exists"
                    );
                    return;
                }
            }
        }
        // Create fresh. Use the daemon process cwd so the orchestrator's
        // shell tools resolve relative paths from wherever the user
        // started agentd. Interactive mode gives the orchestrator a
        // PTY-backed REPL — the TUI renders it in the minibuffer panel
        // and gets the line-editor / queue / slash popup polish from
        // smith interactive for free. The initial 80×10 pty_size is
        // a placeholder; the TUI sends a pty_resize as soon as it
        // attaches the panel.
        let cwd = std::env::current_dir()
            .map(|p| p.to_string_lossy().to_string())
            .unwrap_or_else(|_| "/".to_string());
        let params = agentd_protocol::CreateSessionParams {
            harness: harness.clone(),
            cwd,
            prompt: None,
            model: None,
            title: Some("orchestrator".to_string()),
            mode: Some("interactive".to_string()),
            pty_size: Some(agentd_protocol::PtySize { cols: 80, rows: 10 }),
            worktree: false,
            env: Default::default(),
            args: Vec::new(),
            kind: agentd_protocol::SessionKind::Orchestrator,
            parent_session_id: None,
            group_id: None,
            position_after_session_id: None,
        };
        match self.create(params).await {
            Ok(id) => tracing::info!(
                id = %id,
                harness = %harness,
                "orchestrator session created"
            ),
            Err(e) => tracing::warn!(
                harness = %harness,
                error = %e,
                "orchestrator session create failed; clients fall back to palette mode"
            ),
        }
    }

    /// Re-spawn the adapter for every persisted session whose state is
    /// resumable at daemon startup. `Done` sessions stay closed; `Errored`
    /// sessions are retried because an error can mean the previous adapter or
    /// machine died rather than the underlying agent conversation ending.
    /// Each adapter
    /// receives `CONSTRUCT_RESUME=1` in its env plus the same start params it
    /// was originally launched with (cwd, model, prompt, etc.) — the
    /// adapter decides what "resume" means for its harness. Sessions that
    /// can't be re-spawned (missing start.json, missing adapter binary,
    /// spawn failure) are marked Errored.
    pub async fn resume_running_sessions(self: Arc<Self>) {
        let ids: Vec<String> = {
            let guard = self.sessions.read().await;
            let mut v = Vec::new();
            for (id, entry) in guard.iter() {
                let s = entry.summary.read().await;
                // Archived sessions stay down across daemon restarts — the
                // user terminated them on purpose and brings them back with an
                // explicit restart, not auto-resume.
                if should_resume_on_startup(s.state) && !s.archived {
                    v.push(id.clone());
                }
            }
            v
        };
        for id in ids {
            if let Err(e) = self.clone().respawn(&id).await {
                tracing::warn!(session = %id, error = ?e, "resume failed; marking Errored");
                if let Some(entry) = self.get_entry(&id).await {
                    let snapshot = {
                        let mut s = entry.summary.write().await;
                        s.state = SessionState::Errored;
                        s.clone()
                    };
                    let _ = self.storage.save_summary(&snapshot);
                    let _ = self
                        .broadcast
                        .send(BroadcastMsg::State(StateNotificationPayload {
                            session: snapshot,
                        }));
                }
            }
        }
    }

    /// Spawn an adapter for an already-existing session entry (i.e. on
    /// daemon restart). Reuses the start params persisted at create time
    /// and signals `CONSTRUCT_RESUME=1` so the adapter can pull its own
    /// prior state from `CONSTRUCT_SESSION_DATA_DIR`.
    async fn respawn(self: Arc<Self>, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {id}"))?;
        let mut start_params = self.storage.load_start_params(id)?;
        start_params
            .env
            .insert("CONSTRUCT_RESUME".to_string(), "1".to_string());
        // Make sure the data-dir env is present even if start.json predates
        // the meta env injection.
        start_params.env.insert(
            "CONSTRUCT_SESSION_DATA_DIR".to_string(),
            self.storage.session_dir(id).to_string_lossy().to_string(),
        );
        let (project_id, current_model, operator_loop_disabled) = {
            let s = entry.summary.read().await;
            (
                s.group_id.clone(),
                s.model.clone(),
                s.operator_loop_disabled,
            )
        };
        // The summary's model is the live source of truth — it tracks any
        // mid-session `/model` switch (via `ModelChanged`), whereas the start
        // params were frozen at create. Re-inject it so the resumed adapter
        // comes back on the model the session was last running.
        if current_model.is_some() {
            start_params.model = current_model;
        }
        // Re-inject the operator loop toggle so the resumed adapter starts
        // with the same enabled/disabled state the user left it in.
        if operator_loop_disabled {
            start_params
                .env
                .insert("CONSTRUCT_OPERATOR_LOOP_DISABLED".to_string(), "1".to_string());
        } else {
            start_params.env.remove("CONSTRUCT_OPERATOR_LOOP_DISABLED");
        }
        self.install_memory_env(&mut start_params.env, project_id.as_deref());
        let widgets_dir = self.storage.ensure_widgets_dir(id).unwrap_or_else(|e| {
            tracing::warn!(session = %id, error = ?e, "ensure widgets dir failed");
            self.storage.widgets_dir(id)
        });
        start_params.env.insert(
            "CONSTRUCT_SESSION_WIDGETS_DIR".to_string(),
            widgets_dir.to_string_lossy().to_string(),
        );
        start_params
            .env
            .insert(agentd_protocol::adapter::policy::ENV_AUTO_APPROVE_PATHS.to_string(), widgets_dir.to_string_lossy().to_string());
        self.install_program_run_context_env(&mut start_params.env, id);
        // Use the last-known PTY size so the resumed adapter (which
        // sizes its PTY off start_params on session.start) doesn't draw
        // its banner / resume content at the stale creation default.
        if let Some(size) = self.storage.load_pty_size(id) {
            start_params.pty_size = Some(size);
        }

        let harness = {
            let s = entry.summary.read().await;
            s.harness.clone()
        };
        let (msg_tx, msg_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);

        match Adapter::attach(
            harness.clone(),
            self.adapter_socket_path(id),
            msg_tx.clone(),
        )
        .await
        {
            Ok((adapter, _info)) => {
                *entry.adapter.lock().await = Some(adapter);
                let snapshot = {
                    let mut s = entry.summary.write().await;
                    s.state = SessionState::Running;
                    s.pending_input = false;
                    // Restarting an archived session brings it back to life:
                    // it returns to the active list. Clear the archive-intent
                    // flag too so a future Closed event doesn't re-archive it.
                    s.archived = false;
                    entry.archived.store(false, Ordering::SeqCst);
                    s.clone()
                };
                let _ = self.storage.save_summary(&snapshot);
                let _ = self
                    .broadcast
                    .send(BroadcastMsg::State(StateNotificationPayload {
                        session: snapshot,
                    }));
                let manager = self.clone();
                let entry_for_drain = entry.clone();
                tokio::spawn(async move {
                    manager.drain_adapter(entry_for_drain, msg_rx).await;
                });
                tracing::info!(session = %id, %harness, "reattached adapter");
                return Ok(());
            }
            Err(e) => {
                tracing::debug!(
                    session = %id,
                    %harness,
                    error = ?e,
                    "adapter attach failed; respawning"
                );
            }
        }

        // Attach failed. The probe's reader task in `Adapter::attach`
        // still holds a clone of `msg_tx` and, when its hung-connection
        // read finally errors out, will push a spurious
        // `AdapterMessage::Closed` into the channel. If we kept the
        // same `msg_rx` for the post-spawn `drain_adapter`, that
        // `Closed` would arrive seconds after the freshly-spawned
        // adapter is up and immediately mark the resumed session
        // `Done` — defeating the whole resume. Replace the channel
        // here so the leaked sender's `send` lands in a dropped
        // receiver (and silently fails) instead.
        drop(msg_tx);
        drop(msg_rx);
        let (msg_tx, msg_rx) = mpsc::channel::<AdapterMessage>(ADAPTER_DRAIN_CAP);

        let adapter_cfg = self
            .config
            .adapters
            .get(&harness)
            .ok_or_else(|| anyhow!("unknown harness on resume: {harness}"))?
            .clone();
        let binary_spec = adapter_cfg
            .binary
            .clone()
            .unwrap_or_else(|| harness.clone());
        let binary = locate_binary(&binary_spec)
            .ok_or_else(|| anyhow!("adapter binary not found: {binary_spec}"))?;
        let combined_args = {
            let mut a = adapter_cfg.args.clone();
            a.extend(start_params.args.clone());
            a
        };

        // Merge `[adapters.<name>].env` underneath the persisted
        // start-params env so config.toml-driven defaults apply on
        // respawn too. Per-session env (from `construct new --env`)
        // still wins because start_params.env was constructed with
        // it on top of adapter_cfg.env at create time and gets the
        // same treatment again here.
        let respawn_env = {
            let mut e = adapter_cfg.env.clone();
            for (k, v) in &start_params.env {
                e.insert(k.clone(), v.clone());
            }
            self.install_memory_env(&mut e, project_id.as_deref());
            e
        };

        let (adapter, info) = Adapter::spawn_reconnectable(
            harness.clone(),
            binary,
            combined_args,
            respawn_env,
            self.adapter_socket_path(id),
            msg_tx.clone(),
        )
        .await
        .with_context(|| format!("respawn adapter for {harness}"))?;

        // Drop stale PTY bytes from the previous incarnation BEFORE the
        // new child can start emitting. Without this, the in-memory ring
        // (rehydrated from pty.log at Manager::new) and the on-disk
        // pty.log both hold the old child's TUI state — when a TUI
        // client reconnects and calls pty_replay it gets that history
        // merged with the new child's startup escapes, and vt100 lands
        // in a weird half-rendered state (often appearing blank with
        // just a cursor) until a SIGWINCH forces a redraw.
        //
        // Adapters that advertise `supports_silent_resume` promise to
        // emit nothing on resume (smith does this), so we keep the
        // prior PTY history visible after a daemon restart instead of
        // wiping it.
        if !info.capabilities.supports_silent_resume {
            if let Err(e) = self.storage.truncate_pty_log(id) {
                tracing::warn!(session = %id, error = ?e, "truncate_pty_log on respawn failed");
            }
        }

        adapter
            .request(
                ahp_method::SESSION_START,
                serde_json::to_value(&start_params)?,
            )
            .await
            .context("adapter session.start (resume) failed")?;

        *entry.adapter.lock().await = Some(adapter.clone());

        // Notify clients that this session is alive again. A resumed
        // `Errored` session must stop looking terminal immediately so startup
        // code (notably orchestrator creation) does not treat it as dead while
        // waiting for the adapter's first Status event.
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.state = SessionState::Running;
            s.pending_input = false;
            // Restarting an archived session brings it back to life: it
            // returns to the active list. Clear the archive-intent flag too so
            // a future Closed event doesn't re-archive it.
            s.archived = false;
            entry.archived.store(false, Ordering::SeqCst);
            s.clone()
        };
        let _ = self.storage.save_summary(&snapshot);
        let _ = self.broadcast.send(BroadcastMsg::State(StateNotificationPayload {
            session: snapshot,
        }));

        // Drain adapter messages just like a fresh create.
        let manager = self.clone();
        let entry_for_drain = entry.clone();
        tokio::spawn(async move {
            manager.drain_adapter(entry_for_drain, msg_rx).await;
        });

        // Force-redraw cycle for PTY-backed adapters that don't
        // silently resume. Codex / claude / shell only repaint past
        // content when their PTY's SIGWINCH fires, and the child was
        // just spawned at the cached pty_size — so any pty_resize a
        // TUI sends with the same dimensions is a kernel no-op
        // (ioctl(TIOCSWINSZ) only signals on actual size change),
        // leaving the pane stuck on whatever the child happened to
        // paint at startup (often just a banner / cursor) until the
        // user manually resizes their terminal.
        //
        // We schedule a "bump by 1 col → restore" sequence on a
        // background task. The 250 ms delay gives the child time to
        // settle into its initial draw; the two ioctls then force
        // two SIGWINCH'es, the second of which leaves the PTY at the
        // correct cached size. smith (silent_resume) is skipped —
        // it explicitly emits nothing on resume.
        if let Some(size) = force_redraw_size_on_resume(&info.capabilities, start_params.pty_size) {
            let manager_for_redraw = self.clone();
            let id_owned = id.to_string();
            let entry_for_redraw = entry.clone();
            tokio::spawn(async move {
                // Wait for the resumed child's PTY output to settle (it
                // produced its resume draw and went quiet) before forcing
                // the redraw, so the SIGWINCH lands after the child has
                // loaded its conversation rather than on a half-drawn
                // banner. Falls back to a hard cap if it never settles.
                let started = tokio::time::Instant::now();
                loop {
                    tokio::time::sleep(RESPAWN_REDRAW_POLL).await;
                    let last = entry_for_redraw.summary.read().await.last_pty_at_ms;
                    if resume_redraw_ready(last, Utc::now().timestamp_millis(), started.elapsed()) {
                        break;
                    }
                }
                let bumped_cols = size.cols.saturating_add(1);
                let _ = manager_for_redraw
                    .pty_resize(&id_owned, bumped_cols, size.rows)
                    .await;
                let _ = manager_for_redraw.pty_resize(&id_owned, size.cols, size.rows).await;
            });
        }

        tracing::info!(session = %id, %harness, "resumed");
        Ok(())
    }

    /// Public entry point for "bring a terminated session back to
    /// life". Used by the TUI's restart-confirm flow: the user
    /// pressed `y` on a `Done`/`Errored` session and wants to keep
    /// typing. Refuses sessions that already have a live adapter
    /// — those are running, not done.
    ///
    /// Internally just calls [`Manager::respawn`], which sets
    /// `CONSTRUCT_RESUME=1` in the adapter env so harnesses that
    /// persist conversation state (smith) reload it on the new
    /// process.
    pub async fn restart(self: Arc<Self>, id: &str) -> Result<()> {
        let entry = self
            .get_entry(id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", id))?;
        if entry.adapter.lock().await.is_some() {
            return Err(anyhow!(
                "session already has a live adapter (state is not terminal)"
            ));
        }
        self.respawn(id).await
    }
}

// Build the [`SessionStartParams`] an adapter receives at `session.start`
// from a freshly handled create request.
//
// The initial `prompt` is forwarded verbatim here, and this `session.start`
// payload is the *only* channel by which a newly created session's seed
// prompt reaches its harness. Every adapter starts its first turn from
// `params.prompt`: headless adapters push it onto their run queue and run it
// immediately, while interactive PTY harnesses receive it as a native launch
// argument or a queued submit. There is no separate daemon-side PTY write of
// the seed prompt — so unlike the program `Run` path (see
// `program_pty_submit_bytes`), there is no CR/LF terminator to get right here.
// If this forward is dropped, a created session sits idle in `AwaitingInput`
// with no way to start its turn except a manual follow-up `send_input`. See
// `specs/0046-session-create-initial-prompt-submits.md`.
#[allow(clippy::too_many_arguments)]
pub(super) fn start_params_for_create(
    session_id: String,
    cwd: String,
    prompt: Option<String>,
    model: Option<String>,
    mode: Option<String>,
    pty_size: Option<PtySize>,
    env: HashMap<String, String>,
    args: Vec<String>,
) -> SessionStartParams {
    SessionStartParams {
        session_id,
        cwd,
        prompt,
        model,
        mode,
        pty_size,
        env,
        args,
    }
}

// Whether the post-resume force-redraw should fire now: the child has
// produced PTY output and then gone quiet for [`RESPAWN_REDRAW_SETTLE`],
// or [`RESPAWN_REDRAW_MAX_WAIT`] has elapsed (so a child that streams
// forever, or never draws, still gets a redraw). `last_pty_at_ms` is the
// child's most recent PTY-output timestamp (`None` = nothing yet).
pub(super) fn resume_redraw_ready(last_pty_at_ms: Option<i64>, now_ms: i64, elapsed: Duration) -> bool {
    if elapsed >= RESPAWN_REDRAW_MAX_WAIT {
        return true;
    }
    match last_pty_at_ms {
        Some(t) => now_ms.saturating_sub(t) >= RESPAWN_REDRAW_SETTLE.as_millis() as i64,
        None => false,
    }
}

pub(super) fn should_resume_on_startup(state: SessionState) -> bool {
    !matches!(state, SessionState::Done)
}

// Decide whether to schedule the bump+restore SIGWINCH cycle after a
// session.start succeeds on respawn. Returns the size to restore to
// (we always restore to the cached size, then bump by one column for
// the first leg of the cycle). Returns `None` when no force-redraw
// is warranted:
//   * the adapter advertises `supports_silent_resume` (smith paints
//     nothing on resume — any forced SIGWINCH would corrupt its
//     custom render);
//   * no cached pty_size to restore to (fresh creates skip this);
//   * the adapter doesn't expose a PTY at all.
pub(super) fn force_redraw_size_on_resume(
    caps: &agentd_protocol::Capabilities,
    cached: Option<agentd_protocol::PtySize>,
) -> Option<agentd_protocol::PtySize> {
    if caps.supports_silent_resume {
        return None;
    }
    if !caps.supports_pty {
        return None;
    }
    let size = cached?;
    if size.cols == 0 || size.rows == 0 {
        return None;
    }
    Some(size)
}
