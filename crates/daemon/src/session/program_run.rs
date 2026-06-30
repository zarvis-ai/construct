use super::*;

fn block_ref_or_id(block: &agentd_protocol::ProgramBlockView) -> String {
    if !block.block_ref.is_empty() {
        block.block_ref.clone()
    } else {
        block.id.clone()
    }
}

fn program_block_ids(
    blocks: &[agentd_protocol::ProgramBlockView],
) -> std::collections::HashSet<String> {
    blocks
        .iter()
        .flat_map(|block| [block_ref_or_id(block), block.content_id.clone()])
        .filter(|id| !id.is_empty())
        .collect()
}

impl SessionManager {
    pub(super) fn program_run_snapshot(&self, session_id: &str) -> Option<ProgramRunProgress> {
        let now_ms = Utc::now().timestamp_millis();
        let mut runs = self.program_runs.lock().ok()?;
        let expired = runs
            .get(session_id)
            .is_some_and(|run| run.expires_at_ms <= now_ms);
        if expired {
            runs.remove(session_id);
            return None;
        }
        // An empty pending set means nothing shimmers right now, so report no
        // active run — but KEEP the record so a follow-up declaration can revive
        // it within the same turn (spec 0053): a move/annotate that changes a
        // still-pending block transiently empties the set before the new id is
        // declared, and that must not destroy the run. The record is reaped when
        // the owning session goes idle/terminal or the inactivity backstop fires.
        match runs.get(session_id) {
            Some(run)
                if !run.pending_block_refs.is_empty() || !run.pending_block_ids.is_empty() =>
            {
                let mut out = run.clone();
                if !out.pending_block_refs.is_empty() {
                    if let Ok((program, blocks)) = self.storage.read_program_with_blocks(session_id)
                    {
                        let refs: std::collections::HashSet<String> =
                            out.pending_block_refs.iter().cloned().collect();
                        out.pending_block_ids = blocks
                            .iter()
                            .filter(|block| refs.contains(&block_ref_or_id(block)))
                            .map(|block| block.content_id.clone())
                            .collect();
                        if out.pending_block_ids.is_empty() && !program.markdown.trim().is_empty() {
                            out.pending_block_ids = out.pending_block_refs.clone();
                        }
                    } else {
                        out.pending_block_ids = out.pending_block_refs.clone();
                    }
                }
                Some(out)
            }
            _ => None,
        }
    }

    /// Build the per-block projection (spec 0053): each block of `markdown` with
    /// its stable ref, text, and current shimmer state from the active run.
    pub(super) fn program_blocks_projection(
        &self,
        session_id: &str,
        markdown: &str,
    ) -> Vec<agentd_protocol::ProgramBlockView> {
        let run = self
            .program_runs
            .lock()
            .ok()
            .and_then(|runs| runs.get(session_id).cloned())
            .filter(|run| run.expires_at_ms > Utc::now().timestamp_millis());
        let pending_refs: std::collections::HashSet<String> = run
            .as_ref()
            .map(|run| run.pending_block_refs.iter().cloned().collect())
            .unwrap_or_default();
        let pending_ids: std::collections::HashSet<String> = run
            .as_ref()
            .map(|run| run.pending_block_ids.iter().cloned().collect())
            .unwrap_or_default();
        let has_stable_refs = !pending_refs.is_empty();
        let tooltips = run
            .map(|run| run.pending_block_tooltips)
            .unwrap_or_default();
        let blocks = self
            .storage
            .read_program_with_blocks(session_id)
            .map(|(_, blocks)| blocks)
            .unwrap_or_else(|_| {
                agentd_protocol::program_block_spans(markdown)
                    .into_iter()
                    .map(|span| agentd_protocol::ProgramBlockView {
                        id: span.id.clone(),
                        block_id: String::new(),
                        content_epoch: 0,
                        block_ref: String::new(),
                        content_id: span.id,
                        start_line: span.start_line,
                        end_line: span.end_line,
                        text: span.text,
                        shimmer: false,
                        tooltip: None,
                    })
                    .collect()
            });
        blocks
            .into_iter()
            .map(|mut block| {
                let key = block_ref_or_id(&block);
                block.shimmer = if has_stable_refs {
                    pending_refs.contains(&key)
                } else {
                    pending_ids.contains(&key) || pending_ids.contains(&block.content_id)
                };
                block.tooltip = tooltips
                    .get(&key)
                    .or_else(|| tooltips.get(&block.content_id))
                    .cloned();
                block
            })
            .collect()
    }

    pub(super) fn start_program_run(
        &self,
        session_id: &str,
        body: &str,
        is_selection: bool,
        initial: Option<&[bool]>,
    ) -> Option<ProgramRunProgress> {
        let blocks = match self.storage.read_program_with_blocks(session_id) {
            Ok((program, blocks)) if program.markdown.trim() == body.trim() => blocks,
            _ => agentd_protocol::program_block_spans(body)
                .into_iter()
                .map(|span| agentd_protocol::ProgramBlockView {
                    id: span.id.clone(),
                    block_id: String::new(),
                    content_epoch: 0,
                    block_ref: String::new(),
                    content_id: span.id,
                    start_line: span.start_line,
                    end_line: span.end_line,
                    text: span.text,
                    shimmer: false,
                    tooltip: None,
                })
                .collect(),
        };
        if blocks.is_empty() {
            if let Ok(mut runs) = self.program_runs.lock() {
                runs.remove(session_id);
            }
            return None;
        }
        let body_ids: std::collections::HashSet<String> =
            blocks.iter().map(block_ref_or_id).collect();
        let now_ms = Utc::now().timestamp_millis();
        let pending: std::collections::HashSet<String> =
            if let Some(decl) = initial.filter(|d| d.len() == blocks.len()) {
                // Explicit initial pending set, in document order (spec 0053).
                blocks
                    .iter()
                    .zip(decl.iter())
                    .filter(|(_, &on)| on)
                    .map(|(block, _)| block_ref_or_id(block))
                    .collect()
            } else if is_selection {
                body_ids
            } else if let Ok(runs) = self.program_runs.lock() {
                if let Some(old) = runs.get(session_id) {
                    // Re-run mid-flight preserves the agent's prior narrowing:
                    // keep only blocks that are still pending and still present.
                    let old_ids: std::collections::HashSet<String> = old
                        .pending_block_refs
                        .iter()
                        .chain(old.pending_block_ids.iter())
                        .cloned()
                        .collect();
                    let kept: std::collections::HashSet<String> =
                        body_ids.intersection(&old_ids).cloned().collect();
                    if kept.is_empty() {
                        body_ids
                    } else {
                        kept
                    }
                } else {
                    body_ids
                }
            } else {
                body_ids
            };
        if pending.is_empty() {
            // An explicit all-settled initial set leaves nothing to shimmer.
            if let Ok(mut runs) = self.program_runs.lock() {
                runs.remove(session_id);
            }
            return None;
        }
        let run = ProgramRunProgress {
            run_id: format!("{session_id}:{now_ms}"),
            started_at_ms: now_ms,
            expires_at_ms: now_ms + PROGRAM_RUN_MAX_MS,
            pending_block_ids: Vec::new(),
            pending_block_refs: pending.into_iter().collect(),
            pending_block_tooltips: std::collections::HashMap::new(),
            seen_running: false,
            first_output_seen: false,
            // Unmanaged until the agent narrows it with a declaration/edit.
            // Until then it is the optimistic full-program shimmer and stays
            // subject to the owning-session idle stop signal.
            agent_managed: false,
        };
        if let Ok(mut runs) = self.program_runs.lock() {
            runs.insert(session_id.to_string(), run.clone());
        }
        Some(run)
    }

    /// Apply a partial shimmer declaration after an edit (spec 0053): drop
    /// blocks whose id no longer exists (changed/removed), then set each
    /// declared id pending or settled. Ids absent from the post-edit document
    /// are ignored (fail closed — the block changed underneath the caller).
    pub(super) fn narrow_program_run(
        &self,
        session_id: &str,
        markdown: &str,
        decls: &[agentd_protocol::ProgramShimmerDecl],
    ) {
        let now_ms = Utc::now().timestamp_millis();
        let blocks = self.program_blocks_projection(session_id, markdown);
        let current = program_block_ids(&blocks);
        let by_decl: std::collections::HashMap<String, String> = blocks
            .iter()
            .flat_map(|block| {
                let key = block_ref_or_id(block);
                [(key.clone(), key.clone()), (block.content_id.clone(), key)]
            })
            .filter(|(from, _)| !from.is_empty())
            .collect();
        if let Ok(mut runs) = self.program_runs.lock() {
            let Some(run) = runs.get_mut(session_id) else {
                return;
            };
            // A declaration/edit during the run means the agent is actively
            // managing it: from here on, trust the declarations and the
            // inactivity backstop to clear it, not the owning session's idle
            // transition (a self-scheduling agent goes idle while delegated or
            // background work is still in flight). See spec 0042.
            run.agent_managed = true;
            // Refresh the inactivity backstop — the run is still being worked.
            run.expires_at_ms = now_ms + PROGRAM_RUN_MAX_MS;
            run.pending_block_refs.retain(|id| current.contains(id));
            run.pending_block_ids.retain(|id| current.contains(id));
            for decl in decls {
                let Some(key) = by_decl.get(&decl.id).cloned() else {
                    continue;
                };
                if decl.shimmer {
                    if !run.pending_block_refs.contains(&key) {
                        run.pending_block_refs.push(key.clone());
                    }
                    if let Some(tip) = decl
                        .tooltip
                        .as_deref()
                        .and_then(agentd_protocol::normalize_program_tooltip)
                    {
                        run.pending_block_tooltips.insert(key, tip);
                    }
                } else {
                    run.pending_block_refs.retain(|id| id != &key);
                    run.pending_block_ids.retain(|id| id != &decl.id);
                    run.pending_block_tooltips.remove(&key);
                    run.pending_block_tooltips.remove(&decl.id);
                }
            }
            run.pending_block_tooltips.retain(|id, _| {
                run.pending_block_refs.contains(id) || run.pending_block_ids.contains(id)
            });
            run.pending_block_ids.clear();
            // Reap only on the inactivity backstop. An empty pending set does
            // NOT remove the run mid-turn (spec 0053): a still-running agent may
            // re-declare a moved block's new id next, and destroying the run
            // would make that revival a no-op. Idle/terminal reaping is owned by
            // note_session_state_for_program_run.
            if run.expires_at_ms <= now_ms {
                runs.remove(session_id);
            }
        }
    }

    /// Authoritatively replace a run's pending set with `pending` — a map from
    /// each pending block's id to its optional run-status tooltip, intersected
    /// with blocks present in `markdown`. Used by a program update's complete
    /// declaration (specs 0053, 0056); a no-op when no run is active.
    pub(super) fn set_program_run_pending(
        &self,
        session_id: &str,
        markdown: &str,
        pending: std::collections::HashMap<String, Option<String>>,
    ) {
        let now_ms = Utc::now().timestamp_millis();
        let blocks = self.program_blocks_projection(session_id, markdown);
        let current = program_block_ids(&blocks);
        let by_decl: std::collections::HashMap<String, String> = blocks
            .iter()
            .flat_map(|block| {
                let key = block_ref_or_id(block);
                [(key.clone(), key.clone()), (block.content_id.clone(), key)]
            })
            .filter(|(from, _)| !from.is_empty())
            .collect();
        if let Ok(mut runs) = self.program_runs.lock() {
            let Some(run) = runs.get_mut(session_id) else {
                return;
            };
            // A complete declaration is active management (spec 0042): keep the
            // run alive past owning-session idle and refresh the backstop.
            run.agent_managed = true;
            run.expires_at_ms = now_ms + PROGRAM_RUN_MAX_MS;
            let pending: Vec<(String, Option<String>)> = pending
                .into_iter()
                .filter_map(|(id, tip)| {
                    by_decl
                        .get(&id)
                        .filter(|key| current.contains(*key))
                        .cloned()
                        .map(|key| (key, tip))
                })
                .collect();
            run.pending_block_tooltips = pending
                .iter()
                .filter_map(|(id, tip)| {
                    tip.as_deref()
                        .and_then(agentd_protocol::normalize_program_tooltip)
                        .map(|t| (id.clone(), t))
                })
                .collect();
            run.pending_block_refs = pending.into_iter().map(|(id, _)| id).collect();
            run.pending_block_ids.clear();
            // Reap only on the inactivity backstop (spec 0053); an empty
            // declaration mid-turn keeps the run alive for revival.
            if run.expires_at_ms <= now_ms {
                runs.remove(session_id);
            }
        }
    }

    pub(super) fn mark_program_run_output_seen(&self, session_id: &str) {
        let mut updated = false;
        if let Ok(mut runs) = self.program_runs.lock() {
            if let Some(run) = runs.get_mut(session_id) {
                if !run.first_output_seen {
                    run.first_output_seen = true;
                    updated = true;
                }
            }
        }
        if updated {
            if let Ok(program) = self.storage.read_program(session_id) {
                self.broadcast_program_state(program);
            }
        }
    }

    pub(super) fn note_session_state_for_program_run(
        &self,
        session_id: &str,
        state: agentd_protocol::SessionState,
    ) {
        use agentd_protocol::SessionState;
        let mut clear = false;
        let mut updated = false;
        if let Ok(mut runs) = self.program_runs.lock() {
            if let Some(run) = runs.get_mut(session_id) {
                match state {
                    SessionState::Running => {
                        if !run.seen_running {
                            run.seen_running = true;
                            updated = true;
                        }
                    }
                    SessionState::Done | SessionState::Errored => {
                        // Terminal: the owning agent is gone and can never
                        // settle the remaining blocks, so clear authoritatively
                        // once the run was seen running — whether or not it is
                        // agent-managed.
                        if run.seen_running {
                            clear = true;
                        }
                    }
                    SessionState::AwaitingInput => {
                        // Idle but still alive. For an unmanaged run (a
                        // non-declaring harness's optimistic shimmer, never
                        // narrowed) this is the turn-end stop signal. For a
                        // managed run it is NOT — unless its pending set is empty:
                        // a self-scheduling agent goes idle while delegated work
                        // is still pending (keep shimmering), but a managed run
                        // with nothing pending has either finished or only
                        // transiently emptied, and an idle turn means there is no
                        // pending declaration to revive — so reap it rather than
                        // letting an empty record linger to the backstop. See
                        // specs 0042 and 0053.
                        if run.seen_running
                            && (!run.agent_managed
                                || (run.pending_block_refs.is_empty()
                                    && run.pending_block_ids.is_empty()))
                        {
                            clear = true;
                        }
                    }
                    SessionState::Pending | SessionState::Paused => {}
                }
            }
            if clear {
                runs.remove(session_id);
            }
        }
        if clear || updated {
            if let Ok(program) = self.storage.read_program(session_id) {
                self.broadcast_program_state(program);
            }
        }
    }
}
