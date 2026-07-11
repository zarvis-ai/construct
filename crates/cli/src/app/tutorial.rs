//! Interactive tutorial ("tour") of the construct TUI (spec 0077).
//!
//! A small floating coach-mark card that tells the user what to do,
//! observes the *real* [`KeyAction`] dispatch and daemon events, and
//! advances only when the user actually does the thing. The tour never
//! simulates a fake screen and never steals input — every affordance it
//! mentions is a real, currently-live control; clicking a key label (except
//! in step 1, which teaches real keystrokes) dispatches the exact same
//! [`KeyAction`] a keypress would.
//!
//! All tour *logic* lives here. The call sites sprinkled through `app.rs`
//! (action dispatch, notification handling, minibuffer submit, PTY-forward,
//! the render tick) are one-line hooks that hand control straight back to
//! this module — see each hook's doc comment for why it exists.

use super::*;

/// Number of steps in the tour (spec 0077).
pub const STEP_COUNT: u8 = 9;

/// How long step 5/6 waits for progress before showing the stall hint.
const STALL_TIMEOUT: Duration = Duration::from_secs(60);

/// Sub-phase of step 1's two real-keystroke micro-exercises. Step 1 is the
/// only step whose key labels are not click-advance (spec 0077) — it's
/// teaching fingers, not testing recognition — so this phase machine is
/// driven entirely by [`App::tutorial_observe_key_result`], not by
/// [`App::tutorial_observe_action`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Step1Phase {
    /// Exercise A, part 1: waiting for a bare `Ctrl+X` press so the modeline
    /// echoes the pending chord.
    AwaitCtrlX,
    /// Exercise A, part 2: `C-x` is pending; waiting for `Ctrl+G` to cancel it.
    AwaitCtrlG,
    /// Exercise B: waiting for the real "create session" chord, which both
    /// opens the picker and completes step 1.
    AwaitNewSession,
}

/// Which existing pane (if any) the current step is teaching, so its border
/// can borrow the focused/accent style as a highlight — see
/// [`App::tutorial_wants_list_highlight`] and friends, consumed from
/// `ui.rs`. Reuses `pane_border_style` rather than inventing new styling;
/// deliberately does not cover the modeline (not a bordered pane).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TutorialTarget {
    None,
    List,
    View,
    Program,
}

/// One line of tour-card text: a run of segments, each either plain prose
/// or a clickable key label carrying the [`KeyAction`] a click should
/// dispatch. Built directly as spans (rather than composed strings with
/// substring search) so the renderer can size and hit-test each label
/// exactly, the same way the welcome card and modeline hints do.
pub type TutorialSegment = (String, Option<KeyAction>);
pub type TutorialLine = Vec<TutorialSegment>;

fn t(s: impl Into<String>) -> TutorialSegment {
    (s.into(), None)
}

fn k(s: impl Into<String>, action: KeyAction) -> TutorialSegment {
    (s.into(), Some(action))
}

/// Chord label for an action, in the given keymap profile. Not a generic
/// reverse lookup over `keymap::default_for` — cross-checked by hand
/// against `keymap::emacs()`/`keymap::vim()` for exactly the actions this
/// tour references, so vim's idiomatic alternate binding (`C-w s` over
/// `C-x 2`, etc.) can be shown where one exists. Falls back to the emacs
/// chord when an action has no distinct vim form (most `C-x`-prefixed
/// chords are bound identically in both profiles).
fn chord_label(action: KeyAction, profile: Profile) -> &'static str {
    use KeyAction::*;
    use Profile::*;
    match (action, profile) {
        (OpenNewSession, Emacs) => "C-x C-f",
        (OpenNewSession, Vim) => "o",
        (SwitchFocus, _) => "C-x o",
        (OpenProgram, _) => "C-x SPC",
        (RunProgram, _) => "C-x C-r",
        (SplitWindowBelow, Emacs) => "C-x 2",
        (SplitWindowBelow, Vim) => "C-w s",
        (SplitWindowRight, Emacs) => "C-x 3",
        (SplitWindowRight, Vim) => "C-w v",
        (DeleteWindow, Emacs) => "C-x 0",
        (DeleteWindow, Vim) => "C-w c",
        (DeleteOtherWindows, Emacs) => "C-x 1",
        (DeleteOtherWindows, Vim) => "C-w o",
        (ToggleHelp, _) => "?",
        (Quit, _) => "C-x C-c",
        (OpenDeleteConfirm, Emacs) => "C-x k",
        (OpenDeleteConfirm, Vim) => "dd",
        (NextSession, Emacs) => "C-n",
        (NextSession, Vim) => "j",
        (PrevSession, Emacs) => "C-p",
        (PrevSession, Vim) => "k",
        _ => "?",
    }
}

/// Facts about the live app the card's copy adapts to but the tour state
/// doesn't own — computed fresh each render (and in tests) rather than
/// cached on [`TutorialState`], so the card can never show stale context.
#[derive(Debug, Clone, Copy, Default)]
pub struct TutorialCardCtx {
    /// At most one selectable session in the list. Step 4's
    /// selection exercise has nothing to visibly move to, so the card says
    /// so honestly instead of looking broken.
    pub single_session: bool,
    /// The step-6 subagent has been observed *and* is still visible in the
    /// session list (not yet archived by the board's rule). Drives step 6's
    /// waiting-line → select-instruction swap.
    pub subagent_listed: bool,
    /// Keyboard focus is on the session list. Step 7's bare-`?` instruction
    /// only works from there; with focus in the view pane / program editor
    /// the card shows an explicit "focus first" hint instead.
    pub list_focused: bool,
}

#[derive(Debug, Clone)]
pub struct TutorialState {
    pub step: u8,
    /// Set once step 8's completion condition fires (or `[end tour]` is
    /// clicked from the final step); the card shows a short "tour complete"
    /// message until the user closes it.
    pub completed: bool,
    /// True when `open_configure_popup`'s harness probe found no usable
    /// agent harness at start time (mirrors `no_agent_harness_available`).
    /// Steps 2/3 fall back to shell, step 5 becomes editing-only, step 6
    /// becomes a plain split exercise.
    pub degraded: bool,
    pub profile: Profile,
    /// The session created in step 2, remembered as the practice session
    /// for the rest of the tour.
    pub practice_session_id: Option<String>,
    /// The real fork created in the dedicated fork/merge lesson.
    pub fork_session_id: Option<String>,
    /// The subagent the Tasks template's rule spawns in step 5/6.
    pub subagent_session_id: Option<String>,
    /// Live feedback line: last keystroke echo, a wrong-key correction, or
    /// the stall hint. Cleared on every step advance.
    pub feedback: Option<String>,
    pub step1_phase: Step1Phase,
    // Step 4 sub-checks.
    pub focus_switched: bool,
    pub selection_moved: bool,
    // Step 5 sub-checks.
    pub program_opened: bool,
    pub template_applied: bool,
    pub task_line_present: bool,
    pub run_started: bool,
    // Step 6 sub-checks. Checklist rows are USER actions only (split, hop,
    // close) — what the AGENT does (spawning the subagent, moving the task
    // to Done) is reported by the card's status line and still gates
    // completion via `task_done`, but never renders as a checkbox: a box
    // the user can't tick by doing something themselves reads as broken.
    pub split_done: bool,
    /// The user hopped between panes: any `FocusWindow*` action
    /// (`C-x <arrow>` in both profiles, `C-w h/j/k/l` in vim), or a click
    /// on the card's hop label (which dispatches the same action).
    pub hop_done: bool,
    pub task_done: bool,
    /// Step 6's wrap-up: the user collapsed the split back to one pane,
    /// via either `DeleteWindow` (close this pane) or `DeleteOtherWindows`
    /// (keep only this one) — both end in a single-pane layout, so either
    /// chord counts.
    pub collapse_done: bool,
    /// Last time any sub-check advanced, driving the step 5/6 stall hint.
    pub last_progress_at: Instant,
    pub stalled: bool,
}

impl TutorialState {
    pub fn start(degraded: bool, profile: Profile) -> Self {
        Self::at_step(1, degraded, profile)
    }

    /// Resume an interrupted tour at a persisted step (`TuiState::tutorial_step`).
    pub fn resume(step: u8, degraded: bool, profile: Profile) -> Self {
        Self::at_step(step.clamp(1, STEP_COUNT), degraded, profile)
    }

    fn at_step(step: u8, degraded: bool, profile: Profile) -> Self {
        Self {
            step,
            completed: false,
            degraded,
            profile,
            practice_session_id: None,
            fork_session_id: None,
            subagent_session_id: None,
            feedback: None,
            step1_phase: Step1Phase::AwaitCtrlX,
            focus_switched: false,
            selection_moved: false,
            program_opened: false,
            template_applied: false,
            task_line_present: false,
            run_started: false,
            split_done: false,
            hop_done: false,
            task_done: false,
            collapse_done: false,
            last_progress_at: Instant::now(),
            stalled: false,
        }
    }

    fn target(&self) -> TutorialTarget {
        match self.step {
            2 | 3 => TutorialTarget::View,
            4 => TutorialTarget::View,
            5 => TutorialTarget::List,
            6 | 7 => TutorialTarget::Program,
            _ => TutorialTarget::None,
        }
    }

    pub fn wants(&self, target: TutorialTarget) -> bool {
        !self.completed && self.target() == target
    }

    fn touch_progress(&mut self) {
        self.last_progress_at = Instant::now();
        self.stalled = false;
    }

    fn advance(&mut self, step: u8) {
        self.step = step;
        self.step1_phase = Step1Phase::AwaitCtrlX;
        self.feedback = None;
        self.touch_progress();
    }

    fn finish(&mut self) {
        self.completed = true;
        self.feedback = None;
    }

    /// Steps 4/5/6 gate on more than one sub-check; called after any of
    /// them changes so the step advances the moment the *last* one lands,
    /// regardless of which hook produced it.
    fn recompute_completion(&mut self) {
        if self.completed {
            return;
        }
        match self.step {
            5 if self.focus_switched && self.selection_moved => self.advance(6),
            6 => {
                let ready = if self.degraded {
                    self.program_opened && self.template_applied && self.task_line_present
                } else {
                    self.run_started
                };
                if ready {
                    self.advance(7);
                }
            }
            7 => {
                // Both modes require the three USER actions (split, hop,
                // close the split — the checklist rows); non-degraded
                // additionally waits for the agent's part, the delegated
                // task landing in ## Done (the status-line gate).
                let user_done = self.split_done && self.hop_done && self.collapse_done;
                let ready = if self.degraded {
                    user_done
                } else {
                    user_done && self.task_done
                };
                if ready {
                    self.advance(8);
                }
            }
            _ => {}
        }
    }

    /// Footer `[prev step]`: purely navigational. Decrements the step and
    /// re-arms the target step's transient progress flags (checklist
    /// booleans, the step-1 phase, feedback) so the step can be
    /// demonstrated again. Never undoes real-world effects — sessions,
    /// program contents, and remembered facts (the practice session, an
    /// already-spawned subagent) stay as they are, so a re-entered step
    /// whose real-world condition already holds simply completes again on
    /// the next observation, or can be stepped past with `[next step]`.
    pub fn step_back(&mut self) {
        if self.completed || self.step <= 1 {
            return;
        }
        let prev = self.step - 1;
        match prev {
            5 => {
                self.focus_switched = false;
                self.selection_moved = false;
            }
            6 => {
                self.program_opened = false;
                self.template_applied = false;
                self.task_line_present = false;
                self.run_started = false;
            }
            7 => {
                self.split_done = false;
                self.hop_done = false;
                self.task_done = false;
                self.collapse_done = false;
            }
            _ => {}
        }
        self.advance(prev);
    }

    pub fn tick(&mut self, now: Instant) {
        if self.completed || self.stalled {
            return;
        }
        if matches!(self.step, 6 | 7) && now.duration_since(self.last_progress_at) > STALL_TIMEOUT {
            self.stalled = true;
            self.feedback =
                Some("the agent is taking a while — you can keep waiting or skip ahead".into());
        }
    }

    pub fn card_title(&self) -> String {
        if self.completed {
            " tour complete ".to_string()
        } else {
            format!(" tour {}/{} ", self.step, STEP_COUNT)
        }
    }

    /// Body content for the card. Step 1's key labels carry
    /// [`KeyAction::TutorialNudge`] instead of the real action — clicking
    /// them nudges rather than dispatches, since that step teaches real
    /// keystrokes. `ctx` carries live-app facts (focus, list contents) the
    /// copy adapts to; see [`TutorialCardCtx`].
    pub fn lines(&self, ctx: TutorialCardCtx) -> Vec<TutorialLine> {
        if self.completed {
            return vec![
                vec![t("Tour complete! Nice work covering the core")],
                vec![t("keybindings and the program board.")],
                vec![],
                vec![t("Replay anytime: palette -> tutorial.")],
            ];
        }
        match self.step {
            1 => step1_lines(self.step1_phase, self.profile),
            2 => step2_lines(self.degraded),
            3 => step3_lines(),
            4 => step4_lines(),
            5 => step5_lines(self.profile, ctx.single_session),
            6 => step6_lines(self.profile, self.degraded),
            7 => step7_lines(self, ctx),
            8 => step8_lines(self.profile, ctx.list_focused),
            9 => step9_lines(self.profile),
            _ => Vec::new(),
        }
    }

    /// Mini-checklist for the multi-part steps (4, 5, 6). Empty elsewhere.
    pub fn checklist(&self) -> Vec<(String, bool)> {
        match self.step {
            5 => vec![
                (
                    "switch focus (list <-> view)".to_string(),
                    self.focus_switched,
                ),
                ("move the selection".to_string(), self.selection_moved),
            ],
            6 => {
                let mut items = vec![
                    ("open the program".to_string(), self.program_opened),
                    (
                        "apply the Tasks template".to_string(),
                        self.template_applied,
                    ),
                    ("add a task under Todo".to_string(), self.task_line_present),
                ];
                if !self.degraded {
                    items.push(("run it".to_string(), self.run_started));
                }
                items
            }
            7 => {
                // USER actions only, in the order the user performs them.
                // The agent's side (subagent spawning, the task reaching
                // ## Done) is narrated by the card's status line instead —
                // a checkbox the user can't tick themselves reads as
                // broken. Identical rows in both modes.
                vec![
                    ("split the pane".to_string(), self.split_done),
                    ("hop panes".to_string(), self.hop_done),
                    ("close the split".to_string(), self.collapse_done),
                ]
            }
            _ => Vec::new(),
        }
    }

    pub fn footer(&self) -> TutorialLine {
        if self.completed {
            return vec![k("[close]", KeyAction::TutorialEndTour)];
        }
        // `[prev step]  [next step]  [end tour]` = 36 cols incl. the two
        // 2-space gaps — fits the card's 44-col inner width. `[prev step]`
        // is hidden on step 1 (nothing to go back to).
        let mut segs = Vec::with_capacity(5);
        if self.step > 1 {
            segs.push(k("[prev step]", KeyAction::TutorialPrevStep));
            segs.push(t("  "));
        }
        segs.push(k("[next step]", KeyAction::TutorialNextStep));
        segs.push(t("  "));
        segs.push(k("[end tour]", KeyAction::TutorialEndTour));
        segs
    }
}

// Body lines are authored to fit the card's inner width (44 cols, spec
// 0077's "roughly 46 wide") on a single row each — they are intentionally
// NOT wrapped at render time, so a HintZone's column math stays exact for
// the labels embedded mid-line. Keep new/edited lines at 44 cols or under.

fn step1_lines(phase: Step1Phase, profile: Profile) -> Vec<TutorialLine> {
    let mut lines = vec![
        vec![t("A chord like C-x C-f: hold Ctrl, tap X,")],
        vec![t("let go, then (still holding Ctrl) tap F.")],
        vec![],
    ];
    match phase {
        Step1Phase::AwaitCtrlX => {
            lines.push(vec![
                t("Try it, press "),
                k("Ctrl+X", KeyAction::TutorialNudge),
                t(" now."),
            ]);
        }
        Step1Phase::AwaitCtrlG => {
            lines.push(vec![
                t("Pending! Now press "),
                k("Ctrl+G", KeyAction::TutorialNudge),
                t(" to cancel it."),
            ]);
            lines.push(vec![t("C-g backs out of anything half-typed:")]);
            lines.push(vec![t("chords, prompts, pickers.")]);
        }
        Step1Phase::AwaitNewSession => {
            lines.push(vec![
                t("Cancelled! For real: press "),
                k(
                    chord_label(KeyAction::OpenNewSession, profile),
                    KeyAction::TutorialNudge,
                ),
            ]);
            lines.push(vec![t("to create a session.")]);
        }
    }
    lines
}

fn step2_lines(degraded: bool) -> Vec<TutorialLine> {
    if degraded {
        vec![
            vec![t("The picker is open. No agent harness yet,")],
            vec![t("so pick shell for now.")],
            vec![],
            vec![t("(Set one up in /configure to see")],
            vec![t("delegation live later.)")],
        ]
    } else {
        vec![
            vec![t("The picker is open. Type an agent harness")],
            vec![t("(not shell) and press Enter — step 5 needs")],
            vec![t("an agent behind this session.")],
        ]
    }
}

fn step3_lines() -> Vec<TutorialLine> {
    vec![
        vec![
            t("Focus it — click it, or press "),
            k("Enter", KeyAction::FocusView),
        ],
        vec![t("then type a short message and")],
        vec![t("press Enter to send it.")],
    ]
}

// Ordered on purpose: after step 3 the keyboard focus is in the view pane
// (the user just typed into the practice session), where a bare `C-n` is
// forwarded straight into the child's PTY and never resolves to
// `NextSession`. The tour never steals keys from the PTY, so the card
// teaches the working order instead: focus the list first, then move.
fn step4_lines() -> Vec<TutorialLine> {
    vec![
        vec![t("Open the practice session's title-bar ☰")],
        vec![t("menu and choose Fork conversation." )],
        vec![t("The fork keeps its context but works on an")],
        vec![t("independent path. Create one now.")],
        vec![],
        vec![t("On the fork, ☰ enables merge and archive:")],
        vec![t("send its result back and archive the fork.")],
        vec![t("Or C-x k then m for the same action.")],
    ]
}

fn step5_lines(profile: Profile, single_session: bool) -> Vec<TutorialLine> {
    let mut lines = vec![
        vec![
            t("1. "),
            k(
                chord_label(KeyAction::SwitchFocus, profile),
                KeyAction::SwitchFocus,
            ),
            t(" — jump to the list."),
        ],
        vec![
            t("2. "),
            k(
                chord_label(KeyAction::NextSession, profile),
                KeyAction::NextSession,
            ),
            t(" / "),
            k(
                chord_label(KeyAction::PrevSession, profile),
                KeyAction::PrevSession,
            ),
            t(" — move the selection."),
        ],
    ];
    if single_session {
        // Honest note: the move still counts (the sub-check ticks on the
        // action firing), but with a single session nothing visibly moves.
        lines.push(vec![]);
        lines.push(vec![t("(one session now, so it stays put —")]);
        lines.push(vec![t("matters once you have a fleet)")]);
    }
    lines
}

fn step6_lines(profile: Profile, degraded: bool) -> Vec<TutorialLine> {
    let mut lines = vec![
        vec![
            k(
                chord_label(KeyAction::OpenProgram, profile),
                KeyAction::OpenProgram,
            ),
            t(" opens the program board."),
        ],
        vec![t("Pick the built-in \"Tasks\" template, then")],
        vec![t("type \"- Test task\" under ## Todo.")],
    ];
    if degraded {
        lines.push(vec![t("No agent harness set up, so this step")]);
        lines.push(vec![t("is editing-only — nothing to run yet.")]);
    } else {
        lines.push(vec![
            k(
                chord_label(KeyAction::RunProgram, profile),
                KeyAction::RunProgram,
            ),
            t(" runs it, moves task to In Progress,"),
        ]);
        lines.push(vec![t("and hands it to a subagent.")]);
    }
    lines
}

/// The either-or split line both modes open with: `C-x 2` and `C-x 3` are
/// equal choices, both clickable, and the split sub-check ticks on either
/// action firing.
fn step7_split_line(profile: Profile) -> TutorialLine {
    vec![
        t("Split: "),
        k(
            chord_label(KeyAction::SplitWindowBelow, profile),
            KeyAction::SplitWindowBelow,
        ),
        t(" (below) or "),
        k(
            chord_label(KeyAction::SplitWindowRight, profile),
            KeyAction::SplitWindowRight,
        ),
        t(" (right)."),
    ]
}

/// Step 6's card separates what the USER does (split now, close the split
/// at the end) from what the AGENT is doing, and adapts its middle lines to
/// the live run state: a subagent can take a while to spawn (the card says
/// so instead of pointing at a session that doesn't exist yet), and on a
/// fast model the whole run may already have finished — possibly with the
/// subagent archived by the board's rule — before the user even splits (the
/// card acknowledges that instead of pointing at a vanished session).
fn step7_lines(state: &TutorialState, ctx: TutorialCardCtx) -> Vec<TutorialLine> {
    let profile = state.profile;
    // Clickable hop label: dispatches a real pane-hop (and ticks the hop
    // sub-check) so a mouse-only user can complete the row. `C-x <arrow>`
    // is bound in both profiles.
    let hop = || k("C-x <arrow>", KeyAction::FocusWindowDown);
    if state.degraded {
        return vec![
            step7_split_line(profile),
            vec![t("Hop panes with "), hop(), t(", then close")],
            vec![
                t("the split: "),
                k(
                    chord_label(KeyAction::DeleteWindow, profile),
                    KeyAction::DeleteWindow,
                ),
                t(" closes this pane,"),
            ],
            vec![
                k(
                    chord_label(KeyAction::DeleteOtherWindows, profile),
                    KeyAction::DeleteOtherWindows,
                ),
                t(" keeps only this one."),
            ],
        ];
    }
    let mut lines = vec![
        step7_split_line(profile),
        vec![t("Hop panes with "), hop(), t(".")],
        vec![],
    ];
    // `run_finished` covers both fast-model inverses: the task already sits
    // under ## Done, and/or the spawned subagent has already been archived
    // by the board's rule (observed once, no longer listed).
    let run_finished =
        state.task_done || (state.subagent_session_id.is_some() && !ctx.subagent_listed);
    if run_finished {
        lines.push(vec![t("The task is Done! Close the split:")]);
        lines.push(vec![
            k(
                chord_label(KeyAction::DeleteWindow, profile),
                KeyAction::DeleteWindow,
            ),
            t(" closes this pane; "),
            k(
                chord_label(KeyAction::DeleteOtherWindows, profile),
                KeyAction::DeleteOtherWindows,
            ),
            t(" keeps"),
        ]);
        lines.push(vec![t("only this pane open.")]);
    } else if ctx.subagent_listed {
        lines.push(vec![t("Subagent's up! Select it in the other")]);
        lines.push(vec![t("pane — it's nested under this session.")]);
    } else {
        lines.push(vec![t("⧗ the agent is spawning a subagent —")]);
        lines.push(vec![t("can take a minute depending on model.")]);
    }
    lines
}

// Ordered like step 4, and for the same reason: a bare `?` only resolves
// to `ToggleHelp` from list focus — with focus in the session terminal or
// the program editor it is (correctly) just typed into that surface. The
// tour never steals the key; the card teaches the order and, while focus
// is elsewhere, says so explicitly. Clicking the `?` label works from any
// focus (it dispatches the action directly).
fn step8_lines(profile: Profile, list_focused: bool) -> Vec<TutorialLine> {
    let mut lines = vec![
        vec![
            t("1. "),
            k(
                chord_label(KeyAction::SwitchFocus, profile),
                KeyAction::SwitchFocus,
            ),
            t(" — hop to the list, then"),
        ],
        vec![
            t("2. "),
            k(
                chord_label(KeyAction::ToggleHelp, profile),
                KeyAction::ToggleHelp,
            ),
            t(" — open help (any key closes)."),
        ],
    ];
    if !list_focused {
        lines.push(vec![t("(focus is in the view now, so ? would")]);
        lines.push(vec![t("just be typed there — C-x o first)")]);
    }
    lines.push(vec![t(format!(
        "{} quits — don't press it now!",
        chord_label(KeyAction::Quit, profile)
    ))]);
    lines.push(vec![t("(C-g still backs out, as in step 1.)")]);
    lines.push(vec![]);
    lines.push(vec![k("[got it]", KeyAction::TutorialNextStep)]);
    lines
}

fn step9_lines(profile: Profile) -> Vec<TutorialLine> {
    vec![
        vec![
            k(
                chord_label(KeyAction::OpenDeleteConfirm, profile),
                KeyAction::OpenDeleteConfirm,
            ),
            t(" opens delete for this session."),
        ],
        vec![t("The subagent was already archived by")],
        vec![t("the board's own rule.")],
        vec![t("Confirm the delete to finish the tour.")],
    ]
}

/// Non-empty lines under a `## <heading>` section of a program's Markdown.
/// Deliberately simple (no nested-list awareness) — the Tasks template's
/// three top-level sections never nest.
fn section_lines<'a>(markdown: &'a str, heading: &str) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut in_section = false;
    for line in markdown.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("## ") {
            in_section = rest.trim() == heading;
            continue;
        }
        if in_section && !trimmed.is_empty() {
            out.push(line);
        }
    }
    out
}

fn todo_section_has_task(markdown: &str) -> bool {
    !section_lines(markdown, "Todo").is_empty()
}

fn done_section_has_task(markdown: &str) -> bool {
    !section_lines(markdown, "Done").is_empty()
}

/// Content-shaped detection of the Tasks board: all three of its section
/// headings present. Deliberately independent of `template_id` so a user
/// who types the sections by hand passes step 5's "apply the template"
/// check the same as one who clicked the template button.
fn has_tasks_board_sections(markdown: &str) -> bool {
    let has_heading = |name: &str| {
        markdown
            .lines()
            .any(|line| line.trim().strip_prefix("## ").map(str::trim) == Some(name))
    };
    has_heading("Todo") && has_heading("In Progress") && has_heading("Done")
}

impl App {
    /// Entry point (1 of 2): bare `t` in the welcome card / palette command
    /// `tutorial`. A no-op while a tour is already active (spec 0077); the
    /// second entry point (`t`/`tutorial`) is registered as a `HintZone` and
    /// a `run_slash_command` arm, both of which route through
    /// [`Self::run_action`] like any other `KeyAction`.
    pub fn tutorial_start(&mut self) {
        if self.tutorial.is_some() {
            return;
        }
        let degraded = no_agent_harness_available(&self.harnesses);
        self.tutorial = Some(TutorialState::start(degraded, self.profile));
    }

    /// Resume a tour interrupted by a previous quit (`TuiState::tutorial_step`).
    pub fn tutorial_resume(&mut self, step: u8) {
        if self.tutorial.is_some() {
            return;
        }
        let degraded = no_agent_harness_available(&self.harnesses);
        self.tutorial = Some(TutorialState::resume(step, degraded, self.profile));
    }

    /// Footer `[next step]` (and step 7's `[got it]`): advance without
    /// completing the current step's condition.
    pub fn tutorial_next_step(&mut self) {
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed {
            self.tutorial = None;
            return;
        }
        if t.step >= STEP_COUNT {
            t.finish();
            crate::tui_state::mark_tutorial_done();
            return;
        }
        let next = t.step + 1;
        t.advance(next);
    }

    /// Footer `[prev step]`: see [`TutorialState::step_back`]. Hidden on
    /// step 1 and on the completed card; a stray dispatch there is a no-op.
    pub fn tutorial_prev_step(&mut self) {
        if let Some(t) = self.tutorial.as_mut() {
            t.step_back();
        }
    }

    /// Live-app facts the card copy adapts to; computed at render time (and
    /// directly in tests) so it can never go stale. See [`TutorialCardCtx`].
    pub fn tutorial_card_ctx(&self) -> TutorialCardCtx {
        let selectable = self
            .sessions
            .iter()
            .filter(|s| s.kind != construct_protocol::SessionKind::Orchestrator && !s.archived)
            .count();
        let subagent_listed = self
            .tutorial
            .as_ref()
            .and_then(|t| t.subagent_session_id.as_deref())
            .is_some_and(|id| self.sessions.iter().any(|s| s.id == id && !s.archived));
        TutorialCardCtx {
            single_session: selectable <= 1,
            subagent_listed,
            list_focused: self.focus == PaneFocus::List,
        }
    }

    /// Closes the tour. Writes the done marker only when invoked from the
    /// final step (or after it already completed) — an early `[end tour]`
    /// leaves the marker absent so the welcome card keeps inviting the user
    /// back (spec 0077).
    pub fn tutorial_end_tour(&mut self) {
        let Some(t) = self.tutorial.as_ref() else {
            return;
        };
        if t.completed || t.step == STEP_COUNT {
            crate::tui_state::mark_tutorial_done();
        }
        self.tutorial = None;
    }

    /// Step 1's key labels are click-only nudges, not click-advance —
    /// see [`TutorialState::lines`].
    pub fn tutorial_nudge(&mut self) {
        if let Some(t) = self.tutorial.as_mut() {
            if !t.completed && t.step == 1 {
                t.feedback =
                    Some("this one's for your fingers — try pressing the real keys".into());
            }
        }
    }

    /// Hook (a): called from the top of [`Self::run_action`], the single
    /// chokepoint every `KeyAction` dispatch passes through — keyboard
    /// chords, palette commands, and `HintZone` clicks alike (a click
    /// resolves to the exact same `run_action` call a keypress would, so
    /// this hook covers "every step clickable" for free).
    pub fn tutorial_observe_action(&mut self, action: KeyAction) {
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed {
            return;
        }
        match t.step {
            1 => {
                if t.step1_phase == Step1Phase::AwaitNewSession {
                    if action == KeyAction::OpenNewSession {
                        t.advance(2);
                    } else if !matches!(
                        action,
                        KeyAction::TutorialNudge
                            | KeyAction::TutorialNextStep
                            | KeyAction::TutorialPrevStep
                            | KeyAction::TutorialEndTour
                    ) {
                        let label = chord_label(KeyAction::OpenNewSession, t.profile);
                        t.feedback = Some(format!(
                            "that ran something else — press C-g and try {label} again"
                        ));
                    }
                }
            }
            5 => match action {
                KeyAction::SwitchFocus => {
                    t.focus_switched = true;
                    t.touch_progress();
                }
                KeyAction::NextSession | KeyAction::PrevSession => {
                    t.selection_moved = true;
                    t.touch_progress();
                }
                _ => {}
            },
            6 => match action {
                KeyAction::OpenProgram => {
                    t.program_opened = true;
                    t.touch_progress();
                }
                KeyAction::RunProgram => {
                    t.run_started = true;
                    t.touch_progress();
                }
                _ => {}
            },
            7 => match action {
                // Either split direction counts — the card offers the two
                // chords as an explicit either-or choice.
                KeyAction::SplitWindowBelow | KeyAction::SplitWindowRight => {
                    t.split_done = true;
                    t.touch_progress();
                }
                // Any direction of pane-hopping counts; the card's hop
                // label is clickable and dispatches one of these, so a
                // mouse-only user can tick it too.
                KeyAction::FocusWindowUp
                | KeyAction::FocusWindowDown
                | KeyAction::FocusWindowLeft
                | KeyAction::FocusWindowRight => {
                    t.hop_done = true;
                    t.touch_progress();
                }
                // The wrap-up: either way of collapsing back to one pane
                // counts (close this pane / keep only this one). Ticks on
                // the action firing, layout-independent, so it can't wedge
                // even if the user never actually split.
                KeyAction::DeleteWindow | KeyAction::DeleteOtherWindows => {
                    t.collapse_done = true;
                    t.touch_progress();
                }
                _ => {}
            },
            8 => {
                if action == KeyAction::ToggleHelp {
                    t.advance(9);
                }
            }
            _ => {}
        }
        if let Some(t) = self.tutorial.as_mut() {
            t.recompute_completion();
        }
    }

    /// Hook: wrong-key / pending-chord feedback for step 1, which — unlike
    /// every other step — is gated on literal keystrokes rather than a
    /// resolved `KeyAction` (`C-x` alone, and `C-g`, are not bound to any
    /// action). Called from the single top-level chord dispatch site in
    /// `on_key` whenever the keymap resolves a `KeymapResult` there.
    pub fn tutorial_observe_key_result(&mut self, res: &KeymapResult, key: KeyEvent) {
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed || t.step != 1 {
            return;
        }
        let is_ctrl_x =
            matches!(key.code, KeyCode::Char('x')) && key.modifiers == KeyModifiers::CONTROL;
        let is_ctrl_g =
            matches!(key.code, KeyCode::Char('g')) && key.modifiers == KeyModifiers::CONTROL;
        match t.step1_phase {
            Step1Phase::AwaitCtrlX => {
                if is_ctrl_x && matches!(res, KeymapResult::Pending(_)) {
                    t.feedback = Some("pending: C-x — now press Ctrl+G to cancel it".into());
                    t.step1_phase = Step1Phase::AwaitCtrlG;
                    t.touch_progress();
                } else if matches!(res, KeymapResult::Unhandled) {
                    t.feedback = Some(format!(
                        "you pressed {} — hold Ctrl and tap X (C-x) to start",
                        crate::keymap::format_key(&key)
                    ));
                }
            }
            Step1Phase::AwaitCtrlG => {
                if is_ctrl_g {
                    t.feedback = Some("cancelled — C-g backs out of anything half-typed.".into());
                    t.step1_phase = Step1Phase::AwaitNewSession;
                    t.touch_progress();
                } else if matches!(res, KeymapResult::Unhandled) {
                    t.feedback = Some(format!(
                        "you pressed {} — the key to reach for is C-g",
                        crate::keymap::format_key(&key)
                    ));
                }
            }
            Step1Phase::AwaitNewSession => {
                if matches!(res, KeymapResult::Unhandled) {
                    t.feedback = Some(format!(
                        "you pressed {} — try {}",
                        crate::keymap::format_key(&key),
                        chord_label(KeyAction::OpenNewSession, t.profile)
                    ));
                }
            }
        }
    }

    /// Hook (b): called from the top of [`Self::on_notification`] — the
    /// single place daemon-pushed state changes are applied. Re-parses the
    /// same payload shapes `on_notification` parses further down (cheap at
    /// UI event rates) so this stays the one call site, with all step logic
    /// living in this module. Covers "session created" (STATE, including a
    /// subagent's own STATE push), "program document updated"
    /// (PROGRAM_STATE), and the practice session's own deletion (DELETED).
    pub fn tutorial_observe_notification(&mut self, n: &Notification) {
        if self.tutorial.is_none() {
            return;
        }
        let Some(params) = n.params.clone() else {
            return;
        };
        if n.method == construct_protocol::ipc_notif::STATE {
            if let Ok(payload) = serde_json::from_value::<StateNotificationPayload>(params) {
                let is_new = !self.sessions.iter().any(|s| s.id == payload.session.id);
                if is_new {
                    self.tutorial_on_session_created(&payload.session);
                }
            }
        } else if n.method == construct_protocol::ipc_notif::PROGRAM_STATE {
            if let Ok(payload) =
                serde_json::from_value::<construct_protocol::ProgramStateNotificationPayload>(params)
            {
                self.tutorial_on_program_state(&payload.program);
            }
        } else if n.method == construct_protocol::ipc_notif::DELETED {
            if let Ok(payload) =
                serde_json::from_value::<construct_protocol::DeletedNotificationPayload>(params)
            {
                self.tutorial_on_session_deleted(&payload.session_id);
            }
        }
    }

    fn tutorial_on_session_created(&mut self, session: &SessionSummary) {
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed {
            return;
        }
        match t.step {
            2 => {
                let is_shell = session.harness == "shell";
                if t.degraded || !is_shell {
                    t.practice_session_id = Some(session.id.clone());
                    t.advance(3);
                } else {
                    t.feedback = Some(
                        "that created a shell session — pick an agent harness so step 5 \
                         can run for real ([next step] to continue anyway)"
                            .to_string(),
                    );
                }
            }
            4 => {
                if session
                    .forked_from
                    .as_ref()
                    .is_some_and(|fork| Some(fork.session_id.as_str()) == t.practice_session_id.as_deref())
                {
                    t.fork_session_id = Some(session.id.clone());
                    t.advance(5);
                }
            }
            7 => {
                // `construct_subagent_create` (the MCP tool the practice
                // agent uses) and the board's direct dispatch both stamp
                // `parent_session_id` with the creating session and kind
                // Subagent. Match on parentage when the practice session is
                // known; when it ISN'T (tour resumed after a TUI restart,
                // or the user navigated here with [next step] without doing
                // step 2 — `practice_session_id` is never persisted), any
                // subagent spawning during step 6 is almost surely the
                // board's doing, so accept it and ADOPT its parent as the
                // practice session — that repairs the rest of the tour's
                // scoped observers (Done detection, step 8's delete gate).
                let is_tour_subagent = match t.practice_session_id.as_deref() {
                    Some(practice) => session.parent_session_id.as_deref() == Some(practice),
                    None => session.kind == construct_protocol::SessionKind::Subagent,
                };
                if is_tour_subagent {
                    if t.practice_session_id.is_none() {
                        t.practice_session_id = session.parent_session_id.clone();
                    }
                    t.subagent_session_id = Some(session.id.clone());
                    t.touch_progress();
                }
            }
            _ => {}
        }
    }

    fn tutorial_on_program_state(&mut self, program: &construct_protocol::ProgramDocument) {
        // Computed before borrowing the tour state mutably: is this program
        // the one the user is actually looking at / working with?
        let displayed = self
            .program_popup
            .as_ref()
            .is_some_and(|p| p.program.session_id == program.session_id)
            || self.selected_id().as_deref() == Some(program.session_id.as_str());
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed || !matches!(t.step, 6 | 7) {
            return;
        }
        // Scope to the practice session when known. When it isn't (tour
        // resumed / steps skipped — `practice_session_id` is not
        // persisted), fall back to the program being displayed or the
        // selected session, and ADOPT it as the practice session so the
        // remaining scoped observers work again. Without this fallback the
        // agent's ## Done edit — which arrives ONLY through this daemon
        // event, since an open popup deliberately does not absorb daemon
        // updates into a locally-edited buffer — was silently dropped and
        // step 6 could never detect completion (user report).
        match t.practice_session_id.as_deref() {
            Some(practice) => {
                if practice != program.session_id.as_str() {
                    return;
                }
            }
            None => {
                if !displayed {
                    return;
                }
                t.practice_session_id = Some(program.session_id.clone());
            }
        }
        if program.template_id.as_deref() == Some("tasks") {
            t.template_applied = true;
        }
        if todo_section_has_task(&program.markdown) {
            t.task_line_present = true;
        }
        if done_section_has_task(&program.markdown) {
            t.task_done = true;
        }
        t.touch_progress();
        t.recompute_completion();
    }

    fn tutorial_on_session_deleted(&mut self, session_id: &str) {
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed || t.step != 9 {
            return;
        }
        if t.practice_session_id.as_deref() == Some(session_id) {
            t.finish();
            crate::tui_state::mark_tutorial_done();
        }
    }

    /// Pragmatic addition beyond the three canonical hooks: headless
    /// sessions (no PTY) send input through the minibuffer's `SendInput`
    /// intent, which is handled entirely inside `run_minibuffer_submit` and
    /// never resolves to a `KeyAction` or produces a distinguishable
    /// notification. Called from the top of that function.
    pub fn tutorial_observe_minibuffer_submit(&mut self, intent: &MinibufferIntent, input: &str) {
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed || t.step != 3 || input.trim().is_empty() {
            return;
        }
        if let MinibufferIntent::SendInput { session_id } = intent {
            if t.practice_session_id.as_deref() == Some(session_id.as_str()) {
                t.advance(4);
            }
        }
    }

    /// Pragmatic addition beyond the three canonical hooks: PTY-captured
    /// sessions (the common case — any real agent harness) receive typed
    /// input as raw forwarded keystrokes, bypassing both `run_action` and
    /// the minibuffer entirely. Called from `forward_key_to_selected_pty`
    /// only for `Enter`, so this fires once per submitted line rather than
    /// once per character.
    pub fn tutorial_observe_pty_enter(&mut self) {
        let Some(id) = self.selected_id() else {
            return;
        };
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed || t.step != 3 {
            return;
        }
        if t.practice_session_id.as_deref() == Some(id.as_str()) {
            t.advance(4);
        }
    }

    /// Client-side counterpart of [`Self::tutorial_on_program_state`] for
    /// step 5's two content-driven sub-checks. Applying the Tasks template
    /// and typing the task line are LOCAL edits to the program popup's
    /// buffer — the daemon only learns about them on save (`C-x C-s`) or
    /// run (`C-x C-r`), so waiting for its PROGRAM_STATE event left the
    /// checklist unticked while the user was actually doing the step
    /// (user report). Evaluated from the render tick — a cheap string scan
    /// gated to step 5 with unticked boxes — so the checkmarks appear as
    /// the user acts, before any save. The daemon event path stays as a
    /// second source (a program synced from another client): whichever
    /// source sees it first ticks, and ticks are sticky — later buffer
    /// edits never untick them.
    pub fn tutorial_observe_program_buffer(&mut self) {
        let Some(t) = self.tutorial.as_ref() else {
            return;
        };
        let unticked = match t.step {
            6 => !(t.template_applied && t.task_line_present),
            // Step 6's ## Done gate combines two sources with OR: the
            // daemon's program/state event (the agent's edits — the primary
            // source, see `tutorial_on_program_state`) and this buffer scan
            // (a popup whose view semantics DO refresh, or a user moving
            // the line by hand). Sticky either way.
            7 => !t.task_done,
            _ => false,
        };
        if t.completed || !unticked {
            return;
        }
        let Some(popup) = self.program_popup.as_ref() else {
            return;
        };
        // Scope to the practice session's program when we know it; a tour
        // resumed after a TUI restart loses `practice_session_id` (it is
        // not persisted), so fall back to whatever program the user is
        // actually editing rather than leaving the step un-completable.
        if let Some(practice) = t.practice_session_id.as_deref() {
            if practice != popup.program.session_id.as_str() {
                return;
            }
        }
        let template = popup.program.template_id.as_deref() == Some("tasks")
            || has_tasks_board_sections(&popup.buffer);
        let task = todo_section_has_task(&popup.buffer);
        let done = done_section_has_task(&popup.buffer);
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        let mut progressed = false;
        match t.step {
            6 => {
                if template && !t.template_applied {
                    t.template_applied = true;
                    progressed = true;
                }
                if task && !t.task_line_present {
                    t.task_line_present = true;
                    progressed = true;
                }
            }
            7 if done && !t.task_done => {
                t.task_done = true;
                progressed = true;
            }
            _ => {}
        }
        if progressed {
            t.touch_progress();
            t.recompute_completion();
        }
    }

    /// Drives the step 5/6 stall hint and the client-side program-buffer
    /// observation from the existing render tick — no dedicated timer
    /// thread.
    pub fn tutorial_tick(&mut self, now: Instant) {
        self.tutorial_observe_program_buffer();
        if let Some(t) = self.tutorial.as_mut() {
            t.tick(now);
        }
    }

    pub fn tutorial_wants_list_highlight(&self) -> bool {
        self.tutorial
            .as_ref()
            .is_some_and(|t| t.wants(TutorialTarget::List))
    }

    pub fn tutorial_wants_view_highlight(&self) -> bool {
        self.tutorial
            .as_ref()
            .is_some_and(|t| t.wants(TutorialTarget::View))
    }

    pub fn tutorial_wants_program_highlight(&self) -> bool {
        self.tutorial
            .as_ref()
            .is_some_and(|t| t.wants(TutorialTarget::Program))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn todo_and_done_sections_detect_task_lines() {
        let md = "# Rule\n\n## Todo\n\n- Test task\n\n## In Progress\n\n## Done\n";
        assert!(todo_section_has_task(md));
        assert!(!done_section_has_task(md));

        let moved =
            "# Rule\n\n## Todo\n\n## In Progress\n\n- Test task\n\n## Done\n\n- Test task\n";
        assert!(!todo_section_has_task(moved));
        assert!(done_section_has_task(moved));
    }

    #[test]
    fn tasks_board_sections_detected_by_content() {
        let template = "# Rule\n\nblurb\n\n## Todo\n\n## In Progress\n\n## Done\n";
        assert!(has_tasks_board_sections(template));
        // Hand-typed sections count the same as the template button.
        let hand_typed = "## Todo\n## In Progress\n## Done\n";
        assert!(has_tasks_board_sections(hand_typed));
        // A partial board does not.
        let partial = "# Rule\n\n## Todo\n\n## Done\n";
        assert!(!has_tasks_board_sections(partial));
        assert!(!has_tasks_board_sections(""));
    }

    #[test]
    fn degraded_step5_checklist_has_no_run_row() {
        let mut state = TutorialState::start(true, Profile::Emacs);
        state.step = 6;
        let items = state.checklist();
        assert!(!items.iter().any(|(label, _)| label == "run it"));
    }

    #[test]
    fn non_degraded_step5_checklist_has_run_row() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 6;
        let items = state.checklist();
        assert!(items.iter().any(|(label, _)| label == "run it"));
    }

    #[test]
    fn step5_completion_differs_by_degraded_mode() {
        let mut degraded = TutorialState::start(true, Profile::Emacs);
        degraded.step = 6;
        degraded.program_opened = true;
        degraded.template_applied = true;
        degraded.task_line_present = true;
        degraded.recompute_completion();
        assert_eq!(degraded.step, 7, "degraded step6 completes without a run");

        let mut normal = TutorialState::start(false, Profile::Emacs);
        normal.step = 6;
        normal.program_opened = true;
        normal.template_applied = true;
        normal.task_line_present = true;
        normal.recompute_completion();
        assert_eq!(normal.step, 6, "non-degraded step6 still needs a run");
        normal.run_started = true;
        normal.recompute_completion();
        assert_eq!(normal.step, 7);
    }

    #[test]
    fn vim_profile_uses_vim_idiomatic_labels() {
        assert_eq!(chord_label(KeyAction::OpenNewSession, Profile::Vim), "o");
        assert_eq!(
            chord_label(KeyAction::SplitWindowBelow, Profile::Vim),
            "C-w s"
        );
        assert_eq!(
            chord_label(KeyAction::OpenDeleteConfirm, Profile::Vim),
            "dd"
        );
        assert_eq!(chord_label(KeyAction::DeleteWindow, Profile::Vim), "C-w c");
        assert_eq!(
            chord_label(KeyAction::DeleteOtherWindows, Profile::Vim),
            "C-w o"
        );
    }

    fn footer_actions(state: &TutorialState) -> Vec<KeyAction> {
        state
            .footer()
            .iter()
            .filter_map(|(_, action)| *action)
            .collect()
    }

    #[test]
    fn footer_hides_prev_on_step1_and_orders_prev_next_end_after() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        assert_eq!(
            footer_actions(&state),
            vec![KeyAction::TutorialNextStep, KeyAction::TutorialEndTour],
            "step 1 has nothing to go back to"
        );
        let labels: Vec<String> = state
            .footer()
            .iter()
            .filter(|(_, a)| a.is_some())
            .map(|(s, _)| s.clone())
            .collect();
        assert_eq!(labels, vec!["[next step]", "[end tour]"]);

        state.step = 2;
        assert_eq!(
            footer_actions(&state),
            vec![
                KeyAction::TutorialPrevStep,
                KeyAction::TutorialNextStep,
                KeyAction::TutorialEndTour
            ]
        );
        // `[prev step]  [next step]  [end tour]` must fit the 44-col card.
        let width: usize = state.footer().iter().map(|(s, _)| s.chars().count()).sum();
        assert!(width <= 44, "footer overflows the card: {width} cols");

        state.completed = true;
        assert_eq!(
            footer_actions(&state),
            vec![KeyAction::TutorialEndTour],
            "the completed card keeps its single [close]"
        );
    }

    #[test]
    fn step_back_rearms_transient_flags_but_keeps_remembered_facts() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 6;
        state.practice_session_id = Some("practice".into());
        state.focus_switched = true;
        state.selection_moved = true;
        state.feedback = Some("leftover".into());

        state.step_back();
        assert_eq!(state.step, 5);
        assert!(!state.focus_switched, "step 4's sub-checks are re-armed");
        assert!(!state.selection_moved);
        assert!(state.feedback.is_none());
        assert_eq!(
            state.practice_session_id.as_deref(),
            Some("practice"),
            "remembered real-world facts are never reset"
        );

        // Prev from 7 re-arms all of step 6.
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 8;
        state.split_done = true;
        state.task_done = true;
        state.collapse_done = true;
        state.subagent_session_id = Some("sub".into());
        state.step_back();
        assert_eq!(state.step, 7);
        assert!(!state.split_done && !state.task_done && !state.collapse_done);
        assert_eq!(
            state.subagent_session_id.as_deref(),
            Some("sub"),
            "the observed subagent stays acknowledged"
        );

        // No-ops: step 1 and the completed card.
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step_back();
        assert_eq!(state.step, 1);
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 3;
        state.completed = true;
        state.step_back();
        assert_eq!(state.step, 3);
    }

    fn joined_text(lines: &[TutorialLine]) -> String {
        lines
            .iter()
            .map(|line| {
                line.iter()
                    .map(|(s, _)| s.as_str())
                    .collect::<Vec<_>>()
                    .join("")
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn step4_note_shows_only_with_a_single_session() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 5;
        let single = joined_text(&state.lines(TutorialCardCtx {
            single_session: true,
            ..Default::default()
        }));
        assert!(
            single.contains("one session now, so it stays put"),
            "single-session honesty note missing:\n{single}"
        );
        // Ordered: the focus hop is taught before the selection move.
        let jump = single.find("jump to the list").expect("step order line 1");
        let sel = single
            .find("move the selection")
            .expect("step order line 2");
        assert!(jump < sel, "C-x o must be taught before C-n/C-p");

        let multi = joined_text(&state.lines(TutorialCardCtx::default()));
        assert!(
            !multi.contains("stays put"),
            "note must disappear once a second session exists:\n{multi}"
        );
    }

    #[test]
    fn step6_lines_adapt_to_live_run_state() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 7;

        // Split offered as an explicit either-or choice, both clickable.
        let waiting_lines = state.lines(TutorialCardCtx::default());
        let clickable: Vec<KeyAction> = waiting_lines
            .iter()
            .flatten()
            .filter_map(|(_, a)| *a)
            .collect();
        assert!(clickable.contains(&KeyAction::SplitWindowBelow));
        assert!(clickable.contains(&KeyAction::SplitWindowRight));

        // No subagent yet: the card says the agent is still spawning it.
        let waiting = joined_text(&waiting_lines);
        assert!(
            waiting.contains("spawning a subagent"),
            "waiting copy missing:\n{waiting}"
        );

        // Subagent visible in the list: the card points at it.
        state.subagent_session_id = Some("sub".into());
        let listed = joined_text(&state.lines(TutorialCardCtx {
            subagent_listed: true,
            ..Default::default()
        }));
        assert!(
            listed.contains("Subagent's up!"),
            "select copy missing:\n{listed}"
        );

        // Subagent observed but already archived (fast model): acknowledge
        // the finished run instead of pointing at a vanished session, and
        // teach the collapse pair.
        let archived_lines = state.lines(TutorialCardCtx::default());
        let archived = joined_text(&archived_lines);
        assert!(
            archived.contains("The task is Done!"),
            "finished copy missing:\n{archived}"
        );
        let collapse: Vec<KeyAction> = archived_lines
            .iter()
            .flatten()
            .filter_map(|(_, a)| *a)
            .collect();
        assert!(collapse.contains(&KeyAction::DeleteWindow));
        assert!(collapse.contains(&KeyAction::DeleteOtherWindows));

        // task_done alone (subagent still listed) also reads as finished.
        state.task_done = true;
        let done = joined_text(&state.lines(TutorialCardCtx {
            subagent_listed: true,
            ..Default::default()
        }));
        assert!(done.contains("The task is Done!"));
    }

    #[test]
    fn step6_completion_requires_user_actions_in_both_modes() {
        let mut degraded = TutorialState::start(true, Profile::Emacs);
        degraded.step = 7;
        degraded.split_done = true;
        degraded.collapse_done = true;
        degraded.recompute_completion();
        assert_eq!(degraded.step, 7, "hop is part of the user gate");
        degraded.hop_done = true;
        degraded.recompute_completion();
        assert_eq!(degraded.step, 8);

        let mut normal = TutorialState::start(false, Profile::Emacs);
        normal.step = 7;
        normal.task_done = true;
        normal.recompute_completion();
        assert_eq!(normal.step, 7, "Done alone no longer completes step 7");
        normal.split_done = true;
        normal.hop_done = true;
        normal.collapse_done = true;
        normal.recompute_completion();
        assert_eq!(normal.step, 8);

        // Fast-archive inverse: the subagent was observed and already
        // archived; the Done gate (however it was detected) still
        // completes the step — nothing waits on a still-listed subagent.
        let mut fast = TutorialState::start(false, Profile::Emacs);
        fast.step = 7;
        fast.subagent_session_id = Some("sub".into());
        fast.split_done = true;
        fast.hop_done = true;
        fast.collapse_done = true;
        fast.task_done = true;
        fast.recompute_completion();
        assert_eq!(fast.step, 8);
    }

    #[test]
    fn step6_checklist_is_user_actions_only() {
        for degraded in [false, true] {
            let mut state = TutorialState::start(degraded, Profile::Emacs);
            state.step = 7;
            // Regardless of what the agent has done so far…
            state.subagent_session_id = Some("sub".into());
            state.task_done = true;
            let labels: Vec<String> = state.checklist().into_iter().map(|(l, _)| l).collect();
            assert_eq!(
                labels,
                vec!["split the pane", "hop panes", "close the split"],
                "step-6 checklist must list user actions only (degraded={degraded})"
            );
            assert!(
                !labels
                    .iter()
                    .any(|l| l.contains("subagent") || l.contains("Done")),
                "agent-driven progress must not render as a checkbox"
            );
        }
    }

    #[test]
    fn step7_shows_focus_hint_only_when_focus_is_off_the_list() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 8;
        let off_list = joined_text(&state.lines(TutorialCardCtx::default()));
        assert!(
            off_list.contains("focus is in the view now"),
            "focus hint missing while the view holds focus:\n{off_list}"
        );
        let on_list = joined_text(&state.lines(TutorialCardCtx {
            list_focused: true,
            ..Default::default()
        }));
        assert!(
            !on_list.contains("focus is in the view now"),
            "hint must disappear once the list has focus:\n{on_list}"
        );
        // The ordered copy teaches the hop before the ? key.
        let hop = on_list.find("hop to the list").expect("order line 1");
        let help = on_list.find("open help").expect("order line 2");
        assert!(hop < help);
    }

    #[test]
    fn step4_explains_fork_and_merge_from_the_session_menu() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 4;
        let text = joined_text(&state.lines(TutorialCardCtx {
            list_focused: true,
            ..Default::default()
        }));
        assert!(text.contains("Fork conversation"));
        assert!(text.contains("merge and archive"));
        assert!(text.contains("C-x k"));
        assert!(text.contains("title-bar ☰"));
    }

    // Card body lines are authored to fit the 44-col inner width without
    // render-time wrapping (see the comment above `step1_lines`). Sweep
    // every step in every mode/profile/state combination so a future copy
    // edit can't silently overflow.
    #[test]
    fn all_card_lines_fit_the_44_col_inner_width() {
        use unicode_width::UnicodeWidthStr;
        let ctxs = [
            TutorialCardCtx::default(),
            TutorialCardCtx {
                single_session: true,
                subagent_listed: true,
                list_focused: true,
            },
        ];
        for degraded in [false, true] {
            for profile in [Profile::Emacs, Profile::Vim] {
                for step in 1..=STEP_COUNT {
                    for phase in [
                        Step1Phase::AwaitCtrlX,
                        Step1Phase::AwaitCtrlG,
                        Step1Phase::AwaitNewSession,
                    ] {
                        for sub_seen in [false, true] {
                            for task_done in [false, true] {
                                for ctx in ctxs {
                                    let mut state = TutorialState::start(degraded, profile);
                                    state.step = step;
                                    state.step1_phase = phase;
                                    state.task_done = task_done;
                                    state.subagent_session_id = sub_seen.then(|| "sub".to_string());
                                    for line in state.lines(ctx) {
                                        let text: String =
                                            line.iter().map(|(s, _)| s.as_str()).collect();
                                        assert!(
                                            UnicodeWidthStr::width(text.as_str()) <= 44,
                                            "step {step} (degraded={degraded}, \
                                             {profile:?}) line overflows 44 cols: \
                                             {text:?}"
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
}
