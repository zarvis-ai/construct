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
    OpenKillConfirm,
    OpenDiff,
    Interrupt,
    OpenCommandPalette,
    /// Cycle keyboard focus across the panes (list ↔ view). Bound to `C-x o`
    /// in the emacs profile, matching `other-window`.
    SwitchFocus,
    ToggleView,
    ScrollUp,
    ScrollDown,
    ScrollPageUp,
    ScrollPageDown,
    ScrollTop,
    ScrollBottom,
    ToggleHelp,
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
        match std::env::var("AGENTD_KEYMAP").as_deref() {
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
        (Chord(vec![ch('q')]), Quit),
        // Session navigation
        (Chord(vec![ctrl('n')]), NextSession),
        (Chord(vec![ctrl('p')]), PrevSession),
        (Chord(vec![key(KeyCode::Down)]), NextSession),
        (Chord(vec![key(KeyCode::Up)]), PrevSession),
        // Focus + view (C-x prefix, matching emacs window commands)
        (Chord(vec![ctrl('x'), ch('o')]), SwitchFocus),
        (Chord(vec![key(KeyCode::Tab)]), SwitchFocus),
        (Chord(vec![ctrl('x'), ch('t')]), ToggleView),
        // Session actions
        (Chord(vec![ctrl('x'), ctrl('f')]), OpenNewSession),
        (Chord(vec![ctrl('x'), ch('k')]), OpenKillConfirm),
        (Chord(vec![ctrl('x'), ch('d')]), OpenDiff),
        (Chord(vec![ctrl('x'), ch('i')]), OpenSendInput),
        (Chord(vec![ctrl('x'), ch('r')]), Refresh),
        // Interrupt the running adapter (emacs comint convention)
        (Chord(vec![ctrl('c'), ctrl('c')]), Interrupt),
        // Command palette — `M-x` is the canonical emacs binding; `C-x x` is
        // a Meta-free alias so it works on macOS Terminal.app without setting
        // "Use Option as Meta key".
        (Chord(vec![alt('x')]), OpenCommandPalette),
        (Chord(vec![ctrl('x'), ch('x')]), OpenCommandPalette),
        // Scroll
        (Chord(vec![ctrl('v')]), ScrollPageDown),
        (Chord(vec![alt('v')]), ScrollPageUp),
        (Chord(vec![ch('g'), ch('g')]), ScrollTop),
        (Chord(vec![shift('G')]), ScrollBottom),
        (Chord(vec![key(KeyCode::PageDown)]), ScrollPageDown),
        (Chord(vec![key(KeyCode::PageUp)]), ScrollPageUp),
        // Help
        (Chord(vec![ch('?')]), ToggleHelp),
    ];
    Keymap { bindings }
}

fn vim() -> Keymap {
    use KeyAction::*;
    let bindings = vec![
        (Chord(vec![ch('q')]), Quit),
        (Chord(vec![ch('j')]), NextSession),
        (Chord(vec![ch('k')]), PrevSession),
        (Chord(vec![key(KeyCode::Down)]), NextSession),
        (Chord(vec![key(KeyCode::Up)]), PrevSession),
        (Chord(vec![ch('i')]), OpenSendInput),
        (Chord(vec![ch('n')]), OpenNewSession),
        (Chord(vec![shift('K')]), OpenKillConfirm),
        (Chord(vec![ch('d')]), OpenDiff),
        (Chord(vec![ctrl('c')]), Interrupt),
        (Chord(vec![ch('r')]), Refresh),
        (Chord(vec![ch('v')]), ToggleView),
        (Chord(vec![ch(':')]), OpenCommandPalette),
        (Chord(vec![key(KeyCode::Tab)]), SwitchFocus),
        // PTY-mode escape: C-x is the universal prefix here too, so `C-x o`
        // cycles focus and `C-x C-c` quits even when the PTY is capturing.
        (Chord(vec![ctrl('x'), ch('o')]), SwitchFocus),
        (Chord(vec![ctrl('x'), ctrl('c')]), Quit),
        (Chord(vec![ctrl('x'), ch('t')]), ToggleView),
        (Chord(vec![ctrl('f')]), ScrollPageDown),
        (Chord(vec![ctrl('b')]), ScrollPageUp),
        (Chord(vec![ch('g'), ch('g')]), ScrollTop),
        (Chord(vec![shift('G')]), ScrollBottom),
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
