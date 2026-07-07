//! Inline title-bar rename: click a session's rendered name (in the normal
//! session view or the program popup title) to edit it in place, `Enter`
//! commits, `Esc` cancels. A faster path alongside the existing
//! `MinibufferIntent::Rename` bottom-prompt flow (`r` key / ☰ menu), which is
//! left untouched.

use super::*;

impl App {
    /// Start (or replace) an in-progress inline rename of `session_id`'s
    /// title-bar name, seeded from its current title with the cursor at the
    /// end — the same prefill `OpenRename` uses for the bottom minibuffer
    /// prompt. A no-op if the session no longer exists.
    pub(super) fn start_title_rename(&mut self, session_id: String) {
        let Some(current) = self
            .sessions
            .iter()
            .find(|s| s.id == session_id)
            .map(|s| s.title.clone().unwrap_or_default())
        else {
            return;
        };
        let cursor = current.chars().count();
        self.title_rename = Some(TitleRename {
            session_id,
            buffer: current,
            cursor,
        });
    }

    fn title_rename_push_char(&mut self, c: char) {
        if let Some(r) = self.title_rename.as_mut() {
            let pos = byte_pos(&r.buffer, r.cursor);
            r.buffer.insert(pos, c);
            r.cursor += 1;
        }
    }

    fn title_rename_backspace(&mut self) {
        if let Some(r) = self.title_rename.as_mut() {
            if r.cursor > 0 {
                let prev = r.cursor - 1;
                let pos = byte_pos(&r.buffer, prev);
                r.buffer.remove(pos);
                r.cursor = prev;
            }
        }
    }

    fn title_rename_delete_forward(&mut self) {
        if let Some(r) = self.title_rename.as_mut() {
            if r.cursor < r.buffer.chars().count() {
                let pos = byte_pos(&r.buffer, r.cursor);
                r.buffer.remove(pos);
            }
        }
    }

    /// `C-f`/`C-b`/Left/Right: move the cursor by one char, clamped to the
    /// buffer's bounds.
    fn title_rename_move_cursor(&mut self, delta: isize) {
        if let Some(r) = self.title_rename.as_mut() {
            let len = r.buffer.chars().count();
            r.cursor = if delta < 0 {
                r.cursor.saturating_sub(delta.unsigned_abs())
            } else {
                r.cursor.saturating_add(delta as usize).min(len)
            };
        }
    }

    /// `C-a`/`C-e`/Home/End: jump the cursor to the start or end of the buffer.
    fn title_rename_cursor_to_edge(&mut self, end: bool) {
        if let Some(r) = self.title_rename.as_mut() {
            r.cursor = if end { r.buffer.chars().count() } else { 0 };
        }
    }

    /// `C-k`: kill from the cursor to the end of the buffer.
    fn title_rename_kill_to_end(&mut self) {
        if let Some(r) = self.title_rename.as_mut() {
            let pos = byte_pos(&r.buffer, r.cursor);
            r.buffer.truncate(pos);
        }
    }

    /// Commit the in-progress rename via `client.set_title` — the same RPC
    /// and empty-clears-title behavior as `MinibufferIntent::Rename`
    /// (`crates/cli/src/app/minibuffer.rs`), including the optimistic local
    /// update of `self.sessions`.
    async fn commit_title_rename(&mut self) {
        let Some(rename) = self.title_rename.take() else {
            return;
        };
        let trimmed = rename.buffer.trim().to_string();
        let new_title = if trimmed.is_empty() {
            None
        } else {
            Some(trimmed)
        };
        match self
            .client
            .set_title(&rename.session_id, new_title.clone())
            .await
        {
            Ok(()) => {
                if let Some(i) = self.sessions.iter().position(|s| s.id == rename.session_id) {
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

    /// Discard the in-progress rename without touching the session's title.
    fn cancel_title_rename(&mut self) {
        self.title_rename = None;
    }

    /// Route a key while an inline rename owns input. Mirrors
    /// `handle_session_picker_key`'s editing primitives: typing inserts,
    /// Backspace/Delete remove, Left/Right/Home/End and the Emacs
    /// equivalents move the cursor, `C-k` kills to end. `Enter` commits,
    /// `Esc`/`C-g` cancels.
    pub(super) async fn handle_title_rename_key(&mut self, key: KeyEvent) {
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        match key.code {
            KeyCode::Esc => self.cancel_title_rename(),
            KeyCode::Char('g') if ctrl => self.cancel_title_rename(),
            KeyCode::Enter => self.commit_title_rename().await,
            KeyCode::Left => self.title_rename_move_cursor(-1),
            KeyCode::Right => self.title_rename_move_cursor(1),
            KeyCode::Home => self.title_rename_cursor_to_edge(false),
            KeyCode::End => self.title_rename_cursor_to_edge(true),
            KeyCode::Char('f') if ctrl => self.title_rename_move_cursor(1),
            KeyCode::Char('b') if ctrl => self.title_rename_move_cursor(-1),
            KeyCode::Char('a') if ctrl => self.title_rename_cursor_to_edge(false),
            KeyCode::Char('e') if ctrl => self.title_rename_cursor_to_edge(true),
            KeyCode::Char('k') if ctrl => self.title_rename_kill_to_end(),
            KeyCode::Backspace => self.title_rename_backspace(),
            KeyCode::Delete => self.title_rename_delete_forward(),
            KeyCode::Char(c) if !ctrl && !alt && !super_mod => self.title_rename_push_char(c),
            _ => {}
        }
    }
}
