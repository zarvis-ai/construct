use std::collections::HashMap;

use anyhow::Result;
use chrono::Utc;

use super::SessionManager;
use crate::storage::Storage;
use construct_protocol::{SessionEvent, SessionWidgetDeleteParams, UiPanel};

#[derive(Debug, Clone)]
pub(super) struct WidgetSnapshot {
    files: HashMap<String, UiPanel>,
}

fn ui_panel_changed(previous: Option<&UiPanel>, next: &UiPanel) -> bool {
    let Some(previous) = previous else {
        return true;
    };
    previous.source != next.source
        || previous.title != next.title
        || previous.created_at_ms != next.created_at_ms
        || previous.placement != next.placement
        || previous.markdown != next.markdown
}

impl WidgetSnapshot {
    pub(super) fn read(storage: &Storage, session_id: &str) -> Self {
        let files = storage
            .read_widgets(session_id)
            .unwrap_or_else(|e| {
                tracing::warn!(session = %session_id, error = ?e, "read widgets failed");
                Vec::new()
            })
            .into_iter()
            .map(|panel| (panel.id.clone(), panel))
            .collect();
        Self { files }
    }
}

impl SessionManager {
    pub fn spawn_widget_watcher(self: &std::sync::Arc<Self>) {
        let manager = self.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(super::WIDGET_WATCH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
            loop {
                interval.tick().await;
                manager.poll_widget_files().await;
            }
        });
    }

    pub async fn delete_widget(&self, p: SessionWidgetDeleteParams) -> Result<()> {
        self.get_entry(&p.session_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("session not found: {}", p.session_id))?;
        self.storage.delete_widget(&p.session_id, &p.panel_id)?;
        self.broadcast_widget_event(&p.session_id, SessionEvent::UiDelete { id: p.panel_id });
        Ok(())
    }

    async fn poll_widget_files(&self) {
        let session_ids: Vec<String> = self.sessions.read().await.keys().cloned().collect();
        let mut snapshots = self.widget_snapshots.lock().await;
        for session_id in &session_ids {
            let next = WidgetSnapshot::read(&self.storage, session_id);
            let previous = snapshots
                .get(session_id)
                .cloned()
                .unwrap_or_else(|| WidgetSnapshot {
                    files: HashMap::new(),
                });
            for (id, panel) in &next.files {
                if !ui_panel_changed(previous.files.get(id), panel) {
                    continue;
                }
                self.broadcast_widget_event(&session_id, SessionEvent::UiPanel(panel.clone()));
            }
            for id in previous.files.keys() {
                if !next.files.contains_key(id) {
                    self.broadcast_widget_event(
                        &session_id,
                        SessionEvent::UiDelete { id: id.clone() },
                    );
                }
            }
            snapshots.insert(session_id.clone(), next);
        }
        snapshots.retain(|id, _| session_ids.contains(id));
    }

    pub(super) fn broadcast_widget_event(&self, session_id: &str, event: SessionEvent) {
        let _ = self.broadcast.send(super::BroadcastMsg::Event(
            construct_protocol::EventNotificationPayload {
                session_id: session_id.to_string(),
                at: Utc::now(),
                event,
                seq: 0,
            },
        ));
    }
}
