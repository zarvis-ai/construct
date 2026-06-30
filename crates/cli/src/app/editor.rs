use super::*;

impl App {
    pub(super) fn place_program_cursor(&mut self, modal: ratatui::layout::Rect, col: u16, row: u16) {
        let cursor = {
            let app: &App = self;
            let Some(popup) = app.program_popup.as_ref() else {
                return;
            };
            program_cursor_at_modal_point(
                Some(app),
                &popup.buffer,
                modal,
                popup.scroll_offset,
                col,
                row,
            )
            .unwrap_or(0)
        };
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.closing = false;
        popup.cursor = cursor;
        popup.preferred_col = None;
        popup.selection = None;
        popup.smart_clip = None;
    }

    pub(super) async fn handle_program_mouse(&mut self, ev: &MouseEvent) -> bool {
        use crossterm::event::MouseButton;
        let Some(modal) = self.layout.modal_area else {
            return false;
        };
        if self.program_popup.is_none() {
            return false;
        }
        if let Some(menu) = self.session_title_menu.clone() {
            if let Some(action) = menu.item_at(ev.column, ev.row) {
                if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
                    self.run_session_title_menu_action(menu.session_id, action)
                        .await;
                }
                return true;
            }
            if menu.contains(ev.column, ev.row) {
                return true;
            }
            if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.session_title_menu = None;
            }
        }
        let contains = ev.column >= modal.x
            && ev.column < modal.x.saturating_add(modal.width)
            && ev.row >= modal.y
            && ev.row < modal.y.saturating_add(modal.height);
        if !contains {
            return false;
        }
        // A left click anywhere inside the program modal reclaims keyboard focus
        // for the view pane. Without this, clicking the session list (focus →
        // List) and then clicking back on the program placed the caret but left
        // `focus == List`, so the `on_key` routing gate kept sending keystrokes
        // to the list and typing into the program silently did nothing. Opening
        // or hide→show-ing the program already focuses the view (see
        // `open_program_popup`), which is why those workarounds restored typing.
        // The session-clip handler below re-points focus at the list when the
        // click switches sessions, so that case still behaves correctly.
        if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            self.focus = PaneFocus::View;
        }
        let title_run_hit = self.layout.program_title_run_hit;
        let title_toggle_hit = self.layout.program_title_toggle_hit;
        let title_close_hit = self.layout.program_title_close_hit;
        let selection_run_hit = self.layout.program_selection_run_hit;
        let hit_title_toggle = title_toggle_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        let hit_title_run = title_run_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        let hit_title_close = title_close_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        let hit_selection_run = selection_run_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        if hit_title_toggle || hit_title_run || hit_title_close || hit_selection_run {
            if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
                if hit_title_toggle {
                    self.close_program_popup().await;
                } else if hit_title_close {
                    if let Some(session_id) = self
                        .program_popup
                        .as_ref()
                        .map(|popup| popup.program.session_id.clone())
                    {
                        self.open_session_title_menu(session_id, modal);
                    }
                } else {
                    let selected = hit_selection_run.then(|| {
                        self.program_popup.as_ref().and_then(|popup| {
                            Some((
                                Self::selected_program_text(popup)?,
                                Self::selected_program_block_ids(popup)?,
                            ))
                        })
                    });
                    let (selection, selected_block_ids) = selected
                        .flatten()
                        .map_or((None, None), |(text, ids)| (Some(text), Some(ids)));
                    if hit_selection_run {
                        if let Some(popup) = self.program_popup.as_mut() {
                            popup.selection = None;
                        }
                        self.layout.program_selection_run_hit = None;
                    }
                    self.execute_program_popup(selection, selected_block_ids)
                        .await;
                }
            }
            return true;
        }
        // Clicking a title-bar widget indicator pins/unpins that widget. The
        // program reuses the session view's shared widget geometry, so its icons
        // register into `dynamic_ui_widget_hits` (via `render_session_widget_title`)
        // — the same list the pane title bar uses.
        if let Some(hit) = self
            .layout
            .dynamic_ui_widget_hits
            .iter()
            .find(|hit| hit.contains(ev.column, ev.row))
            .cloned()
        {
            if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
                self.toggle_dynamic_ui_widget_pin(hit.session_id, hit.panel_id);
            }
            return true;
        }
        // Clicking a template button in the empty-program placeholder fills the
        // buffer with that template's Markdown — a starting point the user then
        // edits. Checked before the generic cursor-placement handler so the
        // click doesn't just move the caret.
        if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(hit) = self
                .layout
                .program_template_hits
                .iter()
                .find(|hit| hit.contains(ev.column, ev.row))
                .cloned()
            {
                self.apply_program_template(hit.template_id, hit.markdown);
                return true;
            }
        }
        // Clicking a session smart-clip focuses that session, just like clicking
        // its row in the session list. The program follows selection, so the
        // clicked session's program reveals in place.
        if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(session_id) = self.program_clip_session_at(ev.column, ev.row) {
                if self.sessions.iter().any(|s| s.id == session_id) {
                    self.focus = PaneFocus::List;
                    self.select_session(session_id);
                    // Point the active window pane at the target too — the main
                    // view renders from the pane's selection, not `self.selection`
                    // directly (see `render_main_windows`). The list-row click and
                    // the switch/new/fork paths all do this; without it the clip
                    // updates the selection but the pane keeps rendering the old
                    // session, so the switch never visibly lands.
                    self.sync_active_window_selection();
                }
                return true;
            }
        }
        match ev.kind {
            MouseEventKind::Down(MouseButton::Left) => {
                let cursor = {
                    let app: &App = self;
                    let Some(popup) = app.program_popup.as_ref() else {
                        return true;
                    };
                    program_cursor_at_modal_point(
                        Some(app),
                        &popup.buffer,
                        modal,
                        popup.scroll_offset,
                        ev.column,
                        ev.row,
                    )
                    .unwrap_or(0)
                };
                let Some(popup) = self.program_popup.as_mut() else {
                    return true;
                };
                popup.cursor = cursor;
                popup.preferred_col = None;
                popup.selection = Some(ProgramSelection {
                    anchor: cursor,
                    head: cursor,
                    dragged: false,
                });
                popup.smart_clip = None;
                true
            }
            MouseEventKind::Drag(MouseButton::Left) => {
                let cursor = {
                    let app: &App = self;
                    let Some(popup) = app.program_popup.as_ref() else {
                        return true;
                    };
                    program_cursor_at_modal_point(
                        Some(app),
                        &popup.buffer,
                        modal,
                        popup.scroll_offset,
                        ev.column,
                        ev.row,
                    )
                    .unwrap_or(0)
                };
                let Some(popup) = self.program_popup.as_mut() else {
                    return true;
                };
                popup.cursor = cursor;
                popup.preferred_col = None;
                if let Some(selection) = popup.selection.as_mut() {
                    selection.dragged = selection.dragged || selection.head != cursor;
                    selection.head = cursor;
                }
                true
            }
            MouseEventKind::Up(MouseButton::Left) => {
                let should_copy = self
                    .program_popup
                    .as_ref()
                    .and_then(|popup| popup.selection.as_ref())
                    .is_some_and(|selection| selection.dragged);
                if should_copy {
                    self.copy_program_selection();
                } else if let Some(popup) = self.program_popup.as_mut() {
                    popup.selection = None;
                }
                true
            }
            // Mouse wheel scrolls the body without moving the caret. The next
            // keystroke re-anchors the scroll to the cursor via follow.
            MouseEventKind::ScrollDown => {
                self.scroll_program_popup(PROGRAM_WHEEL_SCROLL_ROWS as isize);
                true
            }
            MouseEventKind::ScrollUp => {
                self.scroll_program_popup(-(PROGRAM_WHEEL_SCROLL_ROWS as isize));
                true
            }
            _ => true,
        }
    }

    pub(super) async fn handle_program_key(&mut self, key: KeyEvent) {
        if self.program_popup.as_ref().is_some_and(|p| p.closing) {
            return;
        }
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        if self.program_search_active() {
            match key.code {
                KeyCode::Esc => self.cancel_program_search(),
                KeyCode::Char('g') if ctrl => self.cancel_program_search(),
                KeyCode::Char('s') if ctrl => self.move_program_search_match(1),
                KeyCode::Char('r') if ctrl => self.move_program_search_match(-1),
                KeyCode::Enter => self.accept_program_search(),
                KeyCode::Backspace => self.delete_program_search_query_char(),
                KeyCode::Char(c) if !ctrl && !alt && !super_mod => {
                    self.append_program_search_query_char(c)
                }
                _ => {}
            }
            self.follow_program_scroll();
            return;
        }
        match key.code {
            KeyCode::Esc if self.program_smart_clip_active() => self.cancel_program_smart_clip(),
            // Esc only cancels the transient smart-clip picker (above); it is
            // intentionally NOT a program-hide affordance. Show/hide is C-x
            // Space (and the title-glyph toggle) only, so a reflexive Esc
            // while editing program prose doesn't blow away the surface.
            KeyCode::Esc => {}
            KeyCode::Enter if self.program_smart_clip_active() => self.accept_program_smart_clip(),
            KeyCode::Tab if self.program_smart_clip_active() && !ctrl && !alt => {
                self.accept_program_smart_clip()
            }
            KeyCode::Up if self.program_smart_clip_active() => {
                self.move_program_smart_clip_selection(-1)
            }
            KeyCode::Down if self.program_smart_clip_active() => {
                self.move_program_smart_clip_selection(1)
            }
            // Right drills into the highlighted category's submenu; Left backs out.
            KeyCode::Right if self.program_smart_clip_active() => {
                self.program_smart_clip_expand()
            }
            KeyCode::Left if self.program_smart_clip_active() => {
                self.program_smart_clip_collapse()
            }
            KeyCode::Char(' ') if ctrl => self.begin_program_selection(),
            KeyCode::Char('g') if ctrl => {
                // C-g cancels: dismiss the transient smart-clip picker and
                // clear any active C-Space selection mark. No-op when neither
                // is active. Like Esc, it is deliberately NOT a program-hide
                // affordance — it never closes or mutates the surface.
                self.cancel_program_smart_clip();
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.selection = None;
                }
            }
            KeyCode::Char('a') if ctrl => {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.cursor = program_line_start(&popup.buffer, popup.cursor);
                    popup.preferred_col = None;
                    Self::update_program_selection_head(popup);
                    Self::update_program_smart_clip_after_cursor_move(popup);
                }
            }
            KeyCode::Char('s') if ctrl => self.begin_program_search(),
            KeyCode::Char('e') if ctrl => {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.cursor = program_line_end(&popup.buffer, popup.cursor);
                    popup.preferred_col = None;
                    Self::update_program_selection_head(popup);
                    Self::update_program_smart_clip_after_cursor_move(popup);
                }
            }
            KeyCode::Char('b') if ctrl => self.move_program_cursor(-1),
            KeyCode::Char('f') if ctrl => self.move_program_cursor(1),
            KeyCode::Char('p') if ctrl && self.program_smart_clip_active() => {
                self.move_program_smart_clip_selection(-1)
            }
            KeyCode::Char('n') if ctrl && self.program_smart_clip_active() => {
                self.move_program_smart_clip_selection(1)
            }
            KeyCode::Char('p') if ctrl => self.move_program_cursor_vertical(-1),
            KeyCode::Char('n') if ctrl => self.move_program_cursor_vertical(1),
            KeyCode::Char('v') if ctrl => self.paste_program_clipboard(),
            KeyCode::Char('y') if ctrl => self.paste_program_clipboard(),
            KeyCode::Char('w') if ctrl => self.cut_program_selection(),
            // M-w is emacs kill-ring-save: copy the selection, never delete.
            KeyCode::Char('w') if alt => self.copy_program_selection_and_deactivate(),
            KeyCode::Char('/') if ctrl => self.undo_program_edit(),
            // Cmd-C / Ctrl-C also copy, but only when a selection exists so we
            // don't disturb existing behavior otherwise (plain C-c stays a
            // no-op here; the C-x C-c quit chord is consumed earlier in
            // handle_program_global_key, and bare Cmd-C still self-inserts 'c').
            KeyCode::Char('c')
                if (ctrl || super_mod)
                    && self
                        .program_popup
                        .as_ref()
                        .and_then(Self::program_selection_range)
                        .is_some() =>
            {
                self.copy_program_selection_and_deactivate()
            }
            KeyCode::Char('d') if ctrl => self.delete_program_forward(),
            KeyCode::Char('h') if ctrl => self.delete_program_back(),
            KeyCode::Char('k') if ctrl => self.cut_program_line(),
            KeyCode::Enter => self.insert_program_text("\n"),
            KeyCode::Backspace => self.delete_program_back(),
            KeyCode::Delete => self.delete_program_forward(),
            KeyCode::Left => self.move_program_cursor(-1),
            KeyCode::Right => self.move_program_cursor(1),
            KeyCode::Up => self.move_program_cursor_vertical(-1),
            KeyCode::Down => self.move_program_cursor_vertical(1),
            KeyCode::Char('l') if ctrl => {
                // C-l: center the current cursor row in the program viewport (emacs-like).
                self.center_program_cursor();
            }
            KeyCode::Home => {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.cursor = program_line_start(&popup.buffer, popup.cursor);
                    popup.preferred_col = None;
                    Self::update_program_selection_head(popup);
                    Self::update_program_smart_clip_after_cursor_move(popup);
                }
            }
            KeyCode::End => {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.cursor = program_line_end(&popup.buffer, popup.cursor);
                    popup.preferred_col = None;
                    Self::update_program_selection_head(popup);
                    Self::update_program_smart_clip_after_cursor_move(popup);
                }
            }
            // Tab / Shift-Tab nest and un-nest the current markdown list
            // item(s). They operate on every list line the selection spans, or
            // just the cursor's line when there is no selection.
            KeyCode::Tab if !ctrl && !alt => self.shift_program_indent(false),
            KeyCode::BackTab if !ctrl && !alt => self.shift_program_indent(true),
            KeyCode::Char(c) if !ctrl && !alt => self.insert_program_text(&c.to_string()),
            _ => {}
        }
        // Any cursor move or edit above may have pushed the caret out of the
        // visible window; re-anchor the scroll so it stays on-screen.
        self.follow_program_scroll();
    }

    /// Indent (or, when `outdent`, un-indent) the markdown list item(s) under
    /// the cursor / selection by one nesting level. Nesting is encoded as
    /// leading spaces on the source line (`PROGRAM_INDENT_UNIT` per level);
    /// non-list lines and the empty leading whitespace at the top level are
    /// left untouched. The cursor and any selection endpoints ride along with
    /// the text they were sitting on so the same logical characters stay
    /// selected / under the cursor after the shift.
    pub(super) fn shift_program_indent(&mut self, outdent: bool) {
        const PROGRAM_INDENT_UNIT: usize = 2;
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let lines: Vec<String> = popup.buffer.split('\n').map(str::to_string).collect();

        // The inclusive band of lines to touch: the selection's lines, or just
        // the cursor's line. A selection that ends exactly at a line start does
        // not pull that trailing line in (its text isn't really selected).
        let (range_start, range_end) =
            Self::program_selection_range(popup).unwrap_or((popup.cursor, popup.cursor));
        let (start_line, _) = program_offset_to_line_col(&lines, range_start);
        let (mut end_line, end_col) = program_offset_to_line_col(&lines, range_end);
        if end_line > start_line && end_col == 0 {
            end_line -= 1;
        }

        // Per-line edit at column 0, recorded as (removed_chars, inserted_chars)
        // so cursor/selection offsets can be remapped afterwards.
        let mut new_lines = lines.clone();
        let mut deltas = vec![(0usize, 0usize); lines.len()];
        let mut changed = false;
        for i in start_line..=end_line.min(lines.len().saturating_sub(1)) {
            let line = &lines[i];
            let stripped = line.trim_start();
            let is_list = stripped.starts_with("- ") || stripped.starts_with("* ");
            if !is_list {
                continue;
            }
            if outdent {
                let leading_spaces = line.chars().take_while(|&c| c == ' ').count();
                let remove = leading_spaces.min(PROGRAM_INDENT_UNIT);
                if remove == 0 {
                    continue;
                }
                new_lines[i] = line.chars().skip(remove).collect();
                deltas[i] = (remove, 0);
                changed = true;
            } else {
                new_lines[i] = format!("{}{}", " ".repeat(PROGRAM_INDENT_UNIT), line);
                deltas[i] = (0, PROGRAM_INDENT_UNIT);
                changed = true;
            }
        }
        if !changed {
            return;
        }

        // Map an old char offset onto the equivalent offset in `new_lines`. Only
        // the offset's own line can have shifted (edits are at column 0), so we
        // shift its column and re-resolve against the rebuilt lines.
        let remap = |offset: usize| -> usize {
            let (line, col) = program_offset_to_line_col(&lines, offset);
            let (removed, inserted) = deltas[line];
            let new_col = if removed > 0 {
                col.saturating_sub(removed)
            } else if inserted > 0 && col > 0 {
                col + inserted
            } else {
                col
            };
            let new_col = new_col.min(new_lines[line].chars().count());
            program_line_col_to_offset(&new_lines, line, new_col)
        };

        let new_cursor = remap(popup.cursor);
        let new_selection = popup.selection.as_ref().map(|sel| ProgramSelection {
            anchor: remap(sel.anchor),
            head: remap(sel.head),
            dragged: sel.dragged,
        });
        self.push_program_undo_state();
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.buffer = new_lines.join("\n");
        popup.cursor = new_cursor;
        popup.selection = new_selection;
        popup.preferred_col = None;
        popup.smart_clip = None;
    }

    pub(super) async fn handle_program_global_key(&mut self, key: KeyEvent) -> bool {
        let chord_active = !self.chord_state.is_empty();
        let is_ctrl_x =
            matches!(key.code, KeyCode::Char('x')) && key.modifiers.contains(KeyModifiers::CONTROL);
        if !chord_active && !is_ctrl_x {
            return false;
        }
        let res = self.chord_state.handle(key, &self.keymap);
        self.chord_label = self.chord_state.label();
        match res {
            KeymapResult::Action(action) => {
                self.chord_label.clear();
                self.run_action(action).await;
            }
            KeymapResult::Pending(label) => {
                self.chord_label = label;
            }
            KeymapResult::Unhandled => {
                self.chord_label.clear();
            }
        }
        true
    }

    pub(super) fn begin_program_selection(&mut self) {
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.smart_clip = None;
        popup.selection = Some(ProgramSelection {
            anchor: popup.cursor,
            head: popup.cursor,
            dragged: false,
        });
        self.set_status("program selection started".to_string());
    }

    pub(super) fn program_search_active(&self) -> bool {
        self.program_popup
            .as_ref()
            .and_then(|popup| popup.search.as_ref())
            .is_some()
    }

    pub(super) fn begin_program_search(&mut self) {
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.search = Some(ProgramSearch {
            anchor_cursor: popup.cursor,
            query: String::new(),
            matches: Vec::new(),
            selected: 0,
        });
        popup.smart_clip = None;
        popup.selection = None;
    }

    pub(super) fn append_program_search_query_char(&mut self, ch: char) {
        self.append_program_search_query_text(&ch.to_string());
    }

    pub(super) fn append_program_search_query_text(&mut self, text: &str) {
        {
            let Some(popup) = self.program_popup.as_mut() else {
                return;
            };
            let Some(search) = popup.search.as_mut() else {
                return;
            };
            search.query.push_str(text);
        }
        self.update_program_search_after_edit();
    }

    pub(super) fn delete_program_search_query_char(&mut self) {
        {
            let Some(popup) = self.program_popup.as_mut() else {
                return;
            };
            let Some(search) = popup.search.as_mut() else {
                return;
            };
            search.query.pop();
        }
        self.update_program_search_after_edit();
    }

    pub(super) fn update_program_search_after_edit(&mut self) {
        let Some(session_id) = self
            .program_popup
            .as_ref()
            .map(|popup| popup.program.session_id.clone())
        else {
            return;
        };
        self.refresh_program_search_for_session(&session_id);
    }

    pub(super) fn refresh_program_search_for_session(&mut self, session_id: &str) {
        let snapshot = self
            .program_popup
            .as_ref()
            .filter(|popup| popup.program.session_id == session_id)
            .or_else(|| self.program_popups.get(session_id))
            .and_then(|popup| {
                popup.search.as_ref().map(|search| {
                    (
                        popup.buffer.clone(),
                        search.query.clone(),
                        search.anchor_cursor,
                    )
                })
            });
        let Some((buffer, query, anchor_cursor)) = snapshot else {
            return;
        };
        let matches = if query.is_empty() {
            Vec::new()
        } else {
            let mut matches = program_search_matches(&buffer, &query);
            program_search_add_clip_label_matches(self, &buffer, &query, &mut matches);
            matches.sort_unstable_by_key(|(start, _)| *start);
            matches.dedup();
            matches
        };
        let popup = if self
            .program_popup
            .as_ref()
            .is_some_and(|popup| popup.program.session_id == session_id)
        {
            self.program_popup.as_mut()
        } else {
            self.program_popups.get_mut(session_id)
        };
        let Some(popup) = popup else {
            return;
        };
        let Some(search) = popup.search.as_mut() else {
            return;
        };
        search.matches = matches;
        if search.matches.is_empty() {
            search.selected = 0;
            popup.cursor = anchor_cursor;
        } else {
            let anchor_match = search
                .matches
                .iter()
                .position(|(start, _)| *start >= anchor_cursor);
            search.selected = anchor_match.unwrap_or(0);
            popup.cursor = search.matches[search.selected].0;
        }
        popup.preferred_col = None;
    }

    pub(super) fn move_program_search_match(&mut self, delta: isize) {
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        let Some(search) = popup.search.as_mut() else {
            return;
        };
        if search.matches.is_empty() {
            popup.cursor = search.anchor_cursor;
            return;
        }
        let count = search.matches.len() as isize;
        let selected = (search.selected as isize + delta).rem_euclid(count) as usize;
        popup.cursor = search.matches[selected].0;
        search.selected = selected;
        popup.preferred_col = None;
    }

    pub(super) fn accept_program_search(&mut self) {
        if let Some(popup) = self.program_popup.as_mut() {
            popup.search = None;
            popup.smart_clip = None;
        }
    }

    pub(super) fn cancel_program_search(&mut self) {
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        if let Some(search) = popup.search.take() {
            popup.cursor = search.anchor_cursor;
            popup.preferred_col = None;
            popup.smart_clip = None;
        }
    }

    pub(super) fn program_smart_clip_active(&self) -> bool {
        self.program_popup
            .as_ref()
            .and_then(|popup| popup.smart_clip.as_ref())
            .is_some()
    }

    pub(super) fn cancel_program_smart_clip(&mut self) {
        if let Some(popup) = self.program_popup.as_mut() {
            popup.smart_clip = None;
        }
    }

    pub(super) fn move_program_smart_clip_selection(&mut self, delta: isize) {
        let selectable = self
            .program_popup
            .as_ref()
            .map(|popup| Self::program_smart_clip_selectable_count(&self.program_smart_clip_rows(popup)))
            .unwrap_or(0);
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        let Some(search) = popup.smart_clip.as_mut() else {
            return;
        };
        if selectable == 0 {
            search.selected = 0;
            return;
        }
        let selected = search.selected.min(selectable - 1);
        search.selected = if delta < 0 {
            selected
                .saturating_add(selectable)
                .saturating_sub(delta.unsigned_abs() % selectable)
                % selectable
        } else {
            (selected + delta as usize) % selectable
        };
    }

    /// Accept the highlighted row: a category expands into its submenu, a clip is
    /// inserted into the buffer. Bound to Enter (and Tab).
    pub(super) fn accept_program_smart_clip(&mut self) {
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let Some(search) = popup.smart_clip.as_ref() else {
            return;
        };
        let rows = self.program_smart_clip_rows(popup);
        let Some(row) = Self::program_smart_clip_selected_row(&rows, search.selected).cloned() else {
            return;
        };
        match row {
            // The session category opens the richer session-picker dialog
            // (spec 0063) instead of the inline submenu — it adds archive
            // groups and query-driven auto-expand. The program's smart-clip
            // search stays live so confirming can replace the `@…` token.
            ProgramSmartClipRow::Category {
                group: ProgramSmartClipGroup::Session,
                ..
            } => {
                self.open_session_picker_for_program_clip();
            }
            ProgramSmartClipRow::Category { group, .. } => {
                self.enter_program_smart_clip_submenu(group);
            }
            ProgramSmartClipRow::Clip { candidate, .. } => {
                self.insert_program_smart_clip_candidate(&candidate);
            }
            ProgramSmartClipRow::Separator | ProgramSmartClipRow::Header(_) => {}
        }
    }

    /// Right-arrow: drill into the highlighted category's submenu (no-op when the
    /// highlighted row is a clip).
    pub(super) fn program_smart_clip_expand(&mut self) {
        let group = self.program_popup.as_ref().and_then(|popup| {
            let search = popup.smart_clip.as_ref()?;
            let rows = self.program_smart_clip_rows(popup);
            match Self::program_smart_clip_selected_row(&rows, search.selected)? {
                ProgramSmartClipRow::Category { group, .. } => Some(*group),
                _ => None,
            }
        });
        match group {
            // Mirror `accept`: the session category drills into the dialog.
            Some(ProgramSmartClipGroup::Session) => {
                self.open_session_picker_for_program_clip();
            }
            Some(group) => self.enter_program_smart_clip_submenu(group),
            None => {}
        }
    }

    /// Left-arrow: back out of a submenu to the root view, re-highlighting the
    /// category we came from so Right/Left are reversible. No-op at the root.
    pub(super) fn program_smart_clip_collapse(&mut self) {
        let selected = {
            let Some(popup) = self.program_popup.as_ref() else {
                return;
            };
            let Some(search) = popup.smart_clip.as_ref() else {
                return;
            };
            let ProgramSmartClipView::Submenu(group) = search.view else {
                return;
            };
            self.program_smart_clip_root_rows(popup)
                .iter()
                .filter(|r| r.is_selectable())
                .position(|r| {
                    matches!(r, ProgramSmartClipRow::Category { group: g, .. } if *g == group)
                })
                .unwrap_or(0)
        };
        if let Some(popup) = self.program_popup.as_mut() {
            if let Some(search) = popup.smart_clip.as_mut() {
                search.view = ProgramSmartClipView::Root;
                search.selected = selected;
            }
        }
    }

    fn enter_program_smart_clip_submenu(&mut self, group: ProgramSmartClipGroup) {
        if let Some(popup) = self.program_popup.as_mut() {
            if let Some(search) = popup.smart_clip.as_mut() {
                search.view = ProgramSmartClipView::Submenu(group);
                search.selected = 0;
            }
        }
    }

    pub(super) fn insert_program_smart_clip_candidate(
        &mut self,
        candidate: &ProgramSmartClipCandidate,
    ) {
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let Some(search) = popup.smart_clip.as_ref() else {
            return;
        };
        if popup.cursor < search.trigger_start {
            if let Some(popup) = self.program_popup.as_mut() {
                popup.smart_clip = None;
            }
            return;
        }
        let trigger_start = search.trigger_start;
        let clip = program_smart_clip_with_instance_id(&candidate.clip, &popup.buffer);
        let start_b = byte_pos(&popup.buffer, trigger_start);
        let end_b = byte_pos(&popup.buffer, popup.cursor);
        let new_cursor = trigger_start + clip.chars().count();
        self.push_program_undo_state();
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.buffer.replace_range(start_b..end_b, &clip);
        popup.cursor = new_cursor;
        popup.preferred_col = None;
        popup.selection = None;
        popup.smart_clip = None;
    }

    pub(super) fn update_program_smart_clip_after_cursor_move(popup: &mut ProgramPopup) {
        let Some(search) = popup.smart_clip.as_ref() else {
            return;
        };
        if program_smart_clip_query(popup, search.trigger_start).is_none() {
            popup.smart_clip = None;
        }
    }

    /// Delete one character from the live `@<typeahead>` token while *keeping*
    /// the smart-clip search alive (unlike [`Self::delete_program_back`], which
    /// always tears the search down). Used by the anchored `@`→session picker so
    /// backspacing narrows the query in place. Returns `true` while the `@`
    /// trigger survives; `false` once the `@` itself is removed (or there is no
    /// live search), signaling the caller to dismiss the picker.
    pub(super) fn program_smart_clip_backspace(&mut self) -> bool {
        let Some(popup) = self.program_popup.as_ref() else {
            return false;
        };
        let Some(search) = popup.smart_clip.as_ref() else {
            return false;
        };
        let trigger_start = search.trigger_start;
        if popup.cursor <= trigger_start {
            return false;
        }
        // An empty query means the cursor sits just past the `@`; backspacing
        // here deletes the `@` and ends the search.
        let drop_trigger = program_smart_clip_query(popup, trigger_start)
            .map(|q| q.is_empty())
            .unwrap_or(true);
        let del_start = popup.cursor - 1;
        let start_b = byte_pos(&popup.buffer, del_start);
        let end_b = byte_pos(&popup.buffer, popup.cursor);
        self.push_program_undo_state();
        let Some(popup) = self.program_popup.as_mut() else {
            return false;
        };
        popup.buffer.replace_range(start_b..end_b, "");
        popup.cursor = del_start;
        popup.preferred_col = None;
        popup.selection = None;
        if drop_trigger {
            popup.smart_clip = None;
            return false;
        }
        true
    }

    fn program_smart_clip_selectable_count(rows: &[ProgramSmartClipRow]) -> usize {
        rows.iter().filter(|r| r.is_selectable()).count()
    }

    /// The selectable row at logical position `selected` (clamped to range), or
    /// `None` when there is nothing selectable.
    fn program_smart_clip_selected_row(
        rows: &[ProgramSmartClipRow],
        selected: usize,
    ) -> Option<&ProgramSmartClipRow> {
        let selectable: Vec<&ProgramSmartClipRow> =
            rows.iter().filter(|r| r.is_selectable()).collect();
        if selectable.is_empty() {
            return None;
        }
        Some(selectable[selected.min(selectable.len() - 1)])
    }

    /// The rows on screen for the picker's current view — the single source of
    /// truth shared by navigation, acceptance, and rendering.
    pub(crate) fn program_smart_clip_rows(
        &self,
        popup: &ProgramPopup,
    ) -> Vec<ProgramSmartClipRow> {
        match popup.smart_clip.as_ref().map(|search| search.view) {
            Some(ProgramSmartClipView::Submenu(group)) => {
                self.program_smart_clip_submenu_rows(popup, group)
            }
            _ => self.program_smart_clip_root_rows(popup),
        }
    }

    /// Root view: up-to-5 most-relevant clips, a separator, then a category row
    /// per non-empty clip type.
    fn program_smart_clip_root_rows(&self, popup: &ProgramPopup) -> Vec<ProgramSmartClipRow> {
        let mut rows: Vec<ProgramSmartClipRow> = self
            .program_smart_clip_candidates(popup)
            .into_iter()
            .map(|candidate| ProgramSmartClipRow::Clip {
                candidate,
                dimmed: false,
            })
            .collect();

        let mut categories: Vec<ProgramSmartClipRow> = Vec::new();
        let session_count = self.program_smart_clip_session_candidates().len();
        if session_count > 0 {
            categories.push(ProgramSmartClipRow::Category {
                group: ProgramSmartClipGroup::Session,
                count: session_count,
            });
        }
        let harness_count = self.harnesses.len();
        if harness_count > 0 {
            categories.push(ProgramSmartClipRow::Category {
                group: ProgramSmartClipGroup::Harness,
                count: harness_count,
            });
        }
        if !rows.is_empty() && !categories.is_empty() {
            rows.push(ProgramSmartClipRow::Separator);
        }
        rows.extend(categories);
        rows
    }

    /// The top relevance section: up to 5 clips across all types, ranked by the
    /// type-ahead query (empty query → the 5 most-recent, sessions before
    /// harnesses).
    pub(crate) fn program_smart_clip_candidates(
        &self,
        popup: &ProgramPopup,
    ) -> Vec<ProgramSmartClipCandidate> {
        let query = Self::program_smart_clip_query_text(popup);
        let mut pool = self.program_smart_clip_session_candidates();
        pool.extend(self.program_smart_clip_harness_candidates());
        if query.is_empty() {
            pool.truncate(5);
            return pool;
        }
        let mut scored: Vec<(i32, usize, ProgramSmartClipCandidate)> = pool
            .into_iter()
            .enumerate()
            .filter_map(|(idx, candidate)| {
                let haystack = Self::program_smart_clip_haystack(&candidate);
                program_clip_match_score(&query, &candidate.label, &haystack)
                    .map(|score| (score, idx, candidate))
            })
            .collect();
        // Best score first; ties keep canonical (recency-then-harness) order.
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
        scored.into_iter().take(5).map(|(_, _, c)| c).collect()
    }

    fn program_smart_clip_submenu_rows(
        &self,
        popup: &ProgramPopup,
        group: ProgramSmartClipGroup,
    ) -> Vec<ProgramSmartClipRow> {
        let query = Self::program_smart_clip_query_text(popup);
        match group {
            ProgramSmartClipGroup::Session => self.program_smart_clip_session_submenu_rows(&query),
            ProgramSmartClipGroup::Harness => self.program_smart_clip_harness_submenu_rows(&query),
        }
    }

    /// Harness submenu: every harness, in config order. Non-matching items are
    /// dimmed (kept visible) rather than hidden.
    fn program_smart_clip_harness_submenu_rows(&self, query: &str) -> Vec<ProgramSmartClipRow> {
        self.program_smart_clip_harness_candidates()
            .into_iter()
            .map(|candidate| {
                let dimmed = Self::program_smart_clip_dimmed(query, &candidate);
                ProgramSmartClipRow::Clip { candidate, dimmed }
            })
            .collect()
    }

    /// Session submenu: mirrors the session-list view — ungrouped sessions first
    /// (position, then recency), then each project/group behind its header
    /// (position order within the group). Non-matching items are dimmed.
    fn program_smart_clip_session_submenu_rows(&self, query: &str) -> Vec<ProgramSmartClipRow> {
        let mut rows: Vec<ProgramSmartClipRow> = Vec::new();
        let push_clip = |rows: &mut Vec<ProgramSmartClipRow>, s: &SessionSummary| {
            let candidate = Self::session_smart_clip_candidate(s);
            let dimmed = Self::program_smart_clip_dimmed(query, &candidate);
            rows.push(ProgramSmartClipRow::Clip { candidate, dimmed });
        };

        let mut ungrouped: Vec<&SessionSummary> = self
            .sessions
            .iter()
            .filter(|s| s.group_id.is_none())
            .filter(|s| is_user_list_session(s))
            .collect();
        ungrouped.sort_by(|a, b| {
            a.position
                .cmp(&b.position)
                .then_with(|| b.created_at.cmp(&a.created_at))
        });
        for s in &ungrouped {
            push_clip(&mut rows, s);
        }

        let mut groups: Vec<&GroupSummary> = self.groups.iter().collect();
        groups.sort_by_key(|g| g.position);
        for g in groups {
            let mut members: Vec<&SessionSummary> = self
                .sessions
                .iter()
                .filter(|s| s.group_id.as_deref() == Some(g.id.as_str()))
                .filter(|s| is_user_list_session(s))
                .collect();
            if members.is_empty() {
                continue;
            }
            members.sort_by_key(|s| s.position);
            rows.push(ProgramSmartClipRow::Header(g.name.clone()));
            for s in &members {
                push_clip(&mut rows, s);
            }
        }
        rows
    }

    /// All user sessions as clip candidates, most-recently-created first — the
    /// canonical ordering for the top relevance section's empty-query fallback.
    fn program_smart_clip_session_candidates(&self) -> Vec<ProgramSmartClipCandidate> {
        let mut sessions: Vec<&SessionSummary> = self
            .sessions
            .iter()
            .filter(|s| is_user_list_session(s))
            .collect();
        sessions.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        sessions
            .iter()
            .map(|s| Self::session_smart_clip_candidate(s))
            .collect()
    }

    fn program_smart_clip_harness_candidates(&self) -> Vec<ProgramSmartClipCandidate> {
        self.harnesses
            .iter()
            .map(|harness| ProgramSmartClipCandidate {
                group: ProgramSmartClipGroup::Harness,
                clip: format!("@{{harness:{}}}", harness.name),
                label: harness.name.clone(),
                detail: if harness.available {
                    String::new()
                } else {
                    "unavailable".to_string()
                },
            })
            .collect()
    }

    pub(super) fn session_smart_clip_candidate(
        session: &SessionSummary,
    ) -> ProgramSmartClipCandidate {
        let title = session
            .title
            .clone()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| short_id(&session.id).to_string());
        ProgramSmartClipCandidate {
            group: ProgramSmartClipGroup::Session,
            clip: format!("@{{session:{}}}", session.id),
            label: title,
            detail: format!("{} · {}", session.harness, session.state.label()),
        }
    }

    /// Lowercased blob of a candidate's searchable text (label, detail, and the
    /// raw clip body, which carries the session id / harness name).
    fn program_smart_clip_haystack(candidate: &ProgramSmartClipCandidate) -> String {
        format!(
            "{} {} {}",
            candidate.label, candidate.detail, candidate.clip
        )
        .to_ascii_lowercase()
    }

    /// Whether a candidate fails the active type-ahead query (so it should render
    /// dimmed inside a submenu). An empty query dims nothing.
    fn program_smart_clip_dimmed(query: &str, candidate: &ProgramSmartClipCandidate) -> bool {
        if query.is_empty() {
            return false;
        }
        let haystack = Self::program_smart_clip_haystack(candidate);
        program_clip_match_score(query, &candidate.label, &haystack).is_none()
    }

    fn program_smart_clip_query_text(popup: &ProgramPopup) -> String {
        popup
            .smart_clip
            .as_ref()
            .and_then(|search| program_smart_clip_query(popup, search.trigger_start))
            .unwrap_or_default()
            .to_ascii_lowercase()
    }

    pub(super) fn update_program_selection_head(popup: &mut ProgramPopup) {
        if let Some(selection) = popup.selection.as_mut() {
            selection.head = popup.cursor;
        }
    }

    pub(super) fn program_selection_range(popup: &ProgramPopup) -> Option<(usize, usize)> {
        let selection = popup.selection.as_ref()?;
        let start = selection.anchor.min(selection.head);
        let end = selection.anchor.max(selection.head);
        (start != end).then_some((start, end))
    }

    pub(super) fn selected_program_text(popup: &ProgramPopup) -> Option<String> {
        let (start, end) = Self::program_selection_range(popup)?;
        let start_b = byte_pos(&popup.buffer, start);
        let end_b = byte_pos(&popup.buffer, end);
        Some(popup.buffer[start_b..end_b].to_string())
    }

    pub(super) fn selected_program_block_ids(popup: &ProgramPopup) -> Option<HashSet<String>> {
        let (selection_start, selection_end) = Self::program_selection_range(popup)?;
        let mut line_ranges = Vec::new();
        let mut line_start = 0usize;
        for line in popup.buffer.lines() {
            let line_end = line_start.saturating_add(line.chars().count());
            line_ranges.push((line_start, line_end));
            line_start = line_end.saturating_add(1);
        }

        let mut ids = HashSet::new();
        for block in program_blocks(&popup.buffer) {
            let Some((block_start, _)) = line_ranges.get(block.start_line).copied() else {
                continue;
            };
            let Some((_, block_end)) = line_ranges.get(block.end_line.saturating_sub(1)).copied()
            else {
                continue;
            };
            if selection_start < block_end && selection_end > block_start {
                ids.insert(block.id);
            }
        }
        Some(ids)
    }

    pub(super) fn push_program_undo_state(&mut self) {
        let popup = match self.program_popup.as_mut() {
            Some(popup) => popup,
            None => return,
        };
        popup.undo_stack.push(ProgramUndoState {
            buffer: popup.buffer.clone(),
            cursor: popup.cursor,
            preferred_col: popup.preferred_col,
            selection: popup.selection.clone(),
            smart_clip: popup.smart_clip.clone(),
            scroll_offset: popup.scroll_offset,
        });
        while popup.undo_stack.len() > PROGRAM_UNDO_STACK_LIMIT {
            popup.undo_stack.remove(0);
        }
    }

    pub(super) fn undo_program_edit(&mut self) {
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        let Some(state) = popup.undo_stack.pop() else {
            return;
        };
        popup.buffer = state.buffer;
        popup.cursor = state.cursor;
        popup.preferred_col = state.preferred_col;
        popup.selection = state.selection;
        popup.smart_clip = state.smart_clip;
        popup.scroll_offset = state.scroll_offset;
        Self::update_program_smart_clip_after_cursor_move(popup);
        self.follow_program_scroll();
    }

    pub(super) fn delete_program_selection(&mut self) -> Option<String> {
        let popup = self.program_popup.as_mut()?;
        let (start, end) = Self::program_selection_range(popup)?;
        let start_b = byte_pos(&popup.buffer, start);
        let end_b = byte_pos(&popup.buffer, end);
        let deleted = popup.buffer[start_b..end_b].to_string();
        popup.buffer.replace_range(start_b..end_b, "");
        popup.cursor = start;
        popup.preferred_col = None;
        popup.selection = None;
        popup.smart_clip = None;
        Some(deleted)
    }

    pub(super) fn copy_program_text(&mut self, text: &str, verb: &str) {
        self.program_clipboard = Some(text.to_string());
        match copy_to_clipboard(text) {
            Ok(outcome) => self.set_status(outcome.status(text.chars().count())),
            Err(e) => self.set_status(format!("{verb} failed: {e}")),
        }
    }

    pub(super) fn copy_program_selection(&mut self) {
        let Some(text) = self
            .program_popup
            .as_ref()
            .and_then(Self::selected_program_text)
        else {
            return;
        };
        if !text.is_empty() {
            self.copy_program_text(&text, "copy");
        }
    }

    /// Keyboard copy chords (M-w, Cmd-C, Ctrl-C). Copies the active selection
    /// to both the system clipboard and the internal `program_clipboard` exactly
    /// like the mouse-drag path, then clears the mark — emacs `kill-ring-save`.
    /// The buffer is never mutated; a no-op when there is no selection.
    pub(super) fn copy_program_selection_and_deactivate(&mut self) {
        self.copy_program_selection();
        if let Some(popup) = self.program_popup.as_mut() {
            popup.selection = None;
        }
    }

    pub(super) fn cut_program_selection(&mut self) {
        if !self
            .program_popup
            .as_ref()
            .and_then(Self::program_selection_range)
            .is_some()
        {
            return;
        }
        self.push_program_undo_state();
        let Some(text) = self.delete_program_selection() else {
            return;
        };
        if !text.is_empty() {
            self.copy_program_text(&text, "cut");
        }
    }

    pub(super) fn paste_program_clipboard(&mut self) {
        let text = self
            .program_clipboard
            .clone()
            .or_else(|| read_from_clipboard().ok());
        let Some(text) = text else {
            self.set_status("program paste failed: clipboard unavailable".to_string());
            return;
        };
        if !text.is_empty() {
            self.insert_program_text(&text);
        }
    }

    pub(super) fn cut_program_line(&mut self) {
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let start = byte_pos(&popup.buffer, popup.cursor);
        let line_end = program_line_end(&popup.buffer, popup.cursor);
        let mut end = byte_pos(&popup.buffer, line_end);
        if start == end && end < popup.buffer.len() {
            end += 1;
        }
        let cut = popup.buffer[start..end].to_string();
        if cut.is_empty() {
            return;
        }
        self.push_program_undo_state();
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.buffer.replace_range(start..end, "");
        popup.preferred_col = None;
        popup.selection = None;
        popup.smart_clip = None;
        if !cut.is_empty() {
            self.copy_program_text(&cut, "cut");
        }
    }

    /// Fill an empty program from a placeholder template button. Replaces the
    /// whole buffer (the placeholder only shows when the program is empty), records
    /// an undo state so the user can back out, and stamps the document's
    /// `template_id`. Persists on the normal save path (close / Run), exactly like
    /// typed edits.
    pub(super) fn apply_program_template(&mut self, template_id: String, markdown: String) {
        if self.program_popup.is_none() {
            return;
        }
        self.push_program_undo_state();
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.buffer = markdown;
        popup.cursor = popup.buffer.chars().count();
        popup.preferred_col = None;
        popup.selection = None;
        popup.smart_clip = None;
        if !template_id.is_empty() {
            popup.program.template_id = Some(template_id);
        }
        Self::update_program_smart_clip_after_cursor_move(popup);
        self.follow_program_scroll();
    }

    pub(super) fn insert_program_text(&mut self, text: &str) {
        if text.is_empty() {
            return;
        }
        if self.program_popup.is_none() {
            return;
        }
        let had_selection = self
            .program_popup
            .as_ref()
            .and_then(Self::program_selection_range)
            .is_some();
        self.push_program_undo_state();
        if had_selection {
            self.delete_program_selection();
        }
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        let trigger_start = if text == "@" {
            Some(popup.cursor)
        } else {
            None
        };
        let pos = byte_pos(&popup.buffer, popup.cursor);
        popup.buffer.insert_str(pos, text);
        popup.cursor += text.chars().count();
        popup.preferred_col = None;
        popup.selection = None;
        if let Some(trigger_start) = trigger_start {
            popup.smart_clip = Some(ProgramSmartClipSearch {
                trigger_start,
                selected: 0,
                view: ProgramSmartClipView::Root,
            });
        } else if popup.smart_clip.is_some() {
            Self::update_program_smart_clip_after_cursor_move(popup);
        }
    }

    pub(super) fn move_program_cursor(&mut self, delta: isize) {
        let cursor = {
            let Some(popup) = self.program_popup.as_ref() else {
                return;
            };
            self.program_horizontal_cursor_target(popup, delta)
        };
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.cursor = cursor;
        popup.preferred_col = None;
        Self::update_program_selection_head(popup);
        Self::update_program_smart_clip_after_cursor_move(popup);
    }

    pub(super) fn program_horizontal_cursor_target(&self, popup: &ProgramPopup, delta: isize) -> usize {
        let mut cursor = popup.cursor;
        let steps = delta.unsigned_abs();
        let direction = delta.signum();
        for _ in 0..steps {
            cursor = self.program_horizontal_cursor_step(popup, cursor, direction);
        }
        cursor
    }

    pub(super) fn program_horizontal_cursor_step(
        &self,
        popup: &ProgramPopup,
        cursor: usize,
        direction: isize,
    ) -> usize {
        let Some(inner) = self.layout.program_inner_area else {
            return if direction < 0 {
                program_cursor_left(&popup.buffer, cursor)
            } else {
                program_cursor_right(&popup.buffer, cursor)
            };
        };
        let width = inner.width as usize;
        if width == 0 {
            return cursor;
        }

        let old_pos = ui::program_cursor_visual_pos(Some(self), &popup.buffer, cursor, width);
        let mut next = cursor;
        loop {
            let candidate = if direction < 0 {
                program_cursor_left(&popup.buffer, next)
            } else {
                program_cursor_right(&popup.buffer, next)
            };
            if candidate == next {
                return candidate;
            }
            next = candidate;
            let new_pos = ui::program_cursor_visual_pos(Some(self), &popup.buffer, next, width);
            if new_pos != old_pos {
                return next;
            }
        }
    }

    pub(super) fn move_program_cursor_vertical(&mut self, delta: isize) {
        // Move by one *visual* (word-wrapped) row, like a normal editor: a
        // logical line that wraps spans several visual rows, and Up/Down step
        // through each. Work in the same wrapped-row space the body is laid out
        // and scrolled in, using the inner content width captured at the last
        // render — so nav, the cursor-follow scroll, and painting all agree. The
        // preferred column is tracked in *visual* columns so it survives crossing
        // wrapped rows onto shorter ones. Without a rendered viewport there is
        // nothing to navigate; the follow-scroll runs afterward in the caller.
        let Some(inner) = self.layout.program_inner_area else {
            return;
        };
        let width = inner.width as usize;
        if width == 0 {
            return;
        }
        let computed = {
            let app: &App = self;
            app.program_popup.as_ref().map(|popup| {
                let (row, col) =
                    ui::program_cursor_visual_pos(Some(app), &popup.buffer, popup.cursor, width);
                let target_col = popup.preferred_col.unwrap_or(col);
                let target_row = if delta < 0 {
                    row.saturating_sub(delta.unsigned_abs())
                } else {
                    row.saturating_add(delta as usize)
                };
                let cursor = ui::program_visual_to_cursor(
                    Some(app),
                    &popup.buffer,
                    target_row,
                    target_col,
                    width,
                );
                (
                    program_normalize_program_cursor(&popup.buffer, cursor),
                    target_col,
                )
            })
        };
        let Some((mut cursor, target_col)) = computed else {
            return;
        };
        // When the clip's rendered chip spans the target visual row, the inner
        // loop in program_visual_to_cursor finds no raw position on that row and
        // picks the '@' that opens the clip syntax instead — a position that is
        // still on the *previous* visual row, making the cursor appear stuck.
        // Advance past the entire clip so the cursor actually moves forward.
        if let Some(range) = self
            .program_popup
            .as_ref()
            .and_then(|p| program_smart_clip_range_at_or_containing(&p.buffer, cursor))
        {
            cursor = range.end;
        }
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.cursor = cursor;
        popup.preferred_col = Some(target_col);
        Self::update_program_selection_head(popup);
        Self::update_program_smart_clip_after_cursor_move(popup);
    }

    /// After a cursor move or edit, scroll the program popup so the caret stays
    /// inside the visible window. Uses the inner viewport captured during the
    /// last render, so it is a no-op until the program has rendered at least once
    /// (the cursor starts at the top, where offset 0 is already correct).
    pub(super) fn follow_program_scroll(&mut self) {
        let Some(inner) = self.layout.program_inner_area else {
            return;
        };
        let width = inner.width as usize;
        let viewport = inner.height as usize;
        if width == 0 || viewport == 0 {
            return;
        }
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let cursor_row =
            crate::ui::program_cursor_visual_row(Some(self), &popup.buffer, popup.cursor, width);
        let total_rows = crate::ui::program_total_visual_rows(Some(self), &popup.buffer, width);
        let max_scroll = total_rows.saturating_sub(viewport);
        let next = crate::ui::program_follow_scroll(popup.scroll_offset, cursor_row, viewport)
            .min(max_scroll);
        if let Some(popup) = self.program_popup.as_mut() {
            popup.scroll_offset = next;
        }
    }

    /// Center the cursor row in the program popup viewport (emacs C-l semantics).
    /// Places the visual cursor row near the middle of the current viewport
    /// (clamped at the top or bottom when near the edges of the buffer).
    pub(super) fn center_program_cursor(&mut self) {
        let Some(inner) = self.layout.program_inner_area else {
            return;
        };
        let width = inner.width as usize;
        let viewport = inner.height as usize;
        if width == 0 || viewport == 0 {
            return;
        }
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let cursor_row =
            crate::ui::program_cursor_visual_row(Some(self), &popup.buffer, popup.cursor, width);
        let total_rows = crate::ui::program_total_visual_rows(Some(self), &popup.buffer, width);
        let max_scroll = total_rows.saturating_sub(viewport);
        let half = viewport / 2;
        // Aim for the cursor to land roughly in the center of the window.
        let desired = cursor_row.saturating_sub(half);
        let next = desired.min(max_scroll);
        if let Some(popup) = self.program_popup.as_mut() {
            popup.scroll_offset = next;
        }
    }

    /// Scroll the program popup by `delta` wrapped rows (negative scrolls up)
    /// without moving the caret — the mouse-wheel path. Bounds against the
    /// last-rendered viewport so it never scrolls past the end of the content.
    pub(super) fn scroll_program_popup(&mut self, delta: isize) {
        let Some(inner) = self.layout.program_inner_area else {
            return;
        };
        let width = inner.width as usize;
        let viewport = inner.height as usize;
        if width == 0 || viewport == 0 {
            return;
        }
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let total_rows = crate::ui::program_total_visual_rows(Some(self), &popup.buffer, width);
        let max_scroll = total_rows.saturating_sub(viewport);
        let current = popup.scroll_offset;
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current.saturating_add(delta as usize).min(max_scroll)
        };
        if let Some(popup) = self.program_popup.as_mut() {
            popup.scroll_offset = next;
        }
    }

    pub(super) fn delete_program_back(&mut self) {
        let has_selection = self
            .program_popup
            .as_ref()
            .and_then(Self::program_selection_range)
            .is_some();
        if has_selection {
            self.push_program_undo_state();
            let _ = self.delete_program_selection();
            return;
        }
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        if popup.cursor == 0 {
            return;
        }
        let (char_start, char_end) =
            if let Some(range) = program_smart_clip_range_before_or_containing(
                &popup.buffer,
                popup.cursor,
            ) {
                (range.start, range.end)
            } else {
                (popup.cursor - 1, popup.cursor)
            };
        let (start, end) = {
            let start = byte_pos(&popup.buffer, char_start);
            let end = byte_pos(&popup.buffer, char_end);
            (start, end)
        };
        self.push_program_undo_state();
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.buffer.replace_range(start..end, "");
        popup.cursor = char_start;
        popup.preferred_col = None;
        popup.selection = None;
        popup.smart_clip = None;
        Self::update_program_smart_clip_after_cursor_move(popup);
    }

    pub(super) fn delete_program_forward(&mut self) {
        let has_selection = self
            .program_popup
            .as_ref()
            .and_then(Self::program_selection_range)
            .is_some();
        if has_selection {
            self.push_program_undo_state();
            let _ = self.delete_program_selection();
            return;
        }
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        if popup.cursor >= popup.buffer.chars().count() {
            return;
        }
        let (char_start, char_end) =
            if let Some(range) = program_smart_clip_range_at_or_containing(
                &popup.buffer,
                popup.cursor,
            ) {
                (range.start, range.end)
            } else {
                (popup.cursor, popup.cursor + 1)
            };
        let (start, end) = {
            let start = byte_pos(&popup.buffer, char_start);
            let end = byte_pos(&popup.buffer, char_end);
            (start, end)
        };
        self.push_program_undo_state();
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.buffer.replace_range(start..end, "");
        popup.cursor = char_start;
        popup.preferred_col = None;
        popup.selection = None;
        popup.smart_clip = None;
        Self::update_program_smart_clip_after_cursor_move(popup);
    }

}
