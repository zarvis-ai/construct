use super::*;

impl SessionManager {
    pub async fn set_session_group(
        &self,
        session_id: &str,
        new_group_id: Option<String>,
        position: agentd_protocol::SessionGroupPosition,
    ) -> Result<()> {
        let all_sessions = self.list().await;
        let edge = match position {
            agentd_protocol::SessionGroupPosition::Top => RegionEdge::Top,
            agentd_protocol::SessionGroupPosition::Bottom => RegionEdge::Bottom,
        };
        self.move_session_into_region(session_id, &new_group_id, edge, &all_sessions)
            .await
    }

    pub(super) async fn move_session_into_region(
        &self,
        session_id: &str,
        new_group_id: &Option<String>,
        edge: RegionEdge,
        all_sessions: &[SessionSummary],
    ) -> Result<()> {
        // Pick a position that puts us at the requested edge of the region.
        let region_positions: Vec<i64> = all_sessions
            .iter()
            .filter(|s| s.id != session_id && s.group_id == *new_group_id)
            .map(|s| s.position)
            .collect();
        let new_pos = match edge {
            RegionEdge::Top => {
                let min = region_positions.iter().min().copied().unwrap_or(0);
                min - 1
            }
            RegionEdge::Bottom => {
                let max = region_positions.iter().max().copied().unwrap_or(0);
                max + 1
            }
        };

        let entry = self
            .get_entry(session_id)
            .await
            .ok_or_else(|| anyhow!("session not found: {}", session_id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.group_id = new_group_id.clone();
            s.position = new_pos;
            s.clone()
        };
        self.storage.save_summary(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::State(StateNotificationPayload {
                session: snapshot,
            }));
        Ok(())
    }

    /// ----- Groups -----

    pub async fn list_groups(&self) -> Vec<GroupSummary> {
        let guard = self.groups.read().await;
        let mut out = Vec::with_capacity(guard.len());
        for entry in guard.values() {
            out.push(entry.summary().await);
        }
        out.sort_by_key(|g| g.position);
        out
    }

    pub async fn create_group(&self, name: String) -> Result<String> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("group name is empty"));
        }
        let id = format!("g{}", uuid::Uuid::new_v4().simple());
        let now = Utc::now();
        let summary = GroupSummary {
            id: id.clone(),
            name: name.to_string(),
            created_at: now,
            position: -now.timestamp_millis(),
            collapsed: false,
        };
        self.storage.save_group(&summary)?;
        self.groups.write().await.insert(
            id.clone(),
            Arc::new(GroupEntry {
                summary: RwLock::new(summary.clone()),
            }),
        );
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: summary,
            }));
        Ok(id)
    }

    pub async fn rename_group(&self, id: &str, name: String) -> Result<()> {
        let name = name.trim();
        if name.is_empty() {
            return Err(anyhow!("group name is empty"));
        }
        let entry = self
            .groups
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("group not found: {}", id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.name = name.to_string();
            s.clone()
        };
        self.storage.save_group(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snapshot,
            }));
        Ok(())
    }

    pub async fn set_group_collapsed(&self, id: &str, collapsed: bool) -> Result<()> {
        let entry = self
            .groups
            .read()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| anyhow!("group not found: {}", id))?;
        let snapshot = {
            let mut s = entry.summary.write().await;
            s.collapsed = collapsed;
            s.clone()
        };
        self.storage.save_group(&snapshot)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snapshot,
            }));
        Ok(())
    }

    /// Delete a group. When `delete_members` is false (default), member
    /// sessions are orphaned: their `group_id` clears to `None` and they
    /// survive. When true, every member session is fully deleted first
    /// (adapter killed, on-disk session dir removed, worktree torn down)
    /// before the group itself is removed.
    pub async fn delete_group(&self, id: &str, delete_members: bool) -> Result<()> {
        // Collect member ids BEFORE we drop the group entry so we don't
        // race with a concurrent set_session_group that might re-parent
        // them under a different group while we're working.
        let member_ids: Vec<String> = {
            let sessions = self.sessions.read().await;
            let mut ids = Vec::new();
            for (sid, entry) in sessions.iter() {
                let s = entry.summary.read().await;
                if s.group_id.as_deref() == Some(id) {
                    ids.push(sid.clone());
                }
            }
            ids
        };

        let entry = self.groups.write().await.remove(id);
        if entry.is_none() {
            return Err(anyhow!("group not found: {}", id));
        }

        if delete_members {
            // Cascade-delete: tear down each member session. Errors are
            // logged but don't abort the cascade — a single broken
            // session shouldn't strand the rest in a now-missing group.
            for sid in &member_ids {
                if let Err(e) = self.delete(sid).await {
                    tracing::warn!(
                        group = %id,
                        session = %sid,
                        error = %e,
                        "group cascade-delete: member delete failed",
                    );
                }
            }
        } else {
            // Orphan members: clear their group_id and rebroadcast.
            for sid in &member_ids {
                let Some(s_entry) = self.sessions.read().await.get(sid).cloned() else {
                    continue;
                };
                let snapshot = {
                    let mut s = s_entry.summary.write().await;
                    s.group_id = None;
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
        let _ = self.storage.remove_group(id);
        let _ = self.broadcast.send(BroadcastMsg::GroupDeleted(
            GroupDeletedNotificationPayload {
                group_id: id.to_string(),
            },
        ));
        Ok(())
    }

    /// Swap a group's position with its neighbor in the requested direction.
    /// No-op at the edges.
    pub async fn move_group(&self, id: &str, dir: MoveDirection) -> Result<()> {
        let groups = self.list_groups().await; // sorted by position
        let idx = groups
            .iter()
            .position(|g| g.id == id)
            .ok_or_else(|| anyhow!("group not found: {}", id))?;
        let neighbor_idx = match dir {
            MoveDirection::Up => {
                if idx == 0 {
                    return Ok(());
                }
                idx - 1
            }
            MoveDirection::Down => {
                if idx + 1 >= groups.len() {
                    return Ok(());
                }
                idx + 1
            }
        };
        let a_id = groups[idx].id.clone();
        let b_id = groups[neighbor_idx].id.clone();
        let a_pos = groups[idx].position;
        let b_pos = groups[neighbor_idx].position;
        let entry_a = self
            .groups
            .read()
            .await
            .get(&a_id)
            .cloned()
            .ok_or_else(|| anyhow!("group missing"))?;
        let entry_b = self
            .groups
            .read()
            .await
            .get(&b_id)
            .cloned()
            .ok_or_else(|| anyhow!("group missing"))?;
        let snap_a = {
            let mut s = entry_a.summary.write().await;
            s.position = b_pos;
            s.clone()
        };
        let snap_b = {
            let mut s = entry_b.summary.write().await;
            s.position = a_pos;
            s.clone()
        };
        self.storage.save_group(&snap_a)?;
        self.storage.save_group(&snap_b)?;
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snap_a,
            }));
        let _ = self
            .broadcast
            .send(BroadcastMsg::GroupState(GroupStateNotificationPayload {
                group: snap_b,
            }));
        Ok(())
    }
}

/// Returns the `group_id` of the region immediately above the given region
/// in display order. `Some(None)` = ungrouped; `Some(Some(id))` = group N-1.
/// `None` = there is nothing above (ungrouped is already at the top).
pub(super) fn region_above(region: Option<&str>, groups: &[GroupSummary]) -> Option<Option<String>> {
    match region {
        None => None,
        Some(id) => {
            let idx = groups.iter().position(|g| g.id == id)?;
            if idx == 0 {
                Some(None)
            } else {
                Some(Some(groups[idx - 1].id.clone()))
            }
        }
    }
}

/// Returns the `group_id` of the region immediately below the given region
/// in display order. `Some(Some(id))` = next group. `None` = nothing below.
pub(super) fn region_below(region: Option<&str>, groups: &[GroupSummary]) -> Option<Option<String>> {
    match region {
        None => groups.first().map(|g| Some(g.id.clone())),
        Some(id) => {
            let idx = groups.iter().position(|g| g.id == id)?;
            groups.get(idx + 1).map(|g| Some(g.id.clone()))
        }
    }
}

/// True if the group `id` exists and is currently collapsed.
pub(super) fn group_collapsed(id: &str, groups: &[GroupSummary]) -> bool {
    groups
        .iter()
        .find(|g| g.id == id)
        .map(|g| g.collapsed)
        .unwrap_or(false)
}

/// Like [`region_above`], but skips over collapsed groups. A collapsed
/// project hides its member sessions, so reordering a visible session past it
/// should jump the entire project in one step rather than swapping with each
/// hidden member. Returns the first non-collapsed region above (the ungrouped
/// region is never collapsed), or `None` if there is nothing above.
pub(super) fn region_above_skipping_collapsed(
    region: Option<&str>,
    groups: &[GroupSummary],
) -> Option<Option<String>> {
    let mut target = region_above(region, groups);
    loop {
        match target {
            Some(Some(gid)) if group_collapsed(&gid, groups) => {
                target = region_above(Some(gid.as_str()), groups);
            }
            other => return other,
        }
    }
}

/// Like [`region_below`], but skips over collapsed groups so a reorder jumps
/// the whole collapsed project in one step. See
/// [`region_above_skipping_collapsed`].
pub(super) fn region_below_skipping_collapsed(
    region: Option<&str>,
    groups: &[GroupSummary],
) -> Option<Option<String>> {
    let mut target = region_below(region, groups);
    loop {
        match target {
            Some(Some(gid)) if group_collapsed(&gid, groups) => {
                target = region_below(Some(gid.as_str()), groups);
            }
            other => return other,
        }
    }
}
