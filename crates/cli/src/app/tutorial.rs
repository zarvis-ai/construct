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
pub const STEP_COUNT: u8 = 8;

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
    // Step 6 sub-checks.
    pub split_done: bool,
    pub task_done: bool,
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
            task_done: false,
            last_progress_at: Instant::now(),
            stalled: false,
        }
    }

    fn target(&self) -> TutorialTarget {
        match self.step {
            2 | 3 => TutorialTarget::View,
            4 => TutorialTarget::List,
            5 | 6 => TutorialTarget::Program,
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
            4 if self.focus_switched && self.selection_moved => self.advance(5),
            5 => {
                let ready = if self.degraded {
                    self.program_opened && self.template_applied && self.task_line_present
                } else {
                    self.run_started
                };
                if ready {
                    self.advance(6);
                }
            }
            6 => {
                let ready = if self.degraded {
                    self.split_done
                } else {
                    self.task_done
                };
                if ready {
                    self.advance(7);
                }
            }
            _ => {}
        }
    }

    pub fn tick(&mut self, now: Instant) {
        if self.completed || self.stalled {
            return;
        }
        if matches!(self.step, 5 | 6) && now.duration_since(self.last_progress_at) > STALL_TIMEOUT
        {
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
    /// keystrokes.
    pub fn lines(&self) -> Vec<TutorialLine> {
        if self.completed {
            return vec![
                vec![t("Tour complete — nice work covering the core")],
                vec![t("keybindings and the program board.")],
                vec![],
                vec![t("Replay anytime from the palette (tutorial).")],
            ];
        }
        match self.step {
            1 => step1_lines(self.step1_phase, self.profile),
            2 => step2_lines(self.degraded),
            3 => step3_lines(),
            4 => step4_lines(self.profile),
            5 => step5_lines(self.profile, self.degraded),
            6 => step6_lines(self.profile, self.degraded),
            7 => step7_lines(self.profile),
            8 => step8_lines(self.profile),
            _ => Vec::new(),
        }
    }

    /// Mini-checklist for the multi-part steps (4, 5, 6). Empty elsewhere.
    pub fn checklist(&self) -> Vec<(String, bool)> {
        match self.step {
            4 => vec![
                ("switch focus (list <-> view)".to_string(), self.focus_switched),
                ("move the selection".to_string(), self.selection_moved),
            ],
            5 => {
                let mut items = vec![
                    ("open the program".to_string(), self.program_opened),
                    ("apply the Tasks template".to_string(), self.template_applied),
                    ("add a task under Todo".to_string(), self.task_line_present),
                ];
                if !self.degraded {
                    items.push(("run it".to_string(), self.run_started));
                }
                items
            }
            6 => {
                if self.degraded {
                    vec![("split the pane".to_string(), self.split_done)]
                } else {
                    vec![
                        ("split the pane".to_string(), self.split_done),
                        (
                            "subagent appears".to_string(),
                            self.subagent_session_id.is_some(),
                        ),
                        ("task moves to Done".to_string(), self.task_done),
                    ]
                }
            }
            _ => Vec::new(),
        }
    }

    pub fn footer(&self) -> TutorialLine {
        if self.completed {
            vec![k("[close]", KeyAction::TutorialEndTour)]
        } else {
            vec![
                k("[skip step]", KeyAction::TutorialSkipStep),
                t("  "),
                k("[end tour]", KeyAction::TutorialEndTour),
            ]
        }
    }
}

fn step1_lines(phase: Step1Phase, profile: Profile) -> Vec<TutorialLine> {
    let mut lines = vec![
        vec![t("A chord like C-x C-f means: hold Ctrl, tap X, let go,")],
        vec![t("then (still holding Ctrl) tap F.")],
        vec![],
    ];
    match phase {
        Step1Phase::AwaitCtrlX => {
            lines.push(vec![
                t("Try it — press "),
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
            lines.push(vec![t("C-g backs out of anything half-typed: chords,")]);
            lines.push(vec![t("prompts, pickers.")]);
        }
        Step1Phase::AwaitNewSession => {
            lines.push(vec![
                t("Cancelled! For real this time: press "),
                k(chord_label(KeyAction::OpenNewSession, profile), KeyAction::TutorialNudge),
            ]);
            lines.push(vec![t("to create a session.")]);
        }
    }
    lines
}

fn step2_lines(degraded: bool) -> Vec<TutorialLine> {
    if degraded {
        vec![
            vec![t("The picker is open. No agent harness is set up yet,")],
            vec![t("so pick shell for now.")],
            vec![],
            vec![t("(Set one up in /configure to see delegation live later.)")],
        ]
    } else {
        vec![
            vec![t("The picker is open. Type the name of an agent harness")],
            vec![t("(not shell) and press Enter — step 5 needs an agent")],
            vec![t("behind this session.")],
        ]
    }
}

fn step3_lines() -> Vec<TutorialLine> {
    vec![
        vec![
            t("Focus the new session — click it, or press "),
            k("Enter", KeyAction::FocusView),
        ],
        vec![t("then type a short message and press Enter to send it.")],
    ]
}

fn step4_lines(profile: Profile) -> Vec<TutorialLine> {
    vec![
        vec![
            k(chord_label(KeyAction::SwitchFocus, profile), KeyAction::SwitchFocus),
            t(" toggles focus between the list and the view."),
        ],
        vec![
            k(chord_label(KeyAction::NextSession, profile), KeyAction::NextSession),
            t(" / "),
            k(chord_label(KeyAction::PrevSession, profile), KeyAction::PrevSession),
            t(" (or the arrow keys) move the selection."),
        ],
    ]
}

fn step5_lines(profile: Profile, degraded: bool) -> Vec<TutorialLine> {
    let mut lines = vec![
        vec![
            k(chord_label(KeyAction::OpenProgram, profile), KeyAction::OpenProgram),
            t(" opens the program board on this session."),
        ],
        vec![t("Pick the built-in \"Tasks\" template, then type")],
        vec![t("\"- Test task\" under ## Todo.")],
    ];
    if degraded {
        lines.push(vec![t("No agent harness is set up, so this step is")]);
        lines.push(vec![t("editing-only — nothing to run yet.")]);
    } else {
        lines.push(vec![
            k(chord_label(KeyAction::RunProgram, profile), KeyAction::RunProgram),
            t(" runs it — the board moves the task to In Progress"),
        ]);
        lines.push(vec![t("and hands it to a subagent.")]);
    }
    lines
}

fn step6_lines(profile: Profile, degraded: bool) -> Vec<TutorialLine> {
    if degraded {
        return vec![
            vec![
                k(
                    chord_label(KeyAction::SplitWindowBelow, profile),
                    KeyAction::SplitWindowBelow,
                ),
                t(" splits the pane below ("),
                k(
                    chord_label(KeyAction::SplitWindowRight, profile),
                    KeyAction::SplitWindowRight,
                ),
                t(" splits right)."),
            ],
            vec![t("Hop panes with C-x <arrow>.")],
        ];
    }
    vec![
        vec![
            t("While the run is in flight, "),
            k(
                chord_label(KeyAction::SplitWindowBelow, profile),
                KeyAction::SplitWindowBelow,
            ),
            t(" splits below."),
        ],
        vec![t("Select the subagent — it appears nested under this")],
        vec![t("session — in the other pane. Hop panes with C-x <arrow>.")],
    ]
}

fn step7_lines(profile: Profile) -> Vec<TutorialLine> {
    vec![
        vec![
            k(chord_label(KeyAction::ToggleHelp, profile), KeyAction::ToggleHelp),
            t(" opens help — any key closes it."),
        ],
        vec![t(format!(
            "{} quits construct — don't press it now!",
            chord_label(KeyAction::Quit, profile)
        ))],
        vec![t("(C-g still backs out of anything half-typed, as in step 1.)")],
        vec![],
        vec![k("[got it]", KeyAction::TutorialSkipStep)],
    ]
}

fn step8_lines(profile: Profile) -> Vec<TutorialLine> {
    vec![
        vec![
            k(
                chord_label(KeyAction::OpenDeleteConfirm, profile),
                KeyAction::OpenDeleteConfirm,
            ),
            t(" opens delete-confirm for this practice session."),
        ],
        vec![t("The subagent was already archived by the board's rule.")],
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

    pub fn tutorial_skip_step(&mut self) {
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
                t.feedback = Some("this one's for your fingers — try pressing the real keys".into());
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
                            | KeyAction::TutorialSkipStep
                            | KeyAction::TutorialEndTour
                    ) {
                        let label = chord_label(KeyAction::OpenNewSession, t.profile);
                        t.feedback = Some(format!(
                            "that ran something else — press C-g and try {label} again"
                        ));
                    }
                }
            }
            4 => match action {
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
            5 => match action {
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
            6 => {
                if matches!(
                    action,
                    KeyAction::SplitWindowBelow | KeyAction::SplitWindowRight
                ) {
                    t.split_done = true;
                    t.touch_progress();
                }
            }
            7 => {
                if action == KeyAction::ToggleHelp {
                    t.advance(8);
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
        let is_ctrl_x = matches!(key.code, KeyCode::Char('x')) && key.modifiers == KeyModifiers::CONTROL;
        let is_ctrl_g = matches!(key.code, KeyCode::Char('g')) && key.modifiers == KeyModifiers::CONTROL;
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
        if n.method == agentd_protocol::ipc_notif::STATE {
            if let Ok(payload) = serde_json::from_value::<StateNotificationPayload>(params) {
                let is_new = !self.sessions.iter().any(|s| s.id == payload.session.id);
                if is_new {
                    self.tutorial_on_session_created(&payload.session);
                }
            }
        } else if n.method == agentd_protocol::ipc_notif::PROGRAM_STATE {
            if let Ok(payload) =
                serde_json::from_value::<agentd_protocol::ProgramStateNotificationPayload>(params)
            {
                self.tutorial_on_program_state(&payload.program);
            }
        } else if n.method == agentd_protocol::ipc_notif::DELETED {
            if let Ok(payload) =
                serde_json::from_value::<agentd_protocol::DeletedNotificationPayload>(params)
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
                         can run for real ([skip step] to continue anyway)"
                            .to_string(),
                    );
                }
            }
            6 => {
                if let Some(practice) = t.practice_session_id.clone() {
                    if session.parent_session_id.as_deref() == Some(practice.as_str()) {
                        t.subagent_session_id = Some(session.id.clone());
                        t.touch_progress();
                    }
                }
            }
            _ => {}
        }
    }

    fn tutorial_on_program_state(&mut self, program: &agentd_protocol::ProgramDocument) {
        let Some(t) = self.tutorial.as_mut() else {
            return;
        };
        if t.completed || !matches!(t.step, 5 | 6) {
            return;
        }
        if t.practice_session_id.as_deref() != Some(program.session_id.as_str()) {
            return;
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
        if t.completed || t.step != 8 {
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

    /// Drives the step 5/6 stall hint from the existing render tick — no
    /// dedicated timer thread.
    pub fn tutorial_tick(&mut self, now: Instant) {
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

        let moved = "# Rule\n\n## Todo\n\n## In Progress\n\n- Test task\n\n## Done\n\n- Test task\n";
        assert!(!todo_section_has_task(moved));
        assert!(done_section_has_task(moved));
    }

    #[test]
    fn degraded_step5_checklist_has_no_run_row() {
        let mut state = TutorialState::start(true, Profile::Emacs);
        state.step = 5;
        let items = state.checklist();
        assert!(!items.iter().any(|(label, _)| label == "run it"));
    }

    #[test]
    fn non_degraded_step5_checklist_has_run_row() {
        let mut state = TutorialState::start(false, Profile::Emacs);
        state.step = 5;
        let items = state.checklist();
        assert!(items.iter().any(|(label, _)| label == "run it"));
    }

    #[test]
    fn step5_completion_differs_by_degraded_mode() {
        let mut degraded = TutorialState::start(true, Profile::Emacs);
        degraded.step = 5;
        degraded.program_opened = true;
        degraded.template_applied = true;
        degraded.task_line_present = true;
        degraded.recompute_completion();
        assert_eq!(degraded.step, 6, "degraded step5 completes without a run");

        let mut normal = TutorialState::start(false, Profile::Emacs);
        normal.step = 5;
        normal.program_opened = true;
        normal.template_applied = true;
        normal.task_line_present = true;
        normal.recompute_completion();
        assert_eq!(normal.step, 5, "non-degraded step5 still needs a run");
        normal.run_started = true;
        normal.recompute_completion();
        assert_eq!(normal.step, 6);
    }

    #[test]
    fn vim_profile_uses_vim_idiomatic_labels() {
        assert_eq!(chord_label(KeyAction::OpenNewSession, Profile::Vim), "o");
        assert_eq!(
            chord_label(KeyAction::SplitWindowBelow, Profile::Vim),
            "C-w s"
        );
        assert_eq!(chord_label(KeyAction::OpenDeleteConfirm, Profile::Vim), "dd");
    }
}
