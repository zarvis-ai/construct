use super::*;

impl App {
    pub fn open_session_title_menu(&mut self, session_id: String, view: ratatui::layout::Rect) {
        const MENU_W: u16 = 26;
        let menu_h = SessionTitleMenuAction::ALL.len() as u16 + 2;
        let width = MENU_W.min(view.width.saturating_sub(2).max(1));
        let x = view
            .x
            .saturating_add(view.width)
            .saturating_sub(width.saturating_add(1));
        self.session_title_menu = Some(SessionTitleMenu {
            session_id,
            area: ratatui::layout::Rect {
                x,
                y: view.y.saturating_add(1),
                width,
                height: menu_h.min(view.height.saturating_sub(1).max(3)),
            },
        });
    }

    pub(super) async fn run_session_title_menu_action(
        &mut self,
        session_id: String,
        action: SessionTitleMenuAction,
    ) {
        self.session_title_menu = None;
        if self.selected_id().as_deref() != Some(session_id.as_str()) {
            self.select_session(session_id.clone());
            self.sync_active_window_selection();
        }
        match action {
            SessionTitleMenuAction::Rename => {
                self.run_action(crate::keymap::KeyAction::OpenRename).await
            }
            SessionTitleMenuAction::SplitHorizontal => {
                self.split_active_window(WindowSplitDirection::Right)
            }
            SessionTitleMenuAction::SplitVertical => {
                self.split_active_window(WindowSplitDirection::Below)
            }
            SessionTitleMenuAction::CloseSplit => self.delete_active_window(),
            SessionTitleMenuAction::Archive => {
                let archived = self
                    .sessions
                    .iter()
                    .find(|s| s.id == session_id)
                    .is_some_and(|s| s.archived);
                let (verb, intent) = if archived {
                    (
                        "Unarchive",
                        MinibufferIntent::MenuUnarchiveConfirm {
                            session_id: session_id.clone(),
                        },
                    )
                } else {
                    (
                        "Archive",
                        MinibufferIntent::MenuArchiveConfirm {
                            session_id: session_id.clone(),
                        },
                    )
                };
                self.minibuffer = Some(Minibuffer {
                    prompt: format!("{verb} session {}? (y/N): ", short_id(&session_id)),
                    input: String::new(),
                    cursor: 0,
                    intent,
                    error: None,
                });
            }
            SessionTitleMenuAction::Delete => {
                self.minibuffer = Some(Minibuffer {
                    prompt: format!(
                        "Delete session {}? This drops transcript + worktree. (y/N): ",
                        short_id(&session_id)
                    ),
                    input: String::new(),
                    cursor: 0,
                    intent: MinibufferIntent::MenuDeleteConfirm { session_id },
                    error: None,
                });
            }
        }
    }
}
