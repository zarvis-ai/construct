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
    ScrollTop,
    ScrollBottom,
    ToggleHelp,
    /// Cycle approval mode on the selected session. Bound to
    /// `C-x A` (emacs) / `A` (vim).
    ToggleAutomode,
    /// Toggle terminal mouse capture. When disabled, native terminal
    /// selection works; agentd mouse interactions are suspended.
    ToggleMouseCapture,
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
        (Chord(vec![ch('i')]), OpenSendInput),
        (Chord(vec![ch('n')]), OpenNewSession),
        (Chord(vec![ctrl('x'), ch('b')]), OpenSwitchSession),
        (Chord(vec![shift('K')]), OpenDeleteConfirm),
        (Chord(vec![ch('d')]), OpenDiff),
        (Chord(vec![ctrl('c')]), Interrupt),
        // `r` opens the rename minibuffer; refresh moved to M-x refresh.
        (Chord(vec![ch('r')]), OpenRename),
        (Chord(vec![ch('f')]), OpenFork),
        (Chord(vec![ch('v')]), ToggleView),
        (Chord(vec![ch('z')]), ToggleZoom),
        (Chord(vec![ch(' ')]), TogglePin),
        (Chord(vec![ch('p')]), TogglePin),
        (Chord(vec![key(KeyCode::Right)]), ExpandGroup),
        (Chord(vec![key(KeyCode::Left)]), CollapseGroup),
        // Reorder selected session in the list. Shift-K/J already taken
        // (Shift-K = delete confirm), so we use Shift+arrows in vim too,
        // with C-x-prefixed Meta-free fallback for terminals that strip
        // the Shift modifier from arrow keys (macOS Terminal.app default).
        (Chord(vec![ctrl('x'), ctrl('p')]), MoveSelectedUp),
        (Chord(vec![ctrl('x'), ctrl('n')]), MoveSelectedDown),
        (Chord(vec![shift_key(KeyCode::Up)]), MoveSelectedUp),
        (Chord(vec![shift_key(KeyCode::Down)]), MoveSelectedDown),
        (Chord(vec![ch(':')]), OpenCommandPalette),
        // Enter from the list focuses the selected session's view;
        // Tab stays unbound for future use.
        (Chord(vec![key(KeyCode::Enter)]), FocusView),
        // PTY-mode escape: C-x is the universal prefix here too, so `C-x o`
        // cycles focus and `C-x C-c` quits even when the PTY is capturing.
        (Chord(vec![ctrl('x'), ch('o')]), SwitchFocus),
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
        (Chord(vec![ctrl('x'), ctrl('c')]), Quit),
        (Chord(vec![ctrl('x'), ch('t')]), ToggleView),
        (Chord(vec![ctrl('x'), ch('[')]), ScrollPageUp),
        (Chord(vec![ctrl('x'), ch(']')]), ScrollPageDown),
        (Chord(vec![key(KeyCode::PageUp)]), ScrollPageUp),
        (Chord(vec![key(KeyCode::PageDown)]), ScrollPageDown),
        (Chord(vec![ctrl('x'), ch('{')]), ScrollTop),
        (Chord(vec![ctrl('x'), shift('{')]), ScrollTop),
        (Chord(vec![ctrl('x'), ch('}')]), ScrollBottom),
        (Chord(vec![ctrl('x'), shift('}')]), ScrollBottom),
        (Chord(vec![ctrl('f')]), ScrollPageDown),
        (Chord(vec![ctrl('b')]), ScrollPageUp),
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
            KeymapResult::Action(KeyAction::ScrollTop)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), shift('{')]),
            KeymapResult::Action(KeyAction::ScrollTop)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), ch('}')]),
            KeymapResult::Action(KeyAction::ScrollBottom)
        ));
        assert!(matches!(
            resolve(&km, vec![ctrl('x'), shift('}')]),
            KeymapResult::Action(KeyAction::ScrollBottom)
        ));
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
}
