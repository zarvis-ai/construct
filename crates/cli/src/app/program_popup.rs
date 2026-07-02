use anyhow::Result;
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};

use super::*;

impl App {
    /// Flip keyboard focus between the *active* rolled-down Program and the
    /// terminal it exposes. The focus flag and its slide animation live on the
    /// popup itself (see [`ProgramPopup::set_terminal_focus`]), so stashed
    /// popups in unfocused split panes keep their own slide state — focusing
    /// another window never resets a different Program's slide.
    pub(crate) fn set_program_terminal_focus(&mut self, focused: bool) {
        if let Some(popup) = self.program_popup.as_mut() {
            popup.set_terminal_focus(focused);
        }
    }

    pub(super) async fn open_program_popup(&mut self) {
        let Some(session_id) = self.selected_id() else {
            self.set_status("program: no session selected".to_string());
            return;
        };

        match self.client.program_get(&session_id).await {
            Ok(result) => {
                let version = result.program.version;
                let now = Instant::now();
                match result.active_run.and_then(ProgramRun::from_progress) {
                    Some(run) => {
                        self.program_runs
                            .insert(result.program.session_id.clone(), run);
                    }
                    None => {
                        self.program_runs.remove(&result.program.session_id);
                    }
                }
                for cursor in result.collaborators {
                    self.program_collaborators
                        .insert(cursor.client_id.clone(), cursor);
                }
                self.program_popups.remove(&result.program.session_id);
                let mut popup = program_popup_from_document(result.program, result.blocks, now);
                // Restore the caret + scroll the user left when this program was
                // last hidden, so hide→show is position-preserving rather than
                // jumping back to the top.
                self.restore_program_view_state(&mut popup);
                self.program_popup = Some(popup);
                // Opening a program focuses the view pane so its keystrokes are
                // captured immediately for editing. `C-x o` then hands focus
                // back to the list for navigation while the program stays
                // visible (see the focus gate in `on_key`).
                self.focus = PaneFocus::View;
                self.set_program_terminal_focus(false);
                self.set_status(format!("program opened at version {version}"));
                // Live reload: the daemon re-reads the templates dir on every
                // call, but the client caches the list. Kick off a non-blocking
                // refresh on open so edits / new template files surface in the
                // empty-state placeholder without a daemon restart.
                self.refresh_program_templates();
            }
            Err(e) => {
                self.set_status(format!("program get failed: {e}"));
            }
        }
    }

    /// Fetch program templates from the daemon in the background and deliver them
    /// to the event loop via `program_templates_tx`. Non-blocking: the program
    /// opens immediately against the cached list and swaps to the fresh one when
    /// it lands, so there's no flicker and a slow daemon never stalls the open.
    fn refresh_program_templates(&self) {
        let client = self.client.clone();
        let tx = self.program_templates_tx.clone();
        tokio::spawn(async move {
            if let Ok(result) = client.program_templates().await {
                let _ = tx.send(result.templates);
            }
        });
    }

    pub(super) async fn toggle_program_popup(&mut self) {
        if self.program_popup.is_some() {
            self.close_program_popup().await;
        } else {
            self.open_program_popup().await;
        }
    }

    pub(super) async fn restore_open_program_popups(&mut self, session_ids: &[String]) {
        let mut seen = HashSet::new();
        let live_sessions: HashSet<String> = self
            .sessions
            .iter()
            .filter(|s| is_user_list_session(s))
            .map(|s| s.id.clone())
            .collect();
        let selected_id = self.selection.session_id().map(str::to_string);
        let now = Instant::now();
        for session_id in session_ids {
            if !seen.insert(session_id.clone()) || !live_sessions.contains(session_id) {
                continue;
            }
            match self.client.program_get(session_id).await {
                Ok(result) => {
                    match result.active_run.and_then(ProgramRun::from_progress) {
                        Some(run) => {
                            self.program_runs
                                .insert(result.program.session_id.clone(), run);
                        }
                        None => {
                            self.program_runs.remove(&result.program.session_id);
                        }
                    }
                    for cursor in result.collaborators {
                        self.program_collaborators
                            .insert(cursor.client_id.clone(), cursor);
                    }
                    let popup = program_popup_from_document(result.program, result.blocks, now);
                    if selected_id.as_deref() == Some(session_id.as_str()) {
                        self.program_popup = Some(popup);
                    } else {
                        self.program_popups.insert(session_id.clone(), popup);
                    }
                }
                Err(e) => {
                    self.status = Some((
                        format!("program restore failed for {}: {e}", short_id(session_id)),
                        Instant::now(),
                    ));
                }
            }
        }
    }

    pub(crate) fn open_program_session_ids(&self) -> Vec<String> {
        let mut ids = Vec::new();
        let mut seen = HashSet::new();
        if let Some(popup) = self.program_popup.as_ref() {
            if !popup.closing && seen.insert(popup.program.session_id.clone()) {
                ids.push(popup.program.session_id.clone());
            }
        }
        for popup in self.program_popups.values() {
            if !popup.closing && seen.insert(popup.program.session_id.clone()) {
                ids.push(popup.program.session_id.clone());
            }
        }
        ids.sort();
        ids
    }

    /// The session id of the program smart-clip occupying the cell at
    /// `(col, row)`, if any. Reads the hitboxes captured during the last program
    /// render; used for clip click-to-focus and hover-preview.
    pub(super) fn program_clip_session_at(&self, col: u16, row: u16) -> Option<String> {
        self.layout
            .program_clip_hits
            .iter()
            .find(|hit| hit.contains(col, row))
            .map(|hit| hit.session_id.clone())
    }

    pub(crate) fn sync_program_popup_with_selection(&mut self) {
        let selected_id = self.selection.session_id().map(str::to_string);
        let active_id = self
            .program_popup
            .as_ref()
            .map(|popup| popup.program.session_id.clone());
        if active_id.as_deref() == selected_id.as_deref() {
            return;
        }
        self.stash_active_program_popup();
        if let Some(selected_id) = selected_id {
            if let Some(mut popup) = self.program_popups.remove(&selected_id) {
                popup.closing = false;
                self.program_popup = Some(popup);
            }
        }
    }

    fn stash_active_program_popup(&mut self) {
        if let Some(popup) = self.program_popup.take() {
            if !popup.closing {
                self.program_popups
                    .insert(popup.program.session_id.clone(), popup);
            }
        }
    }

    /// Flush a popup's edits to the daemon as a whole-document write.
    ///
    /// If the document advanced underneath us (an agent edited it while the
    /// human was typing), the daemon rejects the stale `base_version`; we then
    /// re-read the latest content and 3-way merge our edits onto it, using the
    /// last-saved content as the common ancestor. Disjoint edits merge silently;
    /// only genuinely overlapping edits produce conflict markers. Either way the
    /// write lands, so hiding the program never blocks and no edit is lost.
    async fn save_program_popup_document(
        &self,
        popup: &ProgramPopup,
    ) -> Result<Option<ProgramSaveOutcome>> {
        let mut ours = program_normalize_smart_clip_instance_ids(&popup.buffer);
        if ours == popup.saved_markdown {
            return Ok(None);
        }
        // The content our edits are based on — the common ancestor for a merge.
        let mut ancestor = popup.saved_markdown.clone();
        let mut base = popup.program.version;
        let mut merged = false;
        let mut conflicted = false;
        // Retry to absorb further updates that land between our re-read and
        // our write.
        for _ in 0..5 {
            let params = agentd_protocol::ProgramUpdateParams {
                session_id: popup.program.session_id.clone(),
                markdown: ours.clone(),
                base_version: Some(base),
                actor: agentd_protocol::ProgramUpdateActor::Human,
                template_id: popup.program.template_id.clone(),
                note: None,
                // Human co-edit save: no shimmer declaration — the daemon
                // narrows the active run by content change only (spec 0053).
                shimmer: None,
                shimmer_tooltips: None,
            };
            match self.client.program_update(params).await {
                Ok(result) => {
                    return Ok(Some(ProgramSaveOutcome {
                        program: result.program,
                        blocks: result.blocks,
                        merged,
                        conflicted,
                    }));
                }
                Err(e) if e.to_string().contains("program conflict") => {
                    let latest = self
                        .client
                        .program_get(&popup.program.session_id)
                        .await?
                        .program;
                    let theirs = latest.markdown;
                    merged = true;
                    match diffy::merge(&ancestor, &ours, &theirs) {
                        Ok(clean) => ours = clean,
                        Err(with_markers) => {
                            ours = with_markers;
                            conflicted = true;
                        }
                    }
                    // The content we just merged onto becomes the ancestor for
                    // any further round.
                    ancestor = theirs;
                    base = latest.version;
                }
                Err(e) => return Err(e),
            }
        }
        Err(anyhow::anyhow!(
            "program merge: gave up after repeated concurrent updates"
        ))
    }

    pub(super) async fn save_open_program_popups(&mut self) {
        let active = self.program_popup.clone();
        if let Some(popup) = active.as_ref() {
            match self.save_program_popup_document(popup).await {
                Ok(Some(outcome)) => {
                    if let Some(active) = self.program_popup.as_mut() {
                        active.buffer = outcome.program.markdown.clone();
                        active.saved_markdown = outcome.program.markdown.clone();
                        active.blocks = outcome.blocks.clone();
                        active.program = outcome.program;
                        active.cursor = active.cursor.min(active.buffer.chars().count());
                        active.preferred_col = None;
                    }
                }
                Ok(None) => {}
                Err(e) => self.status = Some((format!("program save failed: {e}"), Instant::now())),
            }
        }

        let cached: Vec<(String, ProgramPopup)> = self
            .program_popups
            .iter()
            .map(|(id, popup)| (id.clone(), popup.clone()))
            .collect();
        for (session_id, popup) in cached {
            match self.save_program_popup_document(&popup).await {
                Ok(Some(outcome)) => {
                    if let Some(cached) = self.program_popups.get_mut(&session_id) {
                        cached.buffer = outcome.program.markdown.clone();
                        cached.saved_markdown = outcome.program.markdown.clone();
                        cached.blocks = outcome.blocks.clone();
                        cached.program = outcome.program;
                        cached.cursor = cached.cursor.min(cached.buffer.chars().count());
                        cached.preferred_col = None;
                    }
                }
                Ok(None) => {}
                Err(e) => self.status = Some((format!("program save failed: {e}"), Instant::now())),
            }
        }
    }

    pub(super) async fn save_program_popup(&mut self) -> bool {
        let Some(popup) = self.program_popup.as_ref() else {
            return true;
        };
        match self.save_program_popup_document(popup).await {
            Ok(Some(outcome)) => {
                let version = outcome.program.version;
                let (merged, conflicted) = (outcome.merged, outcome.conflicted);
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.buffer = outcome.program.markdown.clone();
                    popup.saved_markdown = outcome.program.markdown.clone();
                    popup.blocks = outcome.blocks.clone();
                    popup.program = outcome.program;
                    popup.cursor = popup.cursor.min(popup.buffer.chars().count());
                    popup.preferred_col = None;
                }
                if conflicted {
                    self.set_status(format!(
                        "program merged with conflicts to resolve (version {version})"
                    ));
                } else if merged {
                    self.set_status(format!(
                        "program merged with agent edits (version {version})"
                    ));
                } else {
                    self.set_status(format!("program saved version {version}"));
                }
                true
            }
            Ok(None) => true,
            Err(e) => {
                self.set_status(format!("program save failed: {e}"));
                false
            }
        }
    }

    pub(super) async fn execute_program_popup(
        &mut self,
        selection: Option<String>,
        selected_block_ids: Option<HashSet<String>>,
    ) -> bool {
        let Some(session_id) = self
            .program_popup
            .as_ref()
            .map(|popup| popup.program.session_id.clone())
        else {
            self.set_status("program run failed: no active program".to_string());
            return false;
        };

        // Snapshot the last daemon-synced content *before* saving, so a re-Run
        // can tell which blocks the user changed (vs. ones the agent settled,
        // which are already folded into `saved_markdown`). See spec 0042.
        let prev_saved = self
            .program_popup
            .as_ref()
            .map(|popup| popup.saved_markdown.clone())
            .unwrap_or_default();

        let dirty = self.program_popup.as_ref().is_some_and(|popup| {
            program_normalize_smart_clip_instance_ids(&popup.buffer) != popup.saved_markdown
        });
        if dirty && !self.save_program_popup().await {
            return false;
        }

        let base_version = self
            .program_popup
            .as_ref()
            .map(|popup| popup.program.version);
        let selection =
            selection.map(|selection| program_normalize_smart_clip_instance_ids(&selection));
        let is_selection = selection.is_some();
        // Optimistic feedback (spec 0042): start the Run shimmer the instant
        // Run is pressed, before the execute round trip, so the affordance
        // covers the agent's latency rather than the request's. The executed
        // body is the selection if present, else the whole (now-saved) buffer.
        let run_body = match selection.as_deref() {
            Some(sel) => sel.to_string(),
            None => self
                .program_popup
                .as_ref()
                .map(|popup| popup.buffer.clone())
                .unwrap_or_default(),
        };
        let selected_block_ids = selected_block_ids.or_else(|| {
            self.program_popup
                .as_ref()
                .and_then(Self::selected_program_block_ids)
        });
        match selected_block_ids {
            Some(pending) if is_selection => {
                let pending = self.program_run_pending_with_existing(&session_id, pending);
                self.start_program_run_with_pending(&session_id, pending)
            }
            _ => self.start_program_run(&session_id, &run_body, is_selection, &prev_saved),
        }
        let params = agentd_protocol::ProgramExecuteParams {
            session_id: session_id.clone(),
            selection,
            base_version,
            // Optimistic full-region shimmer; the run's planning pass narrows it.
            shimmer: None,
        };
        match self.client.program_execute(params).await {
            Ok(result) => {
                match result.active_run.and_then(ProgramRun::from_progress) {
                    Some(run) => {
                        self.program_runs.insert(session_id.clone(), run);
                    }
                    None => {
                        self.program_runs.remove(&session_id);
                    }
                }
                let scope = if is_selection { "selection" } else { "program" };
                self.set_status(format!(
                    "program run sent ({scope}, version {})",
                    result.program.version
                ));
                true
            }
            Err(e) => {
                // The request never landed — retract the optimistic shimmer.
                self.program_runs.remove(&session_id);
                self.set_status(format!("program run failed: {e}"));
                false
            }
        }
    }

    /// Begin (or refresh) the Run shimmer for `session_id` over `body` — the
    /// executed Markdown (full program or selection). Records the block
    /// signatures to shimmer; they settle as their content changes and the run
    /// clears once the first agent output is observed. See spec 0042.
    ///
    /// A re-Run while a run is still active preserves the narrowing the agent
    /// established: it re-shimmers only the blocks the user changed since the
    /// last daemon sync (`prev_saved`) plus blocks that were still pending —
    /// blocks the agent already settled stay calm. A selection run adds its
    /// own scope to any shimmer already in flight rather than replacing it —
    /// running one snippet must not dim blocks another in-flight run is still
    /// working on; a fresh run with nothing in flight shimmers just the
    /// selected region.
    pub(super) fn start_program_run(
        &mut self,
        session_id: &str,
        body: &str,
        is_selection: bool,
        prev_saved: &str,
    ) {
        let body_ids = program_run_pending_ids(body);
        if body_ids.is_empty() {
            // Empty body has nothing to shimmer; drop any stale run.
            self.program_runs.remove(session_id);
            return;
        }
        let pending: HashSet<String> = match self.program_runs.get(session_id) {
            // Re-Run mid-flight: union of the user's fresh edits (blocks present
            // now but not in the last synced content) and blocks that never
            // settled under the prior run. Agent-settled blocks are in
            // `prev_saved` and absent from `old.pending`, so both terms skip
            // them and they stop re-shimmering.
            Some(old) if !is_selection => {
                let prev_ids = program_run_pending_ids(prev_saved);
                let narrowed: HashSet<String> = body_ids
                    .difference(&prev_ids)
                    .chain(body_ids.intersection(&old.pending))
                    .cloned()
                    .collect();
                // A run can already exist over this session with nothing left
                // that overlaps the fresh press — e.g. the prior run was
                // scoped to a selection, or its pending set had transiently
                // emptied mid-turn (spec 0042) without being cleared yet.
                // Pressing Run again must still give immediate feedback for
                // this new request rather than silently going dark, so an
                // empty narrowing falls back to the whole executed body —
                // mirroring the daemon's own mid-flight fallback.
                if narrowed.is_empty() {
                    body_ids
                } else {
                    narrowed
                }
            }
            // A selection run keeps any shimmer already in flight and adds
            // its own scope on top of it (see `program_run_pending_with_existing`).
            Some(old) => old.pending.union(&body_ids).cloned().collect(),
            // Fresh run, nothing in flight: shimmer just the executed body.
            None => body_ids,
        };
        self.start_program_run_with_pending(session_id, pending);
    }

    /// Union `ids` with the pending set of any Run already in flight for
    /// `session_id`. Used by selection runs so that optimistically shimmering
    /// the freshly-run block never clears shimmer another in-flight run
    /// already declared elsewhere in the program (see spec 0042).
    fn program_run_pending_with_existing(
        &self,
        session_id: &str,
        ids: HashSet<String>,
    ) -> HashSet<String> {
        match self.program_runs.get(session_id) {
            Some(old) => old.pending.union(&ids).cloned().collect(),
            None => ids,
        }
    }

    fn start_program_run_with_pending(&mut self, session_id: &str, pending: HashSet<String>) {
        if pending.is_empty() {
            self.program_runs.remove(session_id);
            return;
        }
        let now = Instant::now();
        self.program_runs.insert(
            session_id.to_string(),
            ProgramRun {
                started_at: now,
                total_block_count: pending.len(),
                pending,
                pending_tooltips: HashMap::new(),
                system_status: None,
                deadline: now + Duration::from_millis(PROGRAM_RUN_MAX_MS),
                first_output_seen: false,
                stage: agentd_protocol::ProgramRunStage::Pressed,
                settled_block_count: 0,
            },
        );
    }

    /// Reap Run shimmers that have outlived their backstop deadline, so a
    /// missed first-output signal can never strand the animation (spec 0042).
    pub(super) fn expire_program_runs(&mut self, now: Instant) {
        self.program_runs.retain(|_, run| now < run.deadline);
        self.expire_program_settle_flourishes(now);
    }

    pub(super) fn record_program_settle_flourishes(
        &mut self,
        session_id: &str,
        previous_pending: &HashSet<String>,
        next_pending: &HashSet<String>,
        now: Instant,
    ) {
        let settled: Vec<String> = previous_pending.difference(next_pending).cloned().collect();
        if settled.is_empty() {
            return;
        }
        let flourishes = self
            .program_settle_flourishes
            .entry(session_id.to_string())
            .or_default();
        for block_ref in settled {
            flourishes.insert(block_ref, now);
        }
    }

    fn expire_program_settle_flourishes(&mut self, now: Instant) {
        let ttl = Duration::from_millis(crate::app::PROGRAM_SETTLE_FLASH_MS);
        self.program_settle_flourishes.retain(|_, flourishes| {
            flourishes.retain(|_, started_at| now.saturating_duration_since(*started_at) < ttl);
            !flourishes.is_empty()
        });
    }

    pub(super) async fn close_program_popup(&mut self) {
        if !self.save_program_popup().await {
            return;
        }
        if let Some(session_id) = self
            .program_popup
            .as_ref()
            .map(|popup| popup.program.session_id.clone())
        {
            let _ = self
                .client
                .program_cursor(agentd_protocol::ProgramCursorParams {
                    session_id,
                    cursor: 0,
                    selection_anchor: None,
                    selection_head: None,
                    version: None,
                    label: Some("TUI".to_string()),
                    clear: true,
                })
                .await;
        }
        // Capture caret + scroll before the popup fades so reopening this
        // session's program restores them (the popup itself is dropped once the
        // close animation lapses — see `render_program_popup`).
        self.remember_program_view_state();
        self.set_program_terminal_focus(false);
        if let Some(popup) = self.program_popup.as_mut() {
            self.program_popups.remove(&popup.program.session_id);
            let now = Instant::now();
            popup.closing = true;
            popup.hide_after = now + Duration::from_millis(PROGRAM_REVEAL_MS);
        }
    }

    /// Snapshot the active program's caret + scroll into `program_view_memory`
    /// so a later reopen of the same session's program restores them. Every
    /// path that hides the program calls this before the popup is dropped.
    pub(super) fn remember_program_view_state(&mut self) {
        if let Some(popup) = self.program_popup.as_ref() {
            self.program_view_memory.insert(
                popup.program.session_id.clone(),
                ProgramViewMemory {
                    cursor: popup.cursor,
                    preferred_col: popup.preferred_col,
                    scroll_offset: popup.scroll_offset,
                    cover_percent: popup.cover_percent,
                },
            );
        }
    }

    /// Reapply a remembered caret + scroll (captured when the program was last
    /// hidden) onto a freshly-loaded popup, so a hide→show cycle lands on the
    /// same position. Consumes the entry. The cursor is clamped to the buffer
    /// (the document may have changed on the daemon); an out-of-range scroll is
    /// clamped by the renderer.
    pub(super) fn restore_program_view_state(&mut self, popup: &mut ProgramPopup) {
        if let Some(memory) = self.program_view_memory.remove(&popup.program.session_id) {
            popup.cursor = memory.cursor.min(popup.buffer.chars().count());
            popup.preferred_col = memory.preferred_col;
            popup.scroll_offset = memory.scroll_offset;
            popup.cover_percent = memory.cover_percent.clamp(
                crate::app::PROGRAM_COVER_PERCENT_MIN,
                crate::app::PROGRAM_COVER_PERCENT_MAX,
            );
        }
    }
}
