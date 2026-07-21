use super::*;

impl App {
    pub(super) fn place_program_cursor(
        &mut self,
        modal: ratatui::layout::Rect,
        col: u16,
        row: u16,
    ) {
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
        if self.program_popup.is_none() {
            return false;
        }
        if self.resizing_program_popup.is_some() {
            match ev.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.resize_program_popup_to_row(ev.row);
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.resize_program_popup_to_row(ev.row);
                    self.resizing_program_popup = None;
                    return true;
                }
                _ => return true,
            }
        }
        // An in-flight inline-image resize (spec 0099) owns the mouse until
        // release, same as the popup-resize gesture above. Handled here —
        // not in the generic mouse path — because this handler runs first
        // and consumes drags over the popup body.
        if let Some((key, top)) = self.resizing_program_attachment {
            match ev.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    let rows = (ev.row.saturating_sub(top).saturating_add(1)).clamp(
                        crate::ui::PROGRAM_ATTACHMENT_MIN_ROWS,
                        crate::ui::PROGRAM_ATTACHMENT_MAX_ROWS,
                    );
                    if let Some(popup) = self.program_popup.as_mut() {
                        if let Some(entry) = popup.expanded_attachments.get_mut(&key) {
                            entry.1 = rows;
                        }
                    }
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.resizing_program_attachment = None;
                    self.persist_program_expanded();
                    return true;
                }
                _ => return true,
            }
        }
        // An in-flight pinned-card drag (spec 0090) owns the mouse until the
        // button releases, wherever the pointer wanders — same rule as every
        // other construct drag gesture.
        if let Some(drag) = self.pinned_card_drag {
            match ev.kind {
                MouseEventKind::Drag(MouseButton::Left) => {
                    self.apply_pinned_card_drag(drag, ev);
                    return true;
                }
                MouseEventKind::Up(MouseButton::Left) => {
                    self.apply_pinned_card_drag(drag, ev);
                    self.finish_pinned_card_drag(drag).await;
                    self.pinned_card_drag = None;
                    return true;
                }
                _ => return true,
            }
        }
        let Some(modal) = self.layout.modal_area else {
            return false;
        };
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
        // A pinned clip card (spec 0090) owns the mouse over its own bounds
        // and dismisses on clicks elsewhere:
        // - A left click on the card is consumed here (mouse never forwards
        //   into the pinned session — keyboard-only scope — but the card
        //   reclaims keyboard focus, so a List-focused click resumes typing
        //   into the pin).
        // - Wheel over the card pans its cropped viewport instead of
        //   scrolling the doc: vertical steps back from the live tail the
        //   card is anchored to; ScrollLeft/ScrollRight — or Shift+wheel for
        //   terminals that never synthesize horizontal events — pans across
        //   the screen width. Card-local: nothing forwards to the session.
        // - A left click landing neither on the card nor on a session clip
        //   (the pin's own toggle/switch affordance, handled below) — in the
        //   program body, on its chrome, or outside the modal entirely —
        //   unpins, then proceeds with the effect it always had.
        if self
            .program_popup
            .as_ref()
            .is_some_and(|popup| popup.pinned_clip.is_some())
        {
            let on_card = self
                .layout
                .program_pinned_card_rect
                .is_some_and(|card| Self::rect_contains(card, ev.column, ev.row));
            match ev.kind {
                MouseEventKind::Down(MouseButton::Left) if on_card => {
                    // Border grabs start a drag gesture (spec 0090): the
                    // right/bottom border resizes the card, the top border
                    // (title bar) moves it. Interior clicks just reclaim
                    // keyboard focus for the pin.
                    let card = self
                        .layout
                        .program_pinned_card_rect
                        .expect("on_card implies a painted card rect");
                    let on_right = ev.column == card.x + card.width.saturating_sub(1);
                    let on_bottom = ev.row == card.y + card.height.saturating_sub(1);
                    let on_top = ev.row == card.y;
                    if on_right || on_bottom {
                        if let Some(popup) = self.program_popup.as_ref() {
                            self.pinned_card_drag = Some(crate::app::PinnedCardDrag::Resize {
                                start_cols: popup.pinned_card_cols,
                                start_rows: popup.pinned_card_rows,
                                from: (ev.column, ev.row),
                            });
                        }
                    } else if on_top {
                        self.pinned_card_drag = Some(crate::app::PinnedCardDrag::Move {
                            grab: (
                                ev.column.saturating_sub(card.x),
                                ev.row.saturating_sub(card.y),
                            ),
                        });
                    }
                    self.focus = PaneFocus::View;
                    self.set_program_terminal_focus(false);
                    return true;
                }
                MouseEventKind::Down(MouseButton::Left) => {
                    if self.program_clip_session_at(ev.column, ev.row).is_none() {
                        self.set_program_pinned_clip(None).await;
                    }
                }
                MouseEventKind::ScrollUp
                | MouseEventKind::ScrollDown
                | MouseEventKind::ScrollLeft
                | MouseEventKind::ScrollRight
                    if on_card =>
                {
                    self.pan_pinned_clip_card(ev);
                    return true;
                }
                _ => {}
            }
        }
        let contains = ev.column >= modal.x
            && ev.column < modal.x.saturating_add(modal.width)
            && ev.row >= modal.y
            && ev.row < modal.y.saturating_add(modal.height);
        if !contains {
            if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left))
                && self
                    .layout
                    .program_base_area
                    .is_some_and(|base| Self::rect_contains(base, ev.column, ev.row))
            {
                self.focus = PaneFocus::View;
                self.set_program_terminal_focus(true);
            }
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
            self.set_program_terminal_focus(false);
        }
        if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left))
            && self
                .layout
                .program_resize_hit
                .is_some_and(|hit| Self::rect_contains(hit, ev.column, ev.row))
        {
            self.resizing_program_popup = Some(());
            return true;
        }
        let title_run_hit = self.layout.program_title_run_hit;
        let title_toggle_hit = self.layout.program_title_toggle_hit;
        let title_close_hit = self.layout.program_title_close_hit;
        let title_name_hit = self.layout.program_title_name_hit;
        let selection_run_hit = self.layout.program_selection_run_hit;
        let hit_title_toggle = title_toggle_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        let hit_title_run = title_run_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        let hit_title_close = title_close_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        let hit_title_name = title_name_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        let hit_selection_run = selection_run_hit
            .is_some_and(|(xs, xe, y)| ev.row == y && ev.column >= xs && ev.column < xe);
        // Program selection verb buttons (spec 0089): a small vertical list
        // of rows below the comment/Run row, each its own hit-rect.
        let hit_selection_verb = self
            .layout
            .program_selection_verb_hits
            .iter()
            .find(|(xs, xe, y, _)| ev.row == *y && ev.column >= *xs && ev.column < *xe)
            .map(|(_, _, _, name)| name.clone());
        if hit_title_toggle
            || hit_title_run
            || hit_title_close
            || hit_title_name
            || hit_selection_run
            || hit_selection_verb.is_some()
        {
            if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
                if let Some(verb) = hit_selection_verb.clone() {
                    self.execute_program_selected_verb(
                        verb,
                        ev.modifiers.contains(KeyModifiers::SHIFT),
                    )
                    .await;
                } else if hit_title_toggle {
                    self.close_program_popup().await;
                } else if hit_title_close {
                    if let Some(session_id) = self
                        .program_popup
                        .as_ref()
                        .map(|popup| popup.program.session_id.clone())
                    {
                        self.open_session_title_menu(session_id, modal);
                    }
                } else if hit_title_name {
                    if let Some(session_id) = self
                        .program_popup
                        .as_ref()
                        .map(|popup| popup.program.session_id.clone())
                    {
                        // Cursor lands on the clicked char; a click inside the
                        // field already being edited just repositions it.
                        let display_col = title_name_hit
                            .map(|(xs, _, _)| ev.column.saturating_sub(xs) as usize)
                            .unwrap_or(0);
                        let editing_this_field =
                            self.session_title_rename.as_ref().is_some_and(|r| {
                                r.session_id == session_id && r.origin == TitleRenameOrigin::Program
                            });
                        if editing_this_field {
                            self.session_title_rename_click_cursor(
                                self.layout.program_title_name_window_start,
                                display_col,
                            );
                        } else {
                            self.start_session_title_rename(
                                session_id,
                                TitleRenameOrigin::Program,
                                Some(display_col),
                            );
                        }
                    }
                } else if hit_selection_run {
                    let comment = self
                        .program_popup
                        .as_ref()
                        .and_then(|popup| popup.selection_menu.as_ref())
                        .map(|menu| menu.comment.clone());
                    self.execute_program_selected_text(
                        comment,
                        ev.modifiers.contains(KeyModifiers::SHIFT),
                    )
                    .await;
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
                    self.execute_program_popup(selection, selected_block_ids, None)
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
        // Clicking an action link in the program body dispatches it to the
        // program's owning session as user intent — the same
        // `OBSERVATION: ui.action …` path widget action links use, minus the
        // panel id (the program is not a widget panel). Keyboard shortcuts
        // are deliberately NOT wired here: the program is a typing surface.
        if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(hit) = self
                .layout
                .program_action_link_hits
                .iter()
                .find(|hit| hit.contains(ev.column, ev.row))
                .cloned()
            {
                self.dispatch_dynamic_ui_action(hit.session_id, None, hit.action)
                    .await;
                return true;
            }
        }
        // Clicking an attachment image chip toggles its inline preview
        // (spec 0099); clicking inside an expanded preview collapses it,
        // except on the preview's bottom edge, which starts a drag-resize
        // (tracked in `resizing_program_attachment`, updated in `on_mouse`).
        // File chips have no toggle — the click falls through to normal
        // cursor placement; hover already carries their info.
        if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            let image_chip = self
                .layout
                .program_attachment_hits
                .iter()
                .find(|h| h.is_image && h.contains(ev.column, ev.row))
                .cloned();
            if let Some(hit) = image_chip {
                if let Some(popup) = self.program_popup.as_mut() {
                    if popup.expanded_attachments.remove(&hit.key).is_none() {
                        popup
                            .expanded_attachments
                            .insert(
                                hit.key,
                                (hit.path.clone(), crate::ui::PROGRAM_ATTACHMENT_DEFAULT_ROWS),
                            );
                    }
                }
                self.persist_program_expanded();
                return true;
            }
            // Resize zone first: it overlaps the image's bottom row, and a
            // grab must win over collapse there.
            if let Some((_, key, top)) = self
                .layout
                .program_attachment_resize_zones
                .iter()
                .find(|(r, _, _)| Self::rect_contains(*r, ev.column, ev.row))
                .cloned()
            {
                self.resizing_program_attachment = Some((key, top));
                return true;
            }
            let image_rect = self
                .layout
                .program_attachment_image_rects
                .iter()
                .find(|(r, _, _)| Self::rect_contains(*r, ev.column, ev.row))
                .cloned();
            if let Some((_, key, _path)) = image_rect {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.expanded_attachments.remove(&key);
                }
                self.persist_program_expanded();
                return true;
            }
        }
        // Clicking a session smart-clip: a double-click (within
        // PROGRAM_CLIP_DOUBLE_CLICK_MS, same clip) navigates to the full
        // session view, just like every click did before pinning existed. A
        // single click instead toggles the clip's inline terminal pinned
        // open — clicking the same pinned clip again unpins it; clicking a
        // different clip switches the pin to it. Pinning lets the user
        // answer a verb session's questions (e.g. `interview`) without
        // leaving the Program doc.
        if matches!(ev.kind, MouseEventKind::Down(MouseButton::Left)) {
            if let Some(session_id) = self.program_clip_session_at(ev.column, ev.row) {
                let now = Instant::now();
                let is_double_click =
                    self.last_program_clip_click
                        .as_ref()
                        .is_some_and(|(id, at)| {
                            id == &session_id
                                && now.saturating_duration_since(*at)
                                    < Duration::from_millis(PROGRAM_CLIP_DOUBLE_CLICK_MS)
                        });
                if is_double_click {
                    self.last_program_clip_click = None;
                    self.set_program_pinned_clip(None).await;
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
                } else {
                    self.last_program_clip_click = Some((session_id.clone(), now));
                    let pinned = self.program_popup.as_ref().and_then(|popup| {
                        if popup.pinned_clip.as_deref() == Some(session_id.as_str()) {
                            None
                        } else {
                            Some(session_id)
                        }
                    });
                    self.set_program_pinned_clip(pinned).await;
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
                // Shift-click extends the existing mark/cursor to the clicked
                // point instead of starting a fresh selection there, mirroring
                // Shift+Arrow's "extend, don't restart" behavior. Marked
                // `dragged` so mouse-up commits it like a drag-selection
                // (stays highlighted and copies) rather than being cleared
                // like a plain click.
                if ev.modifiers.contains(KeyModifiers::SHIFT) {
                    let anchor = popup
                        .selection
                        .as_ref()
                        .map(|selection| selection.anchor)
                        .unwrap_or(popup.cursor);
                    popup.cursor = cursor;
                    popup.preferred_col = None;
                    popup.selection = Some(ProgramSelection {
                        anchor,
                        head: cursor,
                        dragged: true,
                    });
                    popup.selection_menu = Some(ProgramSelectionMenu::default());
                    popup.smart_clip = None;
                    return true;
                }
                popup.cursor = cursor;
                popup.preferred_col = None;
                popup.selection = Some(ProgramSelection {
                    anchor: cursor,
                    head: cursor,
                    dragged: false,
                });
                popup.selection_menu = Some(ProgramSelectionMenu::default());
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
                if ev.modifiers.contains(crossterm::event::KeyModifiers::CONTROL) {
                    if let Some(hit) = crate::app::url_hit_in_frame(
                        &self.frame_text,
                        ev.column,
                        ev.row,
                        modal,
                    ) {
                        match crate::app::open_url(&hit.url) {
                            Ok(()) => self.set_status(format!("opened {}", hit.url)),
                            Err(e) => self.set_status(format!("open URL failed: {e}")),
                        }
                    }
                }
                let should_copy = self
                    .program_popup
                    .as_ref()
                    .and_then(|popup| popup.selection.as_ref())
                    .is_some_and(|selection| selection.dragged);
                if should_copy {
                    self.copy_program_selection();
                } else if let Some(popup) = self.program_popup.as_mut() {
                    popup.selection = None;
                    popup.selection_menu = None;
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

    fn resize_program_popup_to_row(&mut self, row: u16) {
        let Some(base) = self.layout.program_base_area else {
            return;
        };
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        let max_h = base.height.max(1);
        let raw_h = row.saturating_sub(base.y).saturating_add(1).clamp(1, max_h);
        let min_h = max_h.min(8).max(1);
        let height = raw_h.clamp(min_h, max_h);
        let percent = ((height as u32 * 100) + (max_h as u32 / 2)) / max_h as u32;
        popup.cover_percent = (percent as u16).clamp(
            crate::app::PROGRAM_COVER_PERCENT_MIN,
            crate::app::PROGRAM_COVER_PERCENT_MAX,
        );
        self.set_program_terminal_focus(false);
    }

    pub(super) async fn handle_program_key(&mut self, key: KeyEvent) {
        if self.program_popup.as_ref().is_some_and(|p| p.closing) {
            return;
        }
        let before = self
            .program_popup
            .as_ref()
            .map(|popup| popup.buffer.clone());
        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let ctrl_char = Self::normalized_ctrl_char(key);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        let shift = key.modifiers.contains(KeyModifiers::SHIFT);
        if self.handle_program_selection_menu_key(key).await {
            return;
        }
        if shift
            && matches!(
                key.code,
                KeyCode::Left
                    | KeyCode::Right
                    | KeyCode::Up
                    | KeyCode::Down
                    | KeyCode::Home
                    | KeyCode::End
            )
            && self.program_popup.as_ref().is_some_and(|popup| {
                popup.selection.is_none() && popup.smart_clip.is_none() && popup.search.is_none()
            })
        {
            self.begin_program_selection();
        }
        if self.program_search_active() {
            match key.code {
                KeyCode::Esc => self.cancel_program_search(),
                _ if ctrl_char == Some('g') => self.cancel_program_search(),
                _ if ctrl_char == Some('s') => self.move_program_search_match(1),
                _ if ctrl_char == Some('r') => self.move_program_search_match(-1),
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
            KeyCode::Esc => {
                // Esc is Construct's browser-safe universal cancel key. It
                // dismisses transient Program UI and clears an active mark,
                // but deliberately never hides or mutates the Program itself.
                self.cancel_program_smart_clip();
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.selection = None;
                    popup.selection_menu = None;
                }
                self.set_status("program selection canceled".to_string());
            }
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
            KeyCode::Right if self.program_smart_clip_active() => self.program_smart_clip_expand(),
            KeyCode::Left if self.program_smart_clip_active() => self.program_smart_clip_collapse(),
            _ if matches!(ctrl_char, Some(' ' | '@' | '\0')) => self.begin_program_selection(),
            _ if ctrl_char == Some('g') => {
                // Keep the traditional Emacs C-g as an alias for Esc.
                self.cancel_program_smart_clip();
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.selection = None;
                    popup.selection_menu = None;
                }
                self.set_status("program selection canceled".to_string());
            }
            _ if ctrl_char == Some('a') => {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.cursor = program_line_start(&popup.buffer, popup.cursor);
                    popup.preferred_col = None;
                    Self::update_program_selection_head(popup);
                    Self::update_program_smart_clip_after_cursor_move(popup);
                }
            }
            _ if ctrl_char == Some('s') => self.begin_program_search(),
            _ if ctrl_char == Some('e') => {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.cursor = program_line_end(&popup.buffer, popup.cursor);
                    popup.preferred_col = None;
                    Self::update_program_selection_head(popup);
                    Self::update_program_smart_clip_after_cursor_move(popup);
                }
            }
            _ if ctrl_char == Some('b') => self.move_program_cursor(-1),
            _ if ctrl_char == Some('f') => self.move_program_cursor(1),
            _ if ctrl_char == Some('p') && self.program_smart_clip_active() => {
                self.move_program_smart_clip_selection(-1)
            }
            _ if ctrl_char == Some('n') && self.program_smart_clip_active() => {
                self.move_program_smart_clip_selection(1)
            }
            _ if ctrl_char == Some('p') => self.move_program_cursor_vertical(-1),
            _ if ctrl_char == Some('n') => self.move_program_cursor_vertical(1),
            _ if ctrl_char == Some('v') => self.paste_program_clipboard(),
            _ if ctrl_char == Some('y') => self.paste_program_clipboard(),
            _ if ctrl_char == Some('w') => self.cut_program_selection(),
            // M-w is emacs kill-ring-save: copy the selection, never delete.
            KeyCode::Char('w') if alt => self.copy_program_selection_and_deactivate(),
            KeyCode::Char('/') if ctrl => self.undo_program_edit(),
            // Cmd-C / Ctrl-C also copy, but only when a selection exists so we
            // don't disturb existing behavior otherwise (plain C-c stays a
            // no-op here; the C-x C-c quit chord is consumed earlier in
            // handle_program_global_key, and bare Cmd-C still self-inserts 'c').
            KeyCode::Char('c')
                if (ctrl_char == Some('c') || super_mod)
                    && self
                        .program_popup
                        .as_ref()
                        .and_then(Self::program_selection_range)
                        .is_some() =>
            {
                self.copy_program_selection_and_deactivate()
            }
            _ if ctrl_char == Some('d') => self.delete_program_forward(),
            _ if ctrl_char == Some('h') => self.delete_program_back(),
            _ if ctrl_char == Some('k') => self.cut_program_line(),
            KeyCode::Enter => self.insert_program_newline(),
            KeyCode::Backspace => self.delete_program_back(),
            KeyCode::Delete => self.delete_program_forward(),
            KeyCode::Left => self.move_program_cursor(-1),
            KeyCode::Right => self.move_program_cursor(1),
            KeyCode::Up => self.move_program_cursor_vertical(-1),
            KeyCode::Down => self.move_program_cursor_vertical(1),
            _ if ctrl_char == Some('l') => {
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
            KeyCode::Char(c) if ctrl_char.is_none() && !ctrl && !alt => {
                self.insert_program_text(&c.to_string())
            }
            _ => {}
        }
        // Any cursor move or edit above may have pushed the caret out of the
        // visible window; re-anchor the scroll so it stays on-screen.
        self.follow_program_scroll();
        self.publish_program_cursor().await;
        if let Some(before) = before {
            self.flush_program_live_edit(before).await;
        }
    }

    /// Change the pinned clip through the size-ownership protocol (spec
    /// 0090). A session pinned while visible nowhere else on screen (no main
    /// window, not the orchestrator, not in the pin strip) gets its PTY
    /// resized to the card's content dims, so the harness reflows to fit —
    /// full fidelity, no crop — and is resized back to the standard pane
    /// size the moment the pin releases (unpin, switch, dismiss, popup
    /// close). A session that IS visible elsewhere keeps its size and the
    /// card stays a crop with wheel pan: at most one render site ever owns a
    /// session's size (spec 0025 discipline).
    pub(super) async fn set_program_pinned_clip(&mut self, pinned: Option<String>) {
        let released = self.program_popup.as_ref().and_then(|popup| {
            popup
                .pinned_terminal_size
                .and_then(|_| popup.pinned_clip.clone())
        });
        if let Some(old_id) = released {
            let (cols, rows) = self.terminal_pane_size;
            let _ = self.client.pty_resize(&old_id, cols, rows).await;
        }
        let card_dims = self
            .program_popup
            .as_ref()
            .map(|popup| (popup.pinned_card_cols, popup.pinned_card_rows))
            .unwrap_or((
                crate::app::PROGRAM_PINNED_CARD_DEFAULT_COLS,
                crate::app::PROGRAM_CLIP_HOVER_PREVIEW_ROWS,
            ));
        let owned_size = pinned.as_deref().and_then(|id| {
            if self.session_visible_on_screen(id) {
                return None;
            }
            let modal = self.layout.modal_area?;
            let cols = card_dims.0.clamp(1, modal.width.saturating_sub(2).max(1));
            Some((cols, card_dims.1.max(1)))
        });
        if let (Some(id), Some((cols, rows))) = (pinned.as_deref(), owned_size) {
            let _ = self.client.pty_resize(id, cols, rows).await;
        }
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        popup.set_pinned_clip(pinned);
        popup.pinned_terminal_size = owned_size;
    }

    /// Apply one pointer sample of an in-flight pinned-card drag (spec
    /// 0090): resize computes new content dims from the drag-start dims plus
    /// the pointer delta (stable regardless of how the card's anchor
    /// placement shifts as it grows); move places the card's top-left so the
    /// grabbed cell stays under the pointer. Both clamp inside the Program
    /// modal.
    fn apply_pinned_card_drag(&mut self, drag: crate::app::PinnedCardDrag, ev: &MouseEvent) {
        let Some(modal) = self.layout.modal_area else {
            return;
        };
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        match drag {
            crate::app::PinnedCardDrag::Resize {
                start_cols,
                start_rows,
                from,
            } => {
                let d_cols = i32::from(ev.column) - i32::from(from.0);
                let d_rows = i32::from(ev.row) - i32::from(from.1);
                let max_cols = i32::from(modal.width.saturating_sub(4).max(1));
                let max_rows = i32::from(modal.height.saturating_sub(4).max(1));
                popup.pinned_card_cols = (i32::from(start_cols) + d_cols)
                    .clamp(i32::from(crate::app::PROGRAM_PINNED_CARD_MIN_COLS), max_cols)
                    as u16;
                popup.pinned_card_rows = (i32::from(start_rows) + d_rows)
                    .clamp(i32::from(crate::app::PROGRAM_PINNED_CARD_MIN_ROWS), max_rows)
                    as u16;
            }
            crate::app::PinnedCardDrag::Move { grab } => {
                let x = ev
                    .column
                    .saturating_sub(grab.0)
                    .clamp(modal.x, modal.x.saturating_add(modal.width.saturating_sub(1)));
                let y = ev
                    .row
                    .saturating_sub(grab.1)
                    .clamp(modal.y, modal.y.saturating_add(modal.height.saturating_sub(1)));
                popup.pinned_card_pos = Some((x, y));
            }
        }
    }

    /// Finalize a pinned-card drag (spec 0090). Only a resize on a
    /// size-owning pin has work left to do: the session's PTY follows the
    /// card's final dims in one resize, on release rather than per drag
    /// sample, so the harness reflows once instead of on every pointer step.
    async fn finish_pinned_card_drag(&mut self, drag: crate::app::PinnedCardDrag) {
        if !matches!(drag, crate::app::PinnedCardDrag::Resize { .. }) {
            return;
        }
        let target = self.program_popup.as_ref().and_then(|popup| {
            popup.pinned_terminal_size?;
            let id = popup.pinned_clip.clone()?;
            Some((id, popup.pinned_card_cols, popup.pinned_card_rows))
        });
        let Some((id, cols, rows)) = target else {
            return;
        };
        let _ = self.client.pty_resize(&id, cols, rows).await;
        if let Some(popup) = self.program_popup.as_mut() {
            popup.pinned_terminal_size = Some((cols, rows));
        }
    }

    /// Shift the pinned card's crop pan by `(d_cols, d_rows)` — positive
    /// cols pan right, positive rows pan back from the tail. Offsets clamp
    /// loosely to the session's cached screen here; the renderer clamps
    /// precisely against the visible content every frame, so a pan can never
    /// scroll past the content into blank space for long.
    fn pan_pinned_card_by(&mut self, d_cols: i32, d_rows: i32) {
        fn shift(value: u16, delta: i32, max: u16) -> u16 {
            (i32::from(value) + delta).clamp(0, i32::from(max)) as u16
        }
        let dims = self
            .program_popup
            .as_ref()
            .and_then(|popup| popup.pinned_clip.as_deref())
            .and_then(|id| self.histories.get(id))
            .and_then(|history| history.cached_dims());
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        let (max_cols, max_rows) = dims.unwrap_or((u16::MAX, u16::MAX));
        popup.pinned_scroll_cols = shift(popup.pinned_scroll_cols, d_cols, max_cols);
        popup.pinned_scroll_rows = shift(popup.pinned_scroll_rows, d_rows, max_rows);
    }

    /// Pan the pinned clip card's cropped viewport by one wheel step (spec
    /// 0090). Vertical steps back/forward through the tail-anchored content;
    /// ScrollLeft/ScrollRight — or Shift/Alt+vertical-wheel, since not every
    /// terminal synthesizes horizontal wheel events (and some never report
    /// Shift-modified wheels at all, reserving Shift for native selection;
    /// Alt tends to pass through) — pans across the screen width.
    /// Shift+arrows (see `handle_pinned_clip_key`) are the guaranteed
    /// keyboard fallback when a terminal delivers none of these.
    ///
    /// Horizontal signs are the OPPOSITE of the raw event names: terminals
    /// normalize vertical wheel events for scroll intent ("ScrollUp" always
    /// means "look back", whatever the natural-scroll setting), but they do
    /// not apply the same normalization horizontally — observed in practice,
    /// the raw horizontal deltas arrive gesture-encoded, so the naive
    /// mapping panned the wrong way. Keyboard pan is unambiguous and stays
    /// direction-literal.
    fn pan_pinned_clip_card(&mut self, ev: &MouseEvent) {
        let v_step = PROGRAM_WHEEL_SCROLL_ROWS as i32;
        let h_step = PROGRAM_PINNED_PAN_COLS_STEP as i32;
        let horizontal = ev.modifiers.contains(KeyModifiers::SHIFT)
            || ev.modifiers.contains(KeyModifiers::ALT);
        match (ev.kind, horizontal) {
            (MouseEventKind::ScrollUp, true) => self.pan_pinned_card_by(h_step, 0),
            (MouseEventKind::ScrollDown, true) => self.pan_pinned_card_by(-h_step, 0),
            (MouseEventKind::ScrollUp, false) => self.pan_pinned_card_by(0, v_step),
            (MouseEventKind::ScrollDown, false) => self.pan_pinned_card_by(0, -v_step),
            (MouseEventKind::ScrollLeft, _) => self.pan_pinned_card_by(h_step, 0),
            (MouseEventKind::ScrollRight, _) => self.pan_pinned_card_by(-h_step, 0),
            _ => {}
        }
    }

    /// Route a keypress to the Program popup's pinned clip, if one is
    /// pinned. Two deliberate carve-outs never reach the session: the
    /// global `C-x` chord prefix (falls through to the keymap, same escape
    /// hatch as a captured session PTY — `C-x C-x` forwards a literal C-x)
    /// and `Shift+arrows` (crop pan, spec 0090); every other key, `Esc`
    /// included (sessions need Esc, e.g. to interrupt a harness mid-turn),
    /// encodes to raw PTY bytes and forwards to that clip's session — not
    /// `self.selected_id()`, since a pinned clip is usually a different
    /// session than the one selected in the sidebar. Unpinning is strictly
    /// a mouse gesture: click the pinned clip again, or click anywhere
    /// outside the card. Returns `false` (nothing to do, or a chord key the
    /// keymap should drive) when no clip is pinned or the key belongs to
    /// the chord tier.
    pub(super) async fn handle_pinned_clip_key(&mut self, key: KeyEvent) -> bool {
        let Some(pinned_session_id) = self
            .program_popup
            .as_ref()
            .and_then(|popup| popup.pinned_clip.clone())
        else {
            return false;
        };
        // The global `C-x` prefix stays with the TUI keymap, exactly as it
        // does over a captured session PTY: starting a chord — and every key
        // continuing one — falls through to the keymap tier below, so
        // `C-x o`, `C-x z`, etc. keep working while a card is pinned. The
        // standard `C-x C-x` escape hatch forwards one literal C-x byte to
        // the pinned session instead.
        let is_ctrl_x = matches!(key.code, KeyCode::Char('x'))
            && key.modifiers.contains(KeyModifiers::CONTROL);
        if !self.chord_state.is_empty() {
            if is_ctrl_x {
                self.chord_state.reset();
                self.chord_label.clear();
                self.queue_pty_input(pinned_session_id, vec![0x18], "pinned_clip_pty_input");
                return true;
            }
            return false;
        }
        if is_ctrl_x {
            return false;
        }
        // Keyboard pan: the guaranteed path on terminals that report neither
        // horizontal wheel events nor Shift/Alt-modified wheels (many
        // reserve Shift+wheel for native selection/scrollback and never send
        // it to the app). Direction-literal: the arrow points where the crop
        // window moves — Shift+Up looks back from the tail, Shift+Right
        // reveals content further right.
        if key.modifiers.contains(KeyModifiers::SHIFT) {
            let v_step = PROGRAM_WHEEL_SCROLL_ROWS as i32;
            let h_step = PROGRAM_PINNED_PAN_COLS_STEP as i32;
            match key.code {
                KeyCode::Up => {
                    self.pan_pinned_card_by(0, v_step);
                    return true;
                }
                KeyCode::Down => {
                    self.pan_pinned_card_by(0, -v_step);
                    return true;
                }
                KeyCode::Left => {
                    self.pan_pinned_card_by(-h_step, 0);
                    return true;
                }
                KeyCode::Right => {
                    self.pan_pinned_card_by(h_step, 0);
                    return true;
                }
                _ => {}
            }
        }
        if let Some(bytes) = encode_key_to_bytes(key) {
            self.queue_pty_input(pinned_session_id, bytes, "pinned_clip_pty_input");
        }
        true
    }

    async fn handle_program_selection_menu_key(&mut self, key: KeyEvent) -> bool {
        let selection_active = self
            .program_popup
            .as_ref()
            .and_then(Self::program_selection_range)
            .is_some();
        if !selection_active {
            if let Some(popup) = self.program_popup.as_mut() {
                popup.selection_menu = None;
            }
            return false;
        }

        let ctrl = key.modifiers.contains(KeyModifiers::CONTROL);
        let alt = key.modifiers.contains(KeyModifiers::ALT);
        let super_mod = key.modifiers.contains(KeyModifiers::SUPER);
        let ctrl_char = Self::normalized_ctrl_char(key);
        let menu_focused = self
            .program_popup
            .as_ref()
            .and_then(|popup| popup.selection_menu.as_ref())
            .is_some_and(|menu| menu.focused);
        if !menu_focused {
            if matches!(key.code, KeyCode::Tab) && !ctrl && !alt && !super_mod {
                let Some(popup) = self.program_popup.as_mut() else {
                    return true;
                };
                let menu = popup.selection_menu.get_or_insert_with(Default::default);
                menu.focused = true;
                return true;
            }
            return false;
        }

        // Text-editing keys (cursor movement within the comment, insert,
        // delete) only take effect while Comment is the selected row (spec
        // 0087) — otherwise a keyboard-navigated Run/verb row would silently
        // eat keystrokes meant to move a cursor that isn't showing.
        let comment_active = self
            .program_popup
            .as_ref()
            .and_then(|popup| popup.selection_menu.as_ref())
            .is_some_and(|menu| menu.selected_action == ProgramSelectionAction::Comment);

        match key.code {
            KeyCode::Esc => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.focused = false;
                }
            }
            _ if ctrl_char == Some('g') => {
                if let Some(popup) = self.program_popup.as_mut() {
                    popup.selection = None;
                    popup.selection_menu = None;
                }
                self.layout.program_selection_run_hit = None;
                self.layout.program_selection_verb_hits.clear();
                self.set_status("program selection canceled".to_string());
            }
            KeyCode::Up => self.move_program_selection_action(-1),
            KeyCode::Down => self.move_program_selection_action(1),
            KeyCode::Tab => self.move_program_selection_action(1),
            KeyCode::BackTab => self.move_program_selection_action(-1),
            KeyCode::Enter => {
                let run_on_main = key.modifiers.contains(KeyModifiers::SHIFT);
                let action = self
                    .program_popup
                    .as_ref()
                    .and_then(|popup| popup.selection_menu.as_ref())
                    .map(|menu| menu.selected_action)
                    .unwrap_or(ProgramSelectionAction::Comment);
                match action {
                    ProgramSelectionAction::Verb(idx) => {
                        // Out-of-range only if the verb list shrank (live
                        // reload) while this row was selected — fall back to
                        // Run rather than silently doing nothing.
                        if let Some(verb) = self.program_verbs.get(idx).cloned() {
                            self.execute_program_selected_verb(verb.name, run_on_main).await;
                        } else {
                            let comment = self.program_popup.as_ref().and_then(|popup| {
                                Some(popup.selection_menu.as_ref()?.comment.clone())
                            });
                            self.execute_program_selected_text(comment, run_on_main).await;
                        }
                    }
                    ProgramSelectionAction::Comment | ProgramSelectionAction::Run => {
                        let comment = self
                            .program_popup
                            .as_ref()
                            .and_then(|popup| Some(popup.selection_menu.as_ref()?.comment.clone()));
                        self.execute_program_selected_text(comment, run_on_main).await;
                    }
                }
            }
            KeyCode::Left if comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = menu.cursor.saturating_sub(1);
                }
            }
            KeyCode::Right if comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = (menu.cursor + 1).min(menu.comment.chars().count());
                }
            }
            KeyCode::Home if comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = 0;
                }
            }
            KeyCode::End if comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = menu.comment.chars().count();
                }
            }
            KeyCode::Backspace if comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    if menu.cursor > 0 {
                        let idx = byte_pos(&menu.comment, menu.cursor - 1);
                        let next = byte_pos(&menu.comment, menu.cursor);
                        menu.comment.replace_range(idx..next, "");
                        menu.cursor -= 1;
                    }
                }
            }
            KeyCode::Delete if comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    let len = menu.comment.chars().count();
                    if menu.cursor < len {
                        let idx = byte_pos(&menu.comment, menu.cursor);
                        let next = byte_pos(&menu.comment, menu.cursor + 1);
                        menu.comment.replace_range(idx..next, "");
                    }
                }
            }
            _ if ctrl_char == Some('a') && comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = 0;
                }
            }
            _ if ctrl_char == Some('e') && comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = menu.comment.chars().count();
                }
            }
            _ if ctrl_char == Some('b') && comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = menu.cursor.saturating_sub(1);
                }
            }
            _ if ctrl_char == Some('f') && comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    menu.cursor = (menu.cursor + 1).min(menu.comment.chars().count());
                }
            }
            _ if ctrl_char == Some('d') && comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    let len = menu.comment.chars().count();
                    if menu.cursor < len {
                        let idx = byte_pos(&menu.comment, menu.cursor);
                        let next = byte_pos(&menu.comment, menu.cursor + 1);
                        menu.comment.replace_range(idx..next, "");
                    }
                }
            }
            _ if ctrl_char == Some('k') && comment_active => {
                if let Some(menu) = self
                    .program_popup
                    .as_mut()
                    .and_then(|popup| popup.selection_menu.as_mut())
                {
                    let len = menu.comment.chars().count();
                    if menu.cursor < len {
                        let idx = byte_pos(&menu.comment, menu.cursor);
                        let end = byte_pos(&menu.comment, len);
                        menu.comment.replace_range(idx..end, "");
                    }
                }
            }
            KeyCode::Char(c)
                if comment_active && ctrl_char.is_none() && !ctrl && !alt && !super_mod =>
            {
                if c != '\n' && c != '\r' {
                    if let Some(menu) = self
                        .program_popup
                        .as_mut()
                        .and_then(|popup| popup.selection_menu.as_mut())
                    {
                        let idx = byte_pos(&menu.comment, menu.cursor);
                        menu.comment.insert(idx, c);
                        menu.cursor += 1;
                    }
                }
            }
            _ if ctrl_char == Some('p') => self.move_program_selection_action(-1),
            _ if ctrl_char == Some('n') => self.move_program_selection_action(1),
            _ => {}
        }
        true
    }

    /// Move the selection menu's keyboard focus among its rows — Comment,
    /// Run, then each advertised verb in order — wrapping at both ends
    /// (spec 0089). `delta` is `-1` for Up/C-p, `1` for Down/C-n.
    fn move_program_selection_action(&mut self, delta: isize) {
        let verb_count = self.program_verbs.len();
        let count = 2 + verb_count;
        let Some(menu) = self
            .program_popup
            .as_mut()
            .and_then(|popup| popup.selection_menu.as_mut())
        else {
            return;
        };
        let current = match menu.selected_action {
            ProgramSelectionAction::Comment => 0,
            ProgramSelectionAction::Run => 1,
            ProgramSelectionAction::Verb(i) => 2 + i,
        };
        let next = (current as isize + delta).rem_euclid(count as isize) as usize;
        menu.selected_action = match next {
            0 => ProgramSelectionAction::Comment,
            1 => ProgramSelectionAction::Run,
            i => ProgramSelectionAction::Verb(i - 2),
        };
    }

    pub(super) async fn execute_program_selected_text(
        &mut self,
        comment: Option<String>,
        run_on_main: bool,
    ) -> bool {
        let selected = self.program_popup.as_ref().and_then(|popup| {
            Some((
                Self::selected_program_text(popup)?,
                Self::selected_program_block_ids(popup)?,
            ))
        });
        let Some((selection, selected_block_ids)) = selected else {
            return self
                .execute_program_popup_target(None, None, comment, !run_on_main)
                .await;
        };
        if let Some(popup) = self.program_popup.as_mut() {
            popup.selection = None;
            popup.selection_menu = None;
        }
        self.layout.program_selection_run_hit = None;
        self.layout.program_selection_verb_hits.clear();
        self.execute_program_popup_target(
            Some(selection),
            Some(selected_block_ids),
            comment,
            !run_on_main,
        )
        .await
    }

    /// Run a Program selection verb (spec 0089) on the active popup's current
    /// selection. Unlike `execute_program_selected_text` (Run), a verb always
    /// needs an explicit, non-empty selection — there is no whole-document
    /// fallback — and it doesn't touch the Run-progress/pending/shimmer
    /// machinery: the daemon owns the in-flight affordance for a verb (the
    /// provisional clip annotation it applies before this call returns), and
    /// the fork's direct edit arrives through the same `program/state`
    /// broadcast every other program-mutating call already uses — this method
    /// does not need to poke `popup.buffer` itself.
    pub(super) async fn execute_program_selected_verb(
        &mut self,
        verb: String,
        run_on_main: bool,
    ) -> bool {
        let Some(popup) = self.program_popup.as_ref() else {
            self.set_status("program verb failed: no active program".to_string());
            return false;
        };
        let Some(selection) = Self::selected_program_text(popup) else {
            self.set_status("program verb failed: no selection".to_string());
            return false;
        };
        let selected_block_ids = Self::selected_program_block_ids(popup);
        let session_id = popup.program.session_id.clone();
        let comment = popup
            .selection_menu
            .as_ref()
            .map(|menu| menu.comment.clone())
            .filter(|c| !c.trim().is_empty());

        let dirty = self.program_popup.as_ref().is_some_and(|popup| {
            program_normalize_smart_clip_instance_ids(&popup.buffer) != popup.saved_markdown
        });
        if dirty && !self.save_program_popup().await {
            return false;
        }

        if let Some(popup) = self.program_popup.as_mut() {
            popup.selection = None;
            popup.selection_menu = None;
        }
        self.layout.program_selection_run_hit = None;
        self.layout.program_selection_verb_hits.clear();

        let base_version = self
            .program_popup
            .as_ref()
            .map(|popup| popup.program.version);
        let selection = program_normalize_smart_clip_instance_ids(&selection);
        let selected_block_ids = selected_block_ids.filter(|ids| !ids.is_empty());
        // Optimistic shimmer (spec 0042/0087): mark the verb's block(s)
        // pending locally before the round trip, the same instant-feedback
        // treatment selection Run gives itself. `program_run_pending_with_existing`
        // unions with any already-active run on this session rather than
        // clobbering it, matching Run's own selection semantics.
        if let Some(ids) = selected_block_ids.clone() {
            let pending = self.program_run_pending_with_existing(&session_id, ids);
            self.start_program_run_with_pending(&session_id, pending);
        }
        let selection_block_ids: Option<Vec<String>> =
            selected_block_ids.map(|ids| ids.into_iter().collect());
        let params = construct_protocol::ProgramVerbExecuteParams {
            session_id,
            verb: verb.clone(),
            selection,
            base_version,
            comment,
            selection_block_ids,
            run_on_owner: run_on_main,
            direct_edit: true,
        };
        match self.client.program_verb_execute(params).await {
            Ok(result) => {
                self.set_status(format!(
                    "verb '{verb}' dispatched ({})",
                    short_id(&result.subagent_session_id)
                ));
                true
            }
            Err(e) => {
                self.set_status(format!("program verb failed: {e}"));
                false
            }
        }
    }

    fn normalized_ctrl_char(key: KeyEvent) -> Option<char> {
        let KeyCode::Char(c) = key.code else {
            return None;
        };
        if key.modifiers.contains(KeyModifiers::CONTROL) {
            return Some(match c {
                '\0' => '\0',
                ' ' | '@' => ' ',
                c => c.to_ascii_lowercase(),
            });
        }
        match c as u32 {
            0 => Some('\0'),
            1..=26 => char::from_u32((c as u32) - 1 + ('a' as u32)),
            _ => None,
        }
    }

    async fn publish_program_cursor(&mut self) {
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        let params = construct_protocol::ProgramCursorParams {
            session_id: popup.program.session_id.clone(),
            cursor: popup.cursor,
            selection_anchor: popup.selection.as_ref().map(|s| s.anchor),
            selection_head: popup.selection.as_ref().map(|s| s.head),
            version: Some(popup.program.version),
            label: Some("TUI".to_string()),
            clear: false,
        };
        if let Ok(result) = self.client.program_cursor(params).await {
            self.own_program_client_id = Some(result.cursor.client_id);
        }
    }

    async fn flush_program_live_edit(&mut self, before: String) {
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        if before == popup.buffer {
            return;
        }
        let Some(edit) = program_anchored_live_edit(&before, &popup.buffer) else {
            return;
        };
        let session_id = popup.program.session_id.clone();
        let params = construct_protocol::ProgramEditParams {
            session_id: session_id.clone(),
            edits: vec![edit],
            actor: construct_protocol::ProgramUpdateActor::Human,
            note: None,
            shimmer: Vec::new(),
        };
        let Ok(result) = self.client.program_edit(params).await else {
            return;
        };
        let Some(popup) = self.program_popup.as_mut() else {
            return;
        };
        if popup.program.session_id != session_id {
            return;
        }
        popup.program = result.program;
        popup.saved_markdown = popup.buffer.clone();
        popup.blocks = result.blocks;
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
        popup.selection_menu = Some(ProgramSelectionMenu::default());
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
            .map(|popup| {
                Self::program_smart_clip_selectable_count(&self.program_smart_clip_rows(popup))
            })
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
        let Some(row) = Self::program_smart_clip_selected_row(&rows, search.selected).cloned()
        else {
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
                .position(
                    |r| matches!(r, ProgramSmartClipRow::Category { group: g, .. } if *g == group),
                )
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
    pub(crate) fn program_smart_clip_rows(&self, popup: &ProgramPopup) -> Vec<ProgramSmartClipRow> {
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
        let harness = if session.harness == "antigravity" {
            "agy"
        } else {
            &session.harness
        };
        ProgramSmartClipCandidate {
            group: ProgramSmartClipGroup::Session,
            clip: format!("@{{session:{}}}", session.id),
            label: title,
            detail: format!("{harness} · {}", session.state.label()),
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

    /// Enter in the program body is list-aware (spec 0094): on a markdown
    /// list item with content it continues the list — the new line starts
    /// with the same indent and marker (checklists restart with an unchecked
    /// box), and any text after the caret becomes the new item's content. On
    /// an item with no content it dissolves the marker instead of stacking
    /// empty bullets, which is how a list is ended. Everywhere else — plain
    /// lines, a caret still inside the indent/marker, or an active selection
    /// (which Enter replaces) — it inserts a plain newline.
    pub(super) fn insert_program_newline(&mut self) {
        let action = {
            let Some(popup) = self.program_popup.as_ref() else {
                return;
            };
            if Self::program_selection_range(popup).is_some() {
                ProgramNewline::Plain
            } else {
                program_newline_action(&popup.buffer, popup.cursor)
            }
        };
        match action {
            ProgramNewline::Plain => self.insert_program_text("\n"),
            ProgramNewline::Continue(prefix) => self.insert_program_text(&format!("\n{prefix}")),
            ProgramNewline::ClearItem {
                line_start,
                line_end,
            } => {
                self.push_program_undo_state();
                let Some(popup) = self.program_popup.as_mut() else {
                    return;
                };
                let start = byte_pos(&popup.buffer, line_start);
                let end = byte_pos(&popup.buffer, line_end);
                popup.buffer.replace_range(start..end, "");
                popup.cursor = line_start;
                popup.preferred_col = None;
                popup.selection = None;
                Self::update_program_smart_clip_after_cursor_move(popup);
            }
        }
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

    pub(super) fn program_horizontal_cursor_target(
        &self,
        popup: &ProgramPopup,
        delta: isize,
    ) -> usize {
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
                // Never land inside an expanded image's rows — hop over the
                // block in the direction of travel (spec 0099).
                let target_row = ui::program_skip_attachment_rows(
                    Some(app),
                    &popup.buffer,
                    target_row,
                    delta > 0,
                    width,
                );
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
        let (char_start, char_end) = if let Some(range) =
            program_smart_clip_range_before_or_containing(&popup.buffer, popup.cursor)
        {
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
        let (char_start, char_end) = if let Some(range) =
            program_smart_clip_range_at_or_containing(&popup.buffer, popup.cursor)
        {
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

fn program_anchored_live_edit(
    before: &str,
    after: &str,
) -> Option<construct_protocol::ProgramEdit> {
    if before == after {
        return None;
    }
    let before_chars: Vec<char> = before.chars().collect();
    let after_chars: Vec<char> = after.chars().collect();
    let mut start = 0usize;
    while start < before_chars.len()
        && start < after_chars.len()
        && before_chars[start] == after_chars[start]
    {
        start += 1;
    }
    let mut old_end = before_chars.len();
    let mut new_end = after_chars.len();
    while old_end > start
        && new_end > start
        && before_chars[old_end - 1] == after_chars[new_end - 1]
    {
        old_end -= 1;
        new_end -= 1;
    }
    let max_context = before_chars.len().min(240);
    for ctx in 0..=max_context {
        let a = start.saturating_sub(ctx);
        let b = (old_end + ctx).min(before_chars.len());
        if a == b {
            continue;
        }
        let old_string: String = before_chars[a..b].iter().collect();
        if before.matches(&old_string).count() != 1 {
            continue;
        }
        let mut new_string = String::new();
        new_string.extend(before_chars[a..start].iter());
        new_string.extend(after_chars[start..new_end].iter());
        new_string.extend(before_chars[old_end..b].iter());
        return Some(construct_protocol::ProgramEdit {
            old_string,
            new_string,
            replace_all: false,
            keep_pending: false,
        });
    }
    Some(construct_protocol::ProgramEdit {
        old_string: before.to_string(),
        new_string: after.to_string(),
        replace_all: false,
        keep_pending: false,
    })
}
