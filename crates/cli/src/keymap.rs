//! Keymap definitions. Default is emacs; an alternative vim profile is
//! provided. The TUI dispatches based on a small chord state machine so
//! emacs-style two-key bindings work.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KeyAction {
    Quit,
    NextSession,
    PrevSession,
    Refresh,
    EnterInsert,
    OpenSendInput,
    OpenNewSession,
    OpenDeleteConfirm,
    OpenRename,
    /// Fork the selected session into a new sibling session backed by a
    /// chosen harness (reuses the harness picker). Bound to `C-x f`
    /// (emacs) / `f` (vim) — distinct from "new session" (`C-x C-f` / `n`).
    OpenFork,
    /// Zoom: the session view fills the screen (list / pin strip / modeline
    /// all hidden; only the minibuffer stays). Toggling again restores the
    /// default layout. Bound to `C-x z` (emacs) / `z` (vim), matching
    /// tmux's `prefix z` (zoom-pane).
    ToggleZoom,
    /// Open the selected session's in-TUI program surface. Bound to
    /// `C-x Space` in both profiles because bare modifier double-taps are not
    /// delivered reliably by terminal emulators.
    OpenProgram,
    /// Save the selected session's in-TUI program surface. Bound to
    /// `C-x C-s`, matching the editor-style save chord.
    SaveProgram,
    /// Undo the selected program edit. Bound to `C-x u` for consistency with
    /// emacs-style history commands.
    UndoProgram,
    /// Run the selected session's program (or just the highlighted selection,
    /// when text is selected). Bound to `C-x C-r` — the keyboard equivalent of
    /// the title-bar ▶ button and the selection ▶ Run button.
    RunProgram,
    /// Toggle keyboard focus between an open Program surface and the
    /// underlying session terminal in the same split. Bound to `C-x C-o`:
    /// Program focus slides the Program right to expose the terminal; terminal
    /// focus slides it back and resumes Program editing.
    ToggleProgramTerminalFocus,
    OpenDiff,
    Interrupt,
    OpenCommandPalette,
    OpenSwitchSession,
    SplitWindowBelow,
    SplitWindowRight,
    DeleteWindow,
    DeleteOtherWindows,
    EnlargeWindow,
    EnlargeWindowHorizontally,
    ShrinkWindowHorizontally,
    /// Cycle keyboard focus across the panes (list ↔ view). Bound to `C-x o`
    /// in the emacs profile, matching `other-window`.
    SwitchFocus,
    /// Move keyboard focus to the spatially adjacent split window in a
    /// direction (emacs `windmove`). Reachable via the `C-x` prefix
    /// (`C-x <arrow>`) so it works even when the terminal reserves
    /// `Shift+<arrow>` for its own scrollback — which iTerm2, macOS
    /// Terminal.app, and GNOME Terminal all do for `Shift+Up`/`Shift+Down`,
    /// the reason the bare `Shift+Arrow` binding never reaches the app there.
    FocusWindowUp,
    FocusWindowDown,
    FocusWindowLeft,
    FocusWindowRight,
    /// Move keyboard focus into the selected session's view pane (from
    /// the list). Acts on Enter from the list — a one-way "drill in"
    /// counterpart to `SwitchFocus`'s toggle.
    FocusView,
    ToggleView,
    /// Pin / unpin the currently-selected session so it stays in the pin
    /// strip below the main view. On a group selection: pin or unpin all
    /// members at once (binary toggle: if all are pinned, unpin all;
    /// otherwise pin all).
    TogglePin,
    /// Right arrow on a group selection → expand it.
    ExpandGroup,
    /// Left arrow on a group selection → collapse it.
    CollapseGroup,
    /// Reorder: move the selected session up one slot in the list.
    MoveSelectedUp,
    /// Reorder: move the selected session down one slot in the list.
    MoveSelectedDown,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollHalfPageUp,
    ScrollHalfPageDown,
    ScrollTop,
    ScrollBottom,
    ToggleHelp,
    /// Cycle approval mode on the selected session. Bound to
    /// `C-x A` (emacs) / `A` (vim).
    ToggleAutomode,
    /// Toggle terminal mouse capture. When disabled, native terminal
    /// selection works; agentd mouse interactions are suspended.
    ToggleMouseCapture,
    /// Cycle the active UI color theme. Click-only for the minibuffer theme
    /// affordance; `/theme` remains the keyboard-facing command.
    CycleTheme,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    Emacs,
    Vim,
}

impl Profile {
    pub fn label(self) -> &'static str {
        match self {
            Profile::Emacs => "emacs",
            Profile::Vim => "vim",
        }
    }

    pub fn from_env() -> Self {
        match std::env::var("CONSTRUCT_KEYMAP").as_deref() {
            Ok("vim") => Profile::Vim,
            _ => Profile::Emacs,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Chord(pub Vec<KeyEvent>);

pub struct Keymap {
    pub bindings: Vec<(Chord, KeyAction)>,
}

#[derive(Default)]
pub struct ChordState {
    pending: Vec<KeyEvent>,
}

#[derive(Debug, Clone)]
pub enum KeymapResult {
    Action(KeyAction),
    Pending(String),
    Unhandled,
}

impl ChordState {
    pub fn is_empty(&self) -> bool {
        self.pending.is_empty()
    }

    pub fn handle(&mut self, ev: KeyEvent, km: &Keymap) -> KeymapResult {
        // Normalize the event: ignore Release events on platforms that emit them.
        if !matches!(
            ev.kind,
            crossterm::event::KeyEventKind::Press | crossterm::event::KeyEventKind::Repeat
        ) {
            return KeymapResult::Pending(self.label());
        }
        let mut probe = self.pending.clone();
        probe.push(ev);

        let mut exact: Option<KeyAction> = None;
        let mut has_prefix = false;
        for (chord, action) in &km.bindings {
            if chord.0 == probe {
                exact = Some(*action);
            } else if chord.0.len() > probe.len() && chord.0.starts_with(&probe) {
                has_prefix = true;
            }
        }
        if let Some(a) = exact {
            self.pending.clear();
            return KeymapResult::Action(a);
        }
        if has_prefix {
            self.pending = probe;
            return KeymapResult::Pending(self.label());
        }
        self.pending.clear();
        KeymapResult::Unhandled
    }

    pub fn reset(&mut self) {
        self.pending.clear();
    }

    pub fn label(&self) -> String {
        self.pending
            .iter()
            .map(format_key)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::NONE)
}
fn ctrl(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::CONTROL)
}
fn alt(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::ALT)
}
fn ch(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE)
}
fn shift(c: char) -> KeyEvent {
    KeyEvent::new(KeyCode::Char(c), KeyModifiers::SHIFT)
}
fn shift_key(code: KeyCode) -> KeyEvent {
    KeyEvent::new(code, KeyModifiers::SHIFT)
}

pub fn default_for(profile: Profile) -> Keymap {
    match profile {
        Profile::Emacs => emacs(),
        Profile::Vim => vim(),
    }
}

fn emacs() -> Keymap {
    use KeyAction::*;
    let bindings = vec![
        // Quit
        (Chord(vec![ctrl('x'), ctrl('c')]), Quit),
        // Session navigation
        (Chord(vec![ctrl('n')]), NextSession),
        (Chord(vec![ctrl('p')]), PrevSession),
        (Chord(vec![key(KeyCode::Down)]), NextSession),
        (Chord(vec![key(KeyCode::Up)]), PrevSession),
        // Focus + view (C-x prefix, matching emacs window commands).
        // Enter from the list "drills in" to the session view; Tab is
        // intentionally left unbound for future use (e.g. completion).
        (Chord(vec![ctrl('x'), ch('o')]), SwitchFocus),
        // Directional pane focus (emacs `windmove`). `Shift+<arrow>` is the
        // fast path, but terminals reserve `Shift+Up`/`Shift+Down` for
        // scrollback (iTerm2, macOS Terminal.app, GNOME Terminal) and never
        // deliver them, so the vertical axis silently dies there. The `C-x`
        // prefix is always forwarded — it's how `C-x o` already escapes a
        // focused child PTY — so `C-x <arrow>` is a reliable alias.
        (Chord(vec![ctrl('x'), key(KeyCode::Up)]), FocusWindowUp),
        (Chord(vec![ctrl('x'), key(KeyCode::Down)]), FocusWindowDown),
        (Chord(vec![ctrl('x'), key(KeyCode::Left)]), FocusWindowLeft),
        (Chord(vec![ctrl('x'), key(KeyCode::Right)]), FocusWindowRight),
        (Chord(vec![ctrl('x'), ch('2')]), SplitWindowBelow),
        (Chord(vec![ctrl('x'), ch('3')]), SplitWindowRight),
        (Chord(vec![ctrl('x'), ch('0')]), DeleteWindow),
        (Chord(vec![ctrl('x'), ch('1')]), DeleteOtherWindows),
        (Chord(vec![ctrl('x'), ch('^')]), EnlargeWindow),
        (Chord(vec![ctrl('x'), ch('}')]), EnlargeWindowHorizontally),
        (
            Chord(vec![ctrl('x'), shift('}')]),
            EnlargeWindowHorizontally,
        ),
        (Chord(vec![ctrl('x'), ch('{')]), ShrinkWindowHorizontally),
        (Chord(vec![ctrl('x'), shift('{')]), ShrinkWindowHorizontally),
        (Chord(vec![key(KeyCode::Enter)]), FocusView),
        (Chord(vec![ctrl('x'), ch('t')]), ToggleView),
        (Chord(vec![ctrl('x'), ch('z')]), ToggleZoom),
        // Session actions
        (Chord(vec![ctrl('x'), ctrl('f')]), OpenNewSession),
        (Chord(vec![ctrl('x'), ch('b')]), OpenSwitchSession),
        (Chord(vec![ctrl('x'), ch('k')]), OpenDeleteConfirm),
        (Chord(vec![ctrl('x'), ch(' ')]), OpenProgram),
        (Chord(vec![ctrl('x'), ctrl('s')]), SaveProgram),
        (Chord(vec![ctrl('x'), ch('u')]), UndoProgram),
        (Chord(vec![ctrl('x'), ctrl('r')]), RunProgram),
        (
            Chord(vec![ctrl('x'), ctrl('o')]),
            ToggleProgramTerminalFocus,
        ),
        (Chord(vec![ctrl('x'), ch('d')]), OpenDiff),
        (Chord(vec![ctrl('x'), ch('i')]), OpenSendInput),
        // `C-x r` opens the rename minibuffer (with current title pre-filled).
        // Refresh moved to the command palette (M-x refresh) — it's rarely
        // needed since the daemon pushes state changes automatically.
        (Chord(vec![ctrl('x'), ch('r')]), OpenRename),
        // `C-x f` forks the selected session into a new harness (distinct
        // from `C-x C-f`, which creates a fresh session).
        (Chord(vec![ctrl('x'), ch('f')]), OpenFork),
        // Pin / unpin selected session (or all members of a selected group)
        (Chord(vec![ctrl('x'), ch('p')]), TogglePin),
        (Chord(vec![ch(' ')]), TogglePin),
        // Expand / collapse on a group selection
        (Chord(vec![key(KeyCode::Right)]), ExpandGroup),
        (Chord(vec![key(KeyCode::Left)]), CollapseGroup),
        // Reorder selected session in the list. macOS Terminal.app doesn't
        // pass Shift through with arrow keys, so the C-x-prefixed bindings
        // are the reliable path — Shift+arrow stays as an alias for
        // terminals that *do* forward the modifier (iTerm2, WezTerm, etc).
        (Chord(vec![ctrl('x'), ctrl('p')]), MoveSelectedUp),
        (Chord(vec![ctrl('x'), ctrl('n')]), MoveSelectedDown),
        (Chord(vec![shift_key(KeyCode::Up)]), MoveSelectedUp),
        (Chord(vec![shift_key(KeyCode::Down)]), MoveSelectedDown),
        // Interrupt the running adapter (emacs comint convention)
        (Chord(vec![ctrl('c'), ctrl('c')]), Interrupt),
        // Command palette — `M-x` is the canonical emacs binding; `C-x x` is
        // a Meta-free alias so it works on macOS Terminal.app without setting
        // "Use Option as Meta key".
        (Chord(vec![alt('x')]), OpenCommandPalette),
        (Chord(vec![ctrl('x'), ch('x')]), OpenCommandPalette),
        // Scroll
        (Chord(vec![ctrl('x'), ch('[')]), ScrollPageUp),
        (Chord(vec![ctrl('x'), ch(']')]), ScrollPageDown),
        (Chord(vec![ctrl('v')]), ScrollPageDown),
        (Chord(vec![alt('v')]), ScrollPageUp),
        (Chord(vec![ch('g'), ch('g')]), ScrollTop),
        (Chord(vec![shift('G')]), ScrollBottom),
        (Chord(vec![key(KeyCode::PageDown)]), ScrollPageDown),
        (Chord(vec![key(KeyCode::PageUp)]), ScrollPageUp),
        // Cycle approval mode on the selected session (smith / future agents).
        (Chord(vec![ctrl('x'), shift('A')]), ToggleAutomode),
        // Give the terminal mouse back for native text selection/copy.
        (Chord(vec![ctrl('x'), ch('m')]), ToggleMouseCapture),
        // Help
        (Chord(vec![ch('?')]), ToggleHelp),
    ];
    Keymap { bindings }
}

fn vim() -> Keymap {
    use KeyAction::*;
    let bindings = vec![
        (Chord(vec![ch('j')]), NextSession),
        (Chord(vec![ch('k')]), PrevSession),
        (Chord(vec![key(KeyCode::Down)]), NextSession),
        (Chord(vec![key(KeyCode::Up)]), PrevSession),
        (Chord(vec![ch('i')]), EnterInsert),
        (Chord(vec![ch('a')]), EnterInsert),
        (Chord(vec![shift('I')]), OpenSendInput),
        (Chord(vec![ch('o')]), OpenNewSession),
        (Chord(vec![ch('n')]), OpenNewSession),
        (Chord(vec![ch('/')]), OpenSwitchSession),
        (Chord(vec![ctrl('x'), ch('b')]), OpenSwitchSession),
        (Chord(vec![ch('d'), ch('d')]), OpenDeleteConfirm),
        (Chord(vec![ctrl('x'), ch(' ')]), OpenProgram),
        (Chord(vec![ctrl('x'), ctrl('s')]), SaveProgram),
        (Chord(vec![ctrl('x'), ch('u')]), UndoProgram),
        (Chord(vec![ctrl('x'), ctrl('r')]), RunProgram),
        (
            Chord(vec![ctrl('x'), ctrl('o')]),
            ToggleProgramTerminalFocus,
        ),
        (Chord(vec![ch('g'), ch('d')]), OpenDiff),
        (Chord(vec![ctrl('c')]), Interrupt),
        // `r` opens the rename minibuffer; refresh moved to M-x refresh.
        (Chord(vec![ch('r')]), OpenRename),
        (Chord(vec![shift('O')]), OpenFork),
        (Chord(vec![ch('f')]), OpenFork),
        (Chord(vec![ch('v')]), ToggleView),
        (Chord(vec![ch('z')]), ToggleZoom),
        (Chord(vec![shift('Z'), shift('Z')]), Quit),
        (Chord(vec![ch(' ')]), TogglePin),
        (Chord(vec![ch('p')]), TogglePin),
        (Chord(vec![key(KeyCode::Right)]), ExpandGroup),
        (Chord(vec![key(KeyCode::Left)]), CollapseGroup),
        // Reorder selected session in the list. Shift+J/K match vim's
        // "move line" mnemonics; the C-x-prefixed fallback still works in
        // terminals that strip the Shift modifier from arrow keys.
        (Chord(vec![ctrl('x'), ctrl('p')]), MoveSelectedUp),
        (Chord(vec![ctrl('x'), ctrl('n')]), MoveSelectedDown),
        (Chord(vec![shift('K')]), MoveSelectedUp),
        (Chord(vec![shift('J')]), MoveSelectedDown),
        (Chord(vec![shift_key(KeyCode::Up)]), MoveSelectedUp),
        (Chord(vec![shift_key(KeyCode::Down)]), MoveSelectedDown),
        (Chord(vec![ch(':')]), OpenCommandPalette),
        // Enter from the list focuses the selected session's view;
        // Tab stays unbound for future use.
        (Chord(vec![key(KeyCode::Enter)]), FocusView),
        // PTY-mode escape: C-x is the universal prefix here too, so `C-x o`
        // cycles focus and `C-x C-c` quits even when the PTY is capturing.
        (Chord(vec![ctrl('x'), ch('o')]), SwitchFocus),
        // Directional pane focus, `C-x <arrow>` — reliable alias for
        // `Shift+<arrow>` where the terminal eats `Shift+Up`/`Shift+Down`
        // (see the emacs profile for the rationale).
        (Chord(vec![ctrl('x'), key(KeyCode::Up)]), FocusWindowUp),
        (Chord(vec![ctrl('x'), key(KeyCode::Down)]), FocusWindowDown),
        (Chord(vec![ctrl('x'), key(KeyCode::Left)]), FocusWindowLeft),
        (Chord(vec![ctrl('x'), key(KeyCode::Right)]), FocusWindowRight),
        (Chord(vec![ctrl('x'), ch('2')]), SplitWindowBelow),
        (Chord(vec![ctrl('x'), ch('3')]), SplitWindowRight),
        (Chord(vec![ctrl('x'), ch('0')]), DeleteWindow),
        (Chord(vec![ctrl('x'), ch('1')]), DeleteOtherWindows),
        (Chord(vec![ctrl('x'), ch('^')]), EnlargeWindow),
        (Chord(vec![ctrl('x'), ch('}')]), EnlargeWindowHorizontally),
        (
            Chord(vec![ctrl('x'), shift('}')]),
            EnlargeWindowHorizontally,
        ),
        (Chord(vec![ctrl('x'), ch('{')]), ShrinkWindowHorizontally),
        (Chord(vec![ctrl('x'), shift('{')]), ShrinkWindowHorizontally),
        (Chord(vec![ctrl('w'), ch('s')]), SplitWindowBelow),
        (Chord(vec![ctrl('w'), ch('v')]), SplitWindowRight),
        (Chord(vec![ctrl('w'), ch('h')]), FocusWindowLeft),
        (Chord(vec![ctrl('w'), ch('j')]), FocusWindowDown),
        (Chord(vec![ctrl('w'), ch('k')]), FocusWindowUp),
        (Chord(vec![ctrl('w'), ch('l')]), FocusWindowRight),
        (Chord(vec![ctrl('w'), ch('w')]), SwitchFocus),
        (Chord(vec![ctrl('w'), ch('c')]), DeleteWindow),
        (Chord(vec![ctrl('w'), ch('o')]), DeleteOtherWindows),
        (Chord(vec![ctrl('w'), ch('+')]), EnlargeWindow),
        (Chord(vec![ctrl('w'), shift('+')]), EnlargeWindow),
        (Chord(vec![ctrl('w'), ch('>')]), EnlargeWindowHorizontally),
        (
            Chord(vec![ctrl('w'), shift('>')]),
            EnlargeWindowHorizontally,
        ),
        (Chord(vec![ctrl('w'), ch('<')]), ShrinkWindowHorizontally),
        (Chord(vec![ctrl('w'), shift('<')]), ShrinkWindowHorizontally),
        (Chord(vec![ctrl('w'), ch('z')]), ToggleZoom),
        (Chord(vec![ctrl('x'), ctrl('c')]), Quit),
        (Chord(vec![ctrl('x'), ch('t')]), ToggleView),
        (Chord(vec![ctrl('x'), ch('[')]), ScrollPageUp),
        (Chord(vec![ctrl('x'), ch(']')]), ScrollPageDown),
        (Chord(vec![key(KeyCode::PageUp)]), ScrollPageUp),
        (Chord(vec![key(KeyCode::PageDown)]), ScrollPageDown),
        (Chord(vec![ctrl('f')]), ScrollPageDown),
        (Chord(vec![ctrl('b')]), ScrollPageUp),
        (Chord(vec![ctrl('d')]), ScrollHalfPageDown),
        (Chord(vec![ctrl('u')]), ScrollHalfPageUp),
        (Chord(vec![ctrl('e')]), ScrollDown),
        (Chord(vec![ctrl('y')]), ScrollUp),
        (Chord(vec![ch('g'), ch('g')]), ScrollTop),
        (Chord(vec![shift('G')]), ScrollBottom),
        (Chord(vec![shift('A')]), ToggleAutomode),
        (Chord(vec![ctrl('x'), ch('m')]), ToggleMouseCapture),
        (Chord(vec![ch('?')]), ToggleHelp),
    ];
    Keymap { bindings }
}

pub fn format_key(k: &KeyEvent) -> String {
    let mut s = String::new();
    if k.modifiers.contains(KeyModifiers::CONTROL) {
        s.push_str("C-");
    }
    if k.modifiers.contains(KeyModifiers::ALT) {
        s.push_str("M-");
    }
    if k.modifiers.contains(KeyModifiers::SHIFT) {
        s.push_str("S-");
    }
    match k.code {
        KeyCode::Char(c) => s.push(c),
        KeyCode::Up => s.push_str("up"),
        KeyCode::Down => s.push_str("down"),
        KeyCode::Left => s.push_str("left"),
        KeyCode::Right => s.push_str("right"),
        KeyCode::Enter => s.push_str("RET"),
        KeyCode::Esc => s.push_str("ESC"),
        KeyCode::Tab => s.push_str("TAB"),
        KeyCode::PageUp => s.push_str("PgUp"),
        KeyCode::PageDown => s.push_str("PgDn"),
        KeyCode::Home => s.push_str("Home"),
        KeyCode::End => s.push_str("End"),
        KeyCode::Backspace => s.push_str("BS"),
        other => s.push_str(&format!("{:?}", other)),
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resolve(km: &Keymap, keys: Vec<KeyEvent>) -> KeymapResult {
        let mut state = ChordState::default();
        let mut result = KeymapResult::Unhandled;
        for key in keys {
            result = state.handle(key, km);
        }
        result
    }

    fn assert_action(km: &Keymap, keys: Vec<KeyEvent>, action: KeyAction) {
        assert!(
            matches!(resolve(km, keys), KeymapResult::Action(a) if a == action),
            "expected {action:?}"
        );
    }

    #[test]
    fn c_x_bracket_scroll_chords_work_in_emacs_profile() {
        let km = default_for(Profile::Emacs);
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch('[')]),
            KeymapResult::Action(KeyAction::ScrollPageUp)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch(']')]),
            KeymapResult::Action(KeyAction::ScrollPageDown)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch('{')]),
            KeymapResult::Action(KeyAction::ShrinkWindowHorizontally)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), shift('{')]),
            KeymapResult::Action(KeyAction::ShrinkWindowHorizontally)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch('}')]),
            KeymapResult::Action(KeyAction::EnlargeWindowHorizontally)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), shift('}')]),
            KeymapResult::Action(KeyAction::EnlargeWindowHorizontally)
        ));
    }

    /// `C-x {` / `C-x }` mean horizontal resize in both profiles. The vim
    /// table used to also bind them to ScrollTop/ScrollBottom; last-match-wins
    /// dispatch made the resize bindings silently dead there (`gg`/`G` already
    /// cover scroll top/bottom in vim).
    #[test]
    fn c_x_bracket_scroll_chords_work_in_vim_profile() {
        let km = default_for(Profile::Vim);
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch('[')]),
            KeymapResult::Action(KeyAction::ScrollPageUp)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch(']')]),
            KeymapResult::Action(KeyAction::ScrollPageDown)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch('{')]),
            KeymapResult::Action(KeyAction::ShrinkWindowHorizontally)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), shift('{')]),
            KeymapResult::Action(KeyAction::ShrinkWindowHorizontally)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch('}')]),
            KeymapResult::Action(KeyAction::EnlargeWindowHorizontally)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), shift('}')]),
            KeymapResult::Action(KeyAction::EnlargeWindowHorizontally)
        ));
    }

    /// Chord dispatch is last-match-wins, so a chord bound twice silently
    /// disables the earlier binding. Keep every profile's table free of
    /// duplicates.
    #[test]
    fn no_duplicate_chords_in_any_profile() {
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            for (i, (chord_a, action_a)) in km.bindings.iter().enumerate() {
                for (chord_b, action_b) in km.bindings.iter().skip(i + 1) {
                    assert!(
                        chord_a != chord_b,
                        "{profile:?}: chord {} bound to both {action_a:?} and {action_b:?}",
                        chord_a
                            .0
                            .iter()
                            .map(format_key)
                            .collect::<Vec<_>>()
                            .join(" ")
                    );
                }
            }
        }
    }

    #[test]
    fn c_x_arrow_focuses_windows_directionally_in_both_profiles() {
        // `C-x <arrow>` is the terminal-agnostic alias for `Shift+<arrow>`
        // directional pane focus — it must resolve in both profiles, because
        // terminals (iTerm2, Terminal.app, GNOME Terminal) eat Shift+Up/Down
        // for scrollback and the bare Shift+Arrow binding never arrives there.
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), key(KeyCode::Up)]),
                    KeymapResult::Action(KeyAction::FocusWindowUp)
                ),
                "C-x Up should focus the window above in {profile:?}"
            );
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), key(KeyCode::Down)]),
                    KeymapResult::Action(KeyAction::FocusWindowDown)
                ),
                "C-x Down should focus the window below in {profile:?}"
            );
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), key(KeyCode::Left)]),
                    KeymapResult::Action(KeyAction::FocusWindowLeft)
                ),
                "C-x Left should focus the window to the left in {profile:?}"
            );
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), key(KeyCode::Right)]),
                    KeymapResult::Action(KeyAction::FocusWindowRight)
                ),
                "C-x Right should focus the window to the right in {profile:?}"
            );
        }
    }

    #[test]
    fn pageup_pagedown_page_scroll_in_both_profiles() {
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            assert!(
                matches!(
                    resolve(&km, vec![key(KeyCode::PageUp)]),
                    KeymapResult::Action(KeyAction::ScrollPageUp)
                ),
                "PageUp should page up (like C-x [) in {profile:?}"
            );
            assert!(
                matches!(
                    resolve(&km, vec![key(KeyCode::PageDown)]),
                    KeymapResult::Action(KeyAction::ScrollPageDown)
                ),
                "PageDown should page down (like C-x ]) in {profile:?}"
            );
        }
    }

    #[test]
    fn c_x_space_opens_program_without_shadowing_c_x_ctrl_c_quit() {
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), ch(' ')]),
                    KeymapResult::Action(KeyAction::OpenProgram)
                ),
                "C-x Space should open program in {profile:?}"
            );
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), ctrl('c')]),
                    KeymapResult::Action(KeyAction::Quit)
                ),
                "C-x C-c should still quit in {profile:?}"
            );
        }
    }

    #[test]
    fn c_x_ctrl_s_saves_program() {
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), ctrl('s')]),
                    KeymapResult::Action(KeyAction::SaveProgram)
                ),
                "C-x C-s should save program in {profile:?}"
            );
        }
    }

    #[test]
    fn c_x_u_undo_program() {
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), ch('u')]),
                    KeymapResult::Action(KeyAction::UndoProgram)
                ),
                "C-x u should trigger UndoProgram in {profile:?}"
            );
        }
    }

    #[test]
    fn c_x_ctrl_r_runs_program() {
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), ctrl('r')]),
                    KeymapResult::Action(KeyAction::RunProgram)
                ),
                "C-x C-r should trigger RunProgram in {profile:?}"
            );
        }
    }

    #[test]
    fn c_x_ctrl_o_toggles_program_terminal_focus() {
        for profile in [Profile::Emacs, Profile::Vim] {
            let km = default_for(profile);
            assert!(
                matches!(
                    resolve(&km, vec![ctrl('x'), ctrl('o')]),
                    KeymapResult::Action(KeyAction::ToggleProgramTerminalFocus)
                ),
                "C-x C-o should toggle Program/session focus in {profile:?}"
            );
        }
    }

    #[test]
    fn vim_phase2_chords_resolve_to_expected_actions() {
        let km = default_for(Profile::Vim);

        assert_action(&km, vec![ch('d'), ch('d')], KeyAction::OpenDeleteConfirm);
        assert_action(&km, vec![ch('g'), ch('d')], KeyAction::OpenDiff);
        assert_action(&km, vec![ch('o')], KeyAction::OpenNewSession);
        assert_action(&km, vec![ch('n')], KeyAction::OpenNewSession);
        assert_action(&km, vec![shift('O')], KeyAction::OpenFork);
        assert_action(&km, vec![ch('f')], KeyAction::OpenFork);
        assert_action(&km, vec![shift('J')], KeyAction::MoveSelectedDown);
        assert_action(&km, vec![shift('K')], KeyAction::MoveSelectedUp);
        assert_action(&km, vec![ch('/')], KeyAction::OpenSwitchSession);
        assert_action(&km, vec![ctrl('x'), ch('b')], KeyAction::OpenSwitchSession);
        assert_action(&km, vec![shift('I')], KeyAction::OpenSendInput);
        assert_action(&km, vec![ch('i')], KeyAction::EnterInsert);
        assert_action(&km, vec![ch('a')], KeyAction::EnterInsert);
        assert_action(&km, vec![shift('Z'), shift('Z')], KeyAction::Quit);
        assert_action(&km, vec![ctrl('d')], KeyAction::ScrollHalfPageDown);
        assert_action(&km, vec![ctrl('u')], KeyAction::ScrollHalfPageUp);
        assert_action(&km, vec![ctrl('e')], KeyAction::ScrollDown);
        assert_action(&km, vec![ctrl('y')], KeyAction::ScrollUp);
    }

    #[test]
    fn vim_c_w_window_chords_resolve_to_expected_actions() {
        let km = default_for(Profile::Vim);

        assert_action(&km, vec![ctrl('w'), ch('s')], KeyAction::SplitWindowBelow);
        assert_action(&km, vec![ctrl('w'), ch('v')], KeyAction::SplitWindowRight);
        assert_action(&km, vec![ctrl('w'), ch('h')], KeyAction::FocusWindowLeft);
        assert_action(&km, vec![ctrl('w'), ch('j')], KeyAction::FocusWindowDown);
        assert_action(&km, vec![ctrl('w'), ch('k')], KeyAction::FocusWindowUp);
        assert_action(&km, vec![ctrl('w'), ch('l')], KeyAction::FocusWindowRight);
        assert_action(&km, vec![ctrl('w'), ch('w')], KeyAction::SwitchFocus);
        assert_action(&km, vec![ctrl('w'), ch('c')], KeyAction::DeleteWindow);
        assert_action(&km, vec![ctrl('w'), ch('o')], KeyAction::DeleteOtherWindows);
        assert_action(&km, vec![ctrl('w'), ch('+')], KeyAction::EnlargeWindow);
        assert_action(&km, vec![ctrl('w'), shift('+')], KeyAction::EnlargeWindow);
        assert_action(
            &km,
            vec![ctrl('w'), ch('>')],
            KeyAction::EnlargeWindowHorizontally,
        );
        assert_action(
            &km,
            vec![ctrl('w'), shift('>')],
            KeyAction::EnlargeWindowHorizontally,
        );
        assert_action(
            &km,
            vec![ctrl('w'), ch('<')],
            KeyAction::ShrinkWindowHorizontally,
        );
        assert_action(
            &km,
            vec![ctrl('w'), shift('<')],
            KeyAction::ShrinkWindowHorizontally,
        );
        assert_action(&km, vec![ctrl('w'), ch('z')], KeyAction::ToggleZoom);
    }
}
