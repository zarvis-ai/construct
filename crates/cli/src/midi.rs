//! Native MIDI control-surface support.
//!
//! `construct midi learn <action>` records a physical control in
//! `$CONSTRUCT_CONFIG_DIR/midi.toml`. The ordinary TUI opens that device and
//! dispatches learned controls through the same `KeyAction` path as keyboard,
//! mouse, palette, and clickable controls.

use std::fmt;
use std::path::Path;
#[cfg(target_os = "macos")]
use std::sync::mpsc as std_mpsc;

#[cfg(target_os = "macos")]
use anyhow::bail;
use anyhow::{Context, Result};
use clap::{Subcommand, ValueEnum};
#[cfg(target_os = "macos")]
use midir::{
    Ignore, MidiInput, MidiInputConnection, MidiInputPort, MidiOutput, MidiOutputConnection,
    MidiOutputPort,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::keymap::KeyAction;
use construct_protocol::paths::Paths;

#[derive(Debug, Clone, Subcommand)]
pub enum MidiCommand {
    /// List MIDI input devices visible to Construct.
    Devices,
    /// List actions that can be learned.
    Actions,
    /// Wait for the next MIDI control and bind it to an action.
    Learn {
        #[arg(value_enum)]
        action: MidiAction,
        /// Case-insensitive substring of the MIDI input device name.
        #[arg(long)]
        device: Option<String>,
        /// Override how the captured value triggers the action.
        #[arg(long, value_enum)]
        trigger: Option<MidiTrigger>,
    },
    /// Show the active device and learned mappings.
    Mappings,
    /// Remove every mapping for an action.
    Forget {
        #[arg(value_enum)]
        action: MidiAction,
    },
    /// Learn the dedicated OP-XY split/session controller layout.
    OpXyLearn {
        /// Case-insensitive substring of the OP-XY MIDI device name.
        #[arg(long)]
        device: Option<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum OpXyControl {
    Split(usize),
    CycleFocus,
    Escape,
    NoOp,
    Backspace,
    Prompt { slot: usize, text: String },
    Left,
    Down,
    Right,
    Up,
    Enter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpXyAuxControl {
    Up,
    Down,
    ScrollUp,
    ScrollDown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct OpXyEvent {
    /// Zero-based `[1]`–`[8]` session slot selected by MIDI channel 1–8.
    pub session: usize,
    pub control: OpXyControl,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MidiInputEvent {
    Action(MidiAction),
    OpXy(OpXyEvent),
    OpXyFocused(OpXyControl),
    OpXyAux(OpXyAuxControl),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackState {
    Idle,
    Working,
    AttentionIdle,
    AttentionWorking,
}

impl FeedbackState {
    fn is_active(self) -> bool {
        matches!(self, Self::Working | Self::AttentionWorking)
    }

    fn needs_attention(self) -> bool {
        matches!(self, Self::AttentionIdle | Self::AttentionWorking)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FeedbackSnapshot {
    pub fleet: FeedbackState,
    /// Bit 0 is session slot `[1]`; bit 7 is session slot `[8]`.
    pub active_slots: u8,
    pub attention_slots: u8,
    /// Low four bits correspond to session slots `[1]`–`[4]` and their synth tracks.
    pub active_tracks: u8,
    pub attention_tracks: u8,
}

impl Default for FeedbackSnapshot {
    fn default() -> Self {
        Self {
            fleet: FeedbackState::Idle,
            active_slots: 0,
            attention_slots: 0,
            active_tracks: 0,
            attention_tracks: 0,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OpXyFeedbackConfig {
    pub enabled: bool,
    /// Scene numbers are written as the one-based numbers shown by OP-XY.
    pub normal_scene: u8,
    pub attention_scene: u8,
    /// First of four consecutive OP-XY synth parameters. Defaults to CC 12–15.
    #[serde(alias = "split_activity_cc")]
    pub track_activity_cc: u8,
}

impl Default for OpXyFeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            normal_scene: 1,
            attention_scene: 2,
            track_activity_cc: 12,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct OpXyAuxConfig {
    pub enabled: bool,
    /// Human-facing MIDI channel used by the OP-XY auxiliary encoder track.
    pub channel: u8,
    /// Aux-track note channels whose keys target the currently focused split pane.
    #[serde(default = "default_op_xy_aux_focused_note_channels")]
    pub focused_note_channels: Vec<u8>,
    /// Absolute CC for the third encoder.
    pub arrow_cc: u8,
    /// Absolute CC for the fourth encoder.
    pub scroll_cc: u8,
}

impl Default for OpXyAuxConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            channel: 10,
            focused_note_channels: default_op_xy_aux_focused_note_channels(),
            arrow_cc: 2,
            scroll_cc: 3,
        }
    }
}

fn default_op_xy_aux_focused_note_channels() -> Vec<u8> {
    vec![10]
}

fn default_op_xy_navigation_channels() -> Vec<u8> {
    (1..=8).chain(std::iter::once(10)).collect()
}

const fn default_op_xy_bank_cc() -> u8 {
    0
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OpXyAuxState {
    arrow_value: Option<u8>,
    scroll_value: Option<u8>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct OpXyNavigationState {
    bank_values: [Option<u8>; 16],
    program_values: [Option<u8>; 16],
}

impl OpXyNavigationState {
    fn event_for(&mut self, config: &OpXyConfig, message: &MidiMessage) -> Option<OpXyAuxControl> {
        if !config.enabled || !config.navigation_channels.contains(&message.channel) {
            return None;
        }
        let channel = usize::from(message.channel.checked_sub(1)?);
        let (previous, increasing, decreasing) = match message.kind {
            MidiMessageKind::Cc if message.number == config.bank_cc => (
                &mut self.bank_values[channel],
                OpXyAuxControl::Down,
                OpXyAuxControl::Up,
            ),
            MidiMessageKind::ProgramChange => (
                &mut self.program_values[channel],
                OpXyAuxControl::ScrollDown,
                OpXyAuxControl::ScrollUp,
            ),
            _ => return None,
        };
        let old = previous.replace(message.value)?;
        absolute_encoder_direction(old, message.value).map(|direction| match direction {
            EncoderDirection::Increase => increasing,
            EncoderDirection::Decrease => decreasing,
        })
    }
}

impl OpXyAuxState {
    fn event_for(
        &mut self,
        config: &OpXyAuxConfig,
        message: &MidiMessage,
    ) -> Option<OpXyAuxControl> {
        if !config.enabled
            || message.kind != MidiMessageKind::Cc
            || message.channel != config.channel
        {
            return None;
        }
        let (previous, increasing, decreasing) = if message.number == config.arrow_cc {
            (&mut self.arrow_value, OpXyAuxControl::Down, OpXyAuxControl::Up)
        } else if message.number == config.scroll_cc {
            (
                &mut self.scroll_value,
                OpXyAuxControl::ScrollDown,
                OpXyAuxControl::ScrollUp,
            )
        } else {
            return None;
        };
        let old = previous.replace(message.value)?;
        absolute_encoder_direction(old, message.value).map(|direction| match direction {
            EncoderDirection::Increase => increasing,
            EncoderDirection::Decrease => decreasing,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum EncoderDirection {
    Increase,
    Decrease,
}

fn absolute_encoder_direction(previous: u8, current: u8) -> Option<EncoderDirection> {
    if previous == current {
        return None;
    }
    // OP-XY encoders can cross the end of their absolute 0–127 range. Choose
    // the shorter direction around that ring so 127→0 remains an increase.
    let forward = current.wrapping_sub(previous) & 0x7f;
    let backward = previous.wrapping_sub(current) & 0x7f;
    Some(if forward <= backward {
        EncoderDirection::Increase
    } else {
        EncoderDirection::Decrease
    })
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OpXyConfig {
    pub enabled: bool,
    /// MIDI channels whose first-black-key anchors were learned. Channels 1–8
    /// always address session slots `[1]`–`[8]`; this list only normalizes
    /// tracks that use different octaves.
    #[serde(alias = "pane_channels")]
    pub session_channels: Vec<u8>,
    /// First black-key note for each entry in `session_channels`.
    #[serde(alias = "pane_anchor_notes")]
    pub track_anchor_notes: Vec<u8>,
    /// The eight black-key notes, learned on the reference track.
    #[serde(alias = "session_notes")]
    pub black_notes: Vec<u8>,
    /// Prompt text assigned to white keys 1–6. Empty or missing entries are unassigned.
    pub prompt_texts: Vec<String>,
    pub left_note: Option<u8>,
    pub down_note: Option<u8>,
    pub right_note: Option<u8>,
    pub up_note: Option<u8>,
    pub enter_note: Option<u8>,
    /// Legacy white-key no-op retained only for old profile compatibility.
    /// New profiles reserve black key 7 instead.
    pub no_op_note: Option<u8>,
    /// Track channels whose Bank Select and Program Change values navigate the TUI.
    #[serde(default = "default_op_xy_navigation_channels")]
    pub navigation_channels: Vec<u8>,
    /// Absolute Bank Select CC used for Up/Down navigation.
    #[serde(default = "default_op_xy_bank_cc")]
    pub bank_cc: u8,
    pub aux: OpXyAuxConfig,
    pub feedback: OpXyFeedbackConfig,
}

impl OpXyConfig {
    fn event_for(&self, message: &MidiMessage) -> Option<OpXyEvent> {
        if !self.enabled || message.kind != MidiMessageKind::Note || !message.pressed {
            return None;
        }
        let session = usize::from(message.channel.checked_sub(1)?);
        if session >= 8 {
            return None;
        }
        let reference_anchor = self.black_notes.first().copied()?;
        let anchor_index = self
            .session_channels
            .iter()
            .position(|channel| *channel == message.channel);
        let track_anchor = anchor_index
            .and_then(|index| self.track_anchor_notes.get(index))
            .copied()
            .unwrap_or(reference_anchor);
        let normalized_note =
            i16::from(message.number) + i16::from(reference_anchor) - i16::from(track_anchor);
        let normalized_note = u8::try_from(normalized_note)
            .ok()
            .filter(|note| *note <= 127)?;
        let control = self.control_for_note(normalized_note)?;
        Some(OpXyEvent { session, control })
    }

    fn focused_event_for(&self, message: &MidiMessage) -> Option<OpXyControl> {
        if !self.enabled
            || message.kind != MidiMessageKind::Note
            || !message.pressed
            || !self.aux.enabled
            || !self.aux.focused_note_channels.contains(&message.channel)
        {
            return None;
        }
        self.control_for_note(message.number)
    }

    fn control_for_note(&self, normalized_note: u8) -> Option<OpXyControl> {
        let reference_anchor = self.black_notes.first().copied()?;
        let control = if let Some(key) = self
            .black_notes
            .iter()
            .position(|note| *note == normalized_note)
        {
            match key {
                0..=3 => OpXyControl::Split(key),
                4 => OpXyControl::CycleFocus,
                5 => OpXyControl::Escape,
                6 => OpXyControl::NoOp,
                7 => OpXyControl::Backspace,
                _ => return None,
            }
        } else if let Some((slot, text)) = self
            .prompt_texts
            .iter()
            .take(OP_XY_PROMPT_KEY_OFFSETS.len())
            .enumerate()
            .find(|(slot, text)| {
                !text.is_empty()
                    && op_xy_prompt_note(reference_anchor, *slot) == Some(normalized_note)
            })
        {
            OpXyControl::Prompt {
                slot,
                text: text.clone(),
            }
        } else if self.left_note == Some(normalized_note) {
            OpXyControl::Left
        } else if self.down_note == Some(normalized_note) {
            OpXyControl::Down
        } else if self.right_note == Some(normalized_note) {
            OpXyControl::Right
        } else if self.up_note == Some(normalized_note) {
            OpXyControl::Up
        } else if self.enter_note == Some(normalized_note) {
            OpXyControl::Enter
        } else {
            return None;
        };
        Some(control)
    }
}

/// White keys 1–6 relative to black key 1. The learned layout starts at F/F♯:
/// white 1 is one semitone below the first black key, then follows white notes.
const OP_XY_PROMPT_KEY_OFFSETS: [i16; 6] = [-1, 1, 3, 5, 6, 8];

fn op_xy_prompt_note(reference_anchor: u8, slot: usize) -> Option<u8> {
    let note = i16::from(reference_anchor) + *OP_XY_PROMPT_KEY_OFFSETS.get(slot)?;
    u8::try_from(note).ok().filter(|note| *note <= 127)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum, Ord, PartialOrd)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum MidiAction {
    NextSession,
    PreviousSession,
    FocusList,
    FocusView,
    SwitchFocus,
    ScrollUp,
    ScrollDown,
    PageUp,
    PageDown,
    NewSession,
    SendInput,
    Fork,
    Diff,
    Program,
    RunProgram,
    Interrupt,
    SwitchSession,
    CommandPalette,
    TogglePin,
    ToggleAutomode,
    ToggleZoom,
    SplitBelow,
    SplitRight,
    Refresh,
    Help,
    Approve,
    Reject,
}

impl MidiAction {
    pub(crate) fn key_action(self) -> Option<KeyAction> {
        use KeyAction as K;
        Some(match self {
            Self::NextSession => K::NextSession,
            Self::PreviousSession => K::PrevSession,
            Self::FocusList => K::FocusList,
            Self::FocusView => K::FocusView,
            Self::SwitchFocus => K::SwitchFocus,
            Self::ScrollUp => K::ScrollUp,
            Self::ScrollDown => K::ScrollDown,
            Self::PageUp => K::ScrollPageUp,
            Self::PageDown => K::ScrollPageDown,
            Self::NewSession => K::OpenNewSession,
            Self::SendInput => K::OpenSendInput,
            Self::Fork => K::OpenFork,
            Self::Diff => K::OpenDiff,
            Self::Program => K::OpenProgram,
            Self::RunProgram => K::RunProgram,
            Self::Interrupt => K::Interrupt,
            Self::SwitchSession => K::OpenSwitchSession,
            Self::CommandPalette => K::OpenCommandPalette,
            Self::TogglePin => K::TogglePin,
            Self::ToggleAutomode => K::ToggleAutomode,
            Self::ToggleZoom => K::ToggleZoom,
            Self::SplitBelow => K::SplitWindowBelow,
            Self::SplitRight => K::SplitWindowRight,
            Self::Refresh => K::Refresh,
            Self::Help => K::ToggleHelp,
            Self::Approve | Self::Reject => return None,
        })
    }

    fn label(self) -> String {
        self.to_possible_value()
            .expect("MidiAction variants have clap names")
            .get_name()
            .to_string()
    }
}

impl fmt::Display for MidiAction {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.label())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ValueEnum)]
#[serde(rename_all = "kebab-case")]
#[clap(rename_all = "kebab-case")]
pub enum MidiTrigger {
    /// Note-on with non-zero velocity.
    Press,
    /// Note-off (including note-on with zero velocity).
    Release,
    /// Relative CC values 1–63.
    Increase,
    /// Relative CC values 65–127.
    Decrease,
    /// Values 64–127, useful for CC buttons.
    High,
    /// Values 0–63, useful for CC button release.
    Low,
    /// Every value change.
    Any,
}

impl fmt::Display for MidiTrigger {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let name = self
            .to_possible_value()
            .expect("MidiTrigger variants have clap names");
        f.write_str(name.get_name())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MidiMessageKind {
    Note,
    Cc,
    ProgramChange,
}

impl fmt::Display for MidiMessageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Note => f.write_str("note"),
            Self::Cc => f.write_str("cc"),
            Self::ProgramChange => f.write_str("program-change"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct MidiMessage {
    kind: MidiMessageKind,
    channel: u8,
    number: u8,
    value: u8,
    pressed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct MidiMapping {
    pub kind: MidiMessageKind,
    /// MIDI channels are written as the human-facing range 1–16.
    pub channel: u8,
    pub number: u8,
    pub trigger: MidiTrigger,
    pub action: MidiAction,
}

impl MidiMapping {
    fn matches(&self, message: &MidiMessage) -> bool {
        if self.kind != message.kind
            || self.channel != message.channel
            || self.number != message.number
        {
            return false;
        }
        match self.trigger {
            MidiTrigger::Press => message.kind == MidiMessageKind::Note && message.pressed,
            MidiTrigger::Release => message.kind == MidiMessageKind::Note && !message.pressed,
            MidiTrigger::Increase => {
                message.kind == MidiMessageKind::Cc && (1..=63).contains(&message.value)
            }
            MidiTrigger::Decrease => {
                message.kind == MidiMessageKind::Cc && (65..=127).contains(&message.value)
            }
            MidiTrigger::High => message.value >= 64,
            MidiTrigger::Low => message.value < 64,
            MidiTrigger::Any => true,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct MidiConfig {
    /// Case-insensitive device-name substring. Learn stores the full name.
    pub device: Option<String>,
    pub mappings: Vec<MidiMapping>,
    pub op_xy: Option<OpXyConfig>,
}

impl MidiConfig {
    fn load(path: &Path) -> Result<Self> {
        match std::fs::read_to_string(path) {
            Ok(raw) => toml::from_str(&raw).with_context(|| format!("parse {}", path.display())),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(e) => Err(e).with_context(|| format!("read {}", path.display())),
        }
    }

    fn save(&self, path: &Path) -> Result<()> {
        let parent = path
            .parent()
            .context("MIDI config path has no parent directory")?;
        std::fs::create_dir_all(parent).with_context(|| format!("create {}", parent.display()))?;
        let raw = toml::to_string_pretty(self).context("serialize MIDI config")?;
        let mut temp = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary file in {}", parent.display()))?;
        use std::io::Write as _;
        temp.write_all(raw.as_bytes())?;
        temp.as_file().sync_all()?;
        temp.persist(path)
            .map_err(|e| e.error)
            .with_context(|| format!("replace {}", path.display()))?;
        Ok(())
    }
}

#[cfg(target_os = "macos")]
pub(crate) struct MidiListener {
    _connection: MidiInputConnection<()>,
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct MidiListener;

type MidiEventReceiver = mpsc::UnboundedReceiver<MidiInputEvent>;

#[cfg(target_os = "macos")]
pub(crate) fn start_listener() -> Result<Option<(MidiListener, MidiEventReceiver)>> {
    let path = Paths::discover().midi_file();
    let config = MidiConfig::load(&path)?;
    let op_xy_enabled = config.op_xy.as_ref().is_some_and(|profile| profile.enabled);
    if config.mappings.is_empty() && !op_xy_enabled {
        return Ok(None);
    }
    let device = config.device.as_deref().context(format!(
        "{} has mappings but no MIDI device",
        path.display()
    ))?;
    let mut input = MidiInput::new("construct-midi").context("initialize MIDI input")?;
    input.ignore(Ignore::All);
    let port = find_port(&input, Some(device))?;
    let port_name = input.port_name(&port).context("read MIDI device name")?;
    let mappings = config.mappings;
    let op_xy = config.op_xy;
    let mut op_xy_aux_state = OpXyAuxState::default();
    let mut op_xy_navigation_state = OpXyNavigationState::default();
    let (tx, rx) = mpsc::unbounded_channel();
    let connection = input
        .connect(
            &port,
            "construct-midi-control",
            move |_timestamp, bytes, _| {
                let Some(message) = parse_message(bytes) else {
                    return;
                };
                if let Some(event) = op_xy
                    .as_ref()
                    .and_then(|profile| profile.event_for(&message))
                {
                    let _ = tx.send(MidiInputEvent::OpXy(event));
                    return;
                }
                if let Some(control) = op_xy
                    .as_ref()
                    .and_then(|profile| profile.focused_event_for(&message))
                {
                    let _ = tx.send(MidiInputEvent::OpXyFocused(control));
                    return;
                }
                if let Some(event) = op_xy.as_ref().and_then(|profile| {
                    profile
                        .enabled
                        .then(|| op_xy_aux_state.event_for(&profile.aux, &message))
                        .flatten()
                }) {
                    let _ = tx.send(MidiInputEvent::OpXyAux(event));
                    return;
                }
                if let Some(event) = op_xy
                    .as_ref()
                    .and_then(|profile| op_xy_navigation_state.event_for(profile, &message))
                {
                    let _ = tx.send(MidiInputEvent::OpXyAux(event));
                    return;
                }
                for mapping in &mappings {
                    if mapping.matches(&message) {
                        let _ = tx.send(MidiInputEvent::Action(mapping.action));
                    }
                }
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .with_context(|| format!("connect MIDI device {port_name:?}"))?;
    Ok(Some((
        MidiListener {
            _connection: connection,
        },
        rx,
    )))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn start_listener() -> Result<Option<(MidiListener, MidiEventReceiver)>> {
    anyhow::bail!("native MIDI control is currently supported on macOS")
}

pub async fn run(command: Option<MidiCommand>) -> Result<()> {
    match command {
        None | Some(MidiCommand::Mappings) => print_mappings(),
        Some(MidiCommand::Devices) => print_devices(),
        Some(MidiCommand::Actions) => {
            for action in MidiAction::value_variants() {
                println!("{}", action.label());
            }
            Ok(())
        }
        Some(MidiCommand::Learn {
            action,
            device,
            trigger,
        }) => learn(action, device.as_deref(), trigger),
        Some(MidiCommand::Forget { action }) => forget(action),
        Some(MidiCommand::OpXyLearn { device }) => op_xy_learn(device.as_deref()),
    }
}

#[cfg(target_os = "macos")]
fn print_devices() -> Result<()> {
    let input = MidiInput::new("construct-midi-devices").context("initialize MIDI input")?;
    let ports = input.ports();
    if ports.is_empty() {
        println!("(no MIDI input devices)");
        return Ok(());
    }
    for port in ports {
        println!(
            "{}",
            input
                .port_name(&port)
                .unwrap_or_else(|_| "(unknown)".into())
        );
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn print_devices() -> Result<()> {
    anyhow::bail!("native MIDI control is currently supported on macOS")
}

fn print_mappings() -> Result<()> {
    let path = Paths::discover().midi_file();
    let config = MidiConfig::load(&path)?;
    println!("config: {}", path.display());
    println!(
        "device: {}",
        config.device.as_deref().unwrap_or("(not set)")
    );
    if config.mappings.is_empty() {
        println!("(no mappings; run `construct midi learn <action>`)");
    }
    for mapping in config.mappings {
        println!(
            "{:<18} {} ch={} number={} trigger={}",
            mapping.action, mapping.kind, mapping.channel, mapping.number, mapping.trigger
        );
    }
    if let Some(op_xy) = config.op_xy {
        println!(
            "op-xy: {}",
            if op_xy.enabled { "enabled" } else { "disabled" }
        );
        println!("  session channels: {:?}", op_xy.session_channels);
        println!("  black notes: {:?}", op_xy.black_notes);
        for (slot, prompt) in op_xy.prompt_texts.iter().take(6).enumerate() {
            if !prompt.is_empty() {
                println!("  white prompt {}: {:?}", slot + 1, prompt);
            }
        }
        if op_xy.aux.enabled {
            println!(
                "  aux: encoder channel {}, focused note channels {:?}, arrow CC {}, scroll CC {}",
                op_xy.aux.channel,
                op_xy.aux.focused_note_channels,
                op_xy.aux.arrow_cc,
                op_xy.aux.scroll_cc
            );
        }
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn op_xy_learn(requested_device: Option<&str>) -> Result<()> {
    let path = Paths::discover().midi_file();
    let mut config = MidiConfig::load(&path)?;
    let selector = requested_device
        .or(config.device.as_deref())
        .or(Some("OP-XY"));
    let mut input = MidiInput::new("construct-op-xy-learn").context("initialize MIDI input")?;
    input.ignore(Ignore::All);
    let port = find_port(&input, selector)?;
    let port_name = input.port_name(&port).context("read MIDI device name")?;
    let (tx, rx) = std_mpsc::channel();
    let _connection = input
        .connect(
            &port,
            "construct-op-xy-learn",
            move |_timestamp, bytes, _| {
                if let Some(message) = parse_message(bytes) {
                    if message.kind == MidiMessageKind::Note && message.pressed {
                        let _ = tx.send(message);
                    }
                }
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .with_context(|| format!("connect MIDI device {port_name:?}"))?;

    fn capture(rx: &std_mpsc::Receiver<MidiMessage>, prompt: &str) -> Result<MidiMessage> {
        eprintln!("{prompt}");
        let message = rx.recv().context("MIDI learn channel closed")?;
        eprintln!(
            "  captured channel {} note {}",
            message.channel, message.number
        );
        Ok(message)
    }

    eprintln!("listening on {port_name:?}");
    eprintln!("Use the linked OP-XY Construct template, not Controller Mode.");
    let first = capture(&rx, "Select MIDI-channel track 1 and press black key 1…")?;
    if first.channel != 1 {
        anyhow::bail!(
            "track 1 sent MIDI channel {}; configure it for channel 1 and learn again",
            first.channel
        );
    }
    let mut session_channels = vec![first.channel];
    let mut track_anchor_notes = vec![first.number];
    for session in 2..=8 {
        let message = capture(
            &rx,
            &format!("Select MIDI-channel track {session} and press black key 1…"),
        )?;
        if message.channel != session {
            anyhow::bail!(
                "track {session} sent MIDI channel {}; configure it for channel {session} and learn again",
                message.channel
            );
        }
        session_channels.push(message.channel);
        track_anchor_notes.push(message.number);
    }
    let mut black_notes = vec![first.number];
    eprintln!("Return to track 1.");
    for key in 2..=8 {
        black_notes.push(capture(&rx, &format!("Press black key {key}…"))?.number);
    }
    let left_note = capture(&rx, "Press the LEFT arrow key…")?.number;
    let down_note = capture(&rx, "Press the DOWN arrow key…")?.number;
    let right_note = capture(&rx, "Press the RIGHT arrow key…")?.number;
    let up_note = capture(&rx, "Press the UP arrow key…")?.number;
    let enter_note = capture(&rx, "Press the final black ENTER key…")?.number;
    let unique_channels: std::collections::HashSet<_> = session_channels.iter().copied().collect();
    if unique_channels.len() != 8 {
        anyhow::bail!(
            "the eight OP-XY tracks did not produce eight distinct MIDI channels; check their external MIDI channel settings and learn again"
        );
    }
    let all_notes = black_notes
        .iter()
        .copied()
        .chain([left_note, down_note, right_note, up_note, enter_note]);
    let unique_notes: std::collections::HashSet<_> = all_notes.clone().collect();
    if unique_notes.len() != all_notes.count() {
        anyhow::bail!(
            "the OP-XY profile captured a key more than once; stop its sequencer and learn again"
        );
    }

    config.device = Some(port_name);
    config.op_xy = Some(OpXyConfig {
        enabled: true,
        session_channels,
        track_anchor_notes,
        black_notes,
        prompt_texts: Vec::new(),
        left_note: Some(left_note),
        down_note: Some(down_note),
        right_note: Some(right_note),
        up_note: Some(up_note),
        enter_note: Some(enter_note),
        no_op_note: None,
        navigation_channels: default_op_xy_navigation_channels(),
        bank_cc: default_op_xy_bank_cc(),
        aux: OpXyAuxConfig::default(),
        feedback: OpXyFeedbackConfig::default(),
    });
    config.save(&path)?;
    println!("saved OP-XY controller profile to {}", path.display());
    println!("prefix session titles with `[1]` through `[8]` to assign controller slots");
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn op_xy_learn(_requested_device: Option<&str>) -> Result<()> {
    anyhow::bail!("native MIDI control is currently supported on macOS")
}

#[cfg(target_os = "macos")]
pub(crate) struct MidiFeedback {
    tx: std_mpsc::Sender<FeedbackSnapshot>,
    last_snapshot: std::cell::Cell<FeedbackSnapshot>,
}

#[cfg(not(target_os = "macos"))]
pub(crate) struct MidiFeedback;

#[cfg(target_os = "macos")]
impl MidiFeedback {
    pub(crate) fn update(&self, snapshot: FeedbackSnapshot) {
        if self.last_snapshot.replace(snapshot) != snapshot {
            let _ = self.tx.send(snapshot);
        }
    }
}

#[cfg(not(target_os = "macos"))]
impl MidiFeedback {
    pub(crate) fn update(&self, _snapshot: FeedbackSnapshot) {}
}

#[cfg(target_os = "macos")]
pub(crate) fn start_feedback() -> Result<Option<MidiFeedback>> {
    let config = MidiConfig::load(&Paths::discover().midi_file())?;
    let Some(profile) = config.op_xy.filter(|profile| profile.enabled && profile.feedback.enabled)
    else {
        return Ok(None);
    };
    let device = config.device.as_deref().context("OP-XY profile has no MIDI device")?;
    let output = MidiOutput::new("construct-op-xy-feedback").context("initialize MIDI output")?;
    let port = find_output_port(&output, device)?;
    let port_name = output.port_name(&port).context("read MIDI output name")?;
    let connection = output
        .connect(&port, "construct-op-xy-feedback")
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .with_context(|| format!("connect MIDI output {port_name:?}"))?;
    let (tx, rx) = std_mpsc::channel();
    std::thread::Builder::new()
        .name("construct-midi-feedback".into())
        .spawn(move || feedback_loop(connection, rx, profile.feedback))
        .context("spawn MIDI feedback thread")?;
    Ok(Some(MidiFeedback {
        tx,
        last_snapshot: std::cell::Cell::new(FeedbackSnapshot::default()),
    }))
}

#[cfg(not(target_os = "macos"))]
pub(crate) fn start_feedback() -> Result<Option<MidiFeedback>> {
    Ok(None)
}

const FEEDBACK_MIN_FRAME_PERIOD: std::time::Duration =
    std::time::Duration::from_millis(200);
const FEEDBACK_MAX_CC_MESSAGES_PER_SECOND: u32 = 16;
const FEEDBACK_RETRY_PERIOD: std::time::Duration = std::time::Duration::from_secs(2);

fn feedback_activity_message_count(snapshot: FeedbackSnapshot) -> u32 {
    (snapshot.active_slots | snapshot.attention_slots).count_ones()
        + (snapshot.active_tracks | snapshot.attention_tracks).count_ones()
            * u32::from(SPLIT_ACTIVITY_PARAMETER_COUNT)
}

fn feedback_frame_period(snapshot: FeedbackSnapshot) -> std::time::Duration {
    let messages = feedback_activity_message_count(snapshot);
    if messages == 0 {
        return std::time::Duration::from_millis(250);
    }
    let budget_period = std::time::Duration::from_secs_f64(
        f64::from(messages) / f64::from(FEEDBACK_MAX_CC_MESSAGES_PER_SECOND),
    );
    budget_period.max(FEEDBACK_MIN_FRAME_PERIOD)
}

#[cfg(target_os = "macos")]
fn feedback_loop(
    mut connection: MidiOutputConnection,
    rx: std_mpsc::Receiver<FeedbackSnapshot>,
    config: OpXyFeedbackConfig,
) {
    // CC 7 is 0–127. Active work moves gently through 25–40%; attention
    // performs a two-stage damped bounce through 30–70%.
    const ACTIVE_MOTION: [u8; 8] = [32, 38, 45, 51, 45, 38, 34, 32];
    const ATTENTION_BOUNCE: [u8; 8] = [38, 62, 89, 58, 38, 56, 44, 38];
    let mut snapshot = FeedbackSnapshot::default();
    let mut transport_started = None;
    let mut sent_fleet = None;
    let mut next_fleet_retry = std::time::Instant::now();
    let mut volume_frame = 0usize;
    let mut next_volume_frame = std::time::Instant::now();
    send_slot_volumes(&mut connection, u8::MAX, 0);
    send_pane_parameters(&mut connection, 0b0000_1111, config.track_activity_cc, 0);
    loop {
        let timeout = feedback_frame_period(snapshot);
        match rx.recv_timeout(timeout) {
            Ok(mut next) => {
                // If CoreMIDI briefly blocks behind the Bluetooth transport,
                // apply only the newest fleet state once it becomes writable.
                while let Ok(newer) = rx.try_recv() {
                    next = newer;
                }
                if next.fleet != snapshot.fleet {
                    sent_fleet = None;
                    next_fleet_retry = std::time::Instant::now();
                }
                if next.active_slots != snapshot.active_slots
                    || next.attention_slots != snapshot.attention_slots
                {
                    let previous_visible = snapshot.active_slots | snapshot.attention_slots;
                    let next_visible = next.active_slots | next.attention_slots;
                    send_slot_volumes(&mut connection, previous_visible & !next_visible, 0);
                    volume_frame = 0;
                    next_volume_frame = std::time::Instant::now();
                }
                if next.active_tracks != snapshot.active_tracks
                    || next.attention_tracks != snapshot.attention_tracks
                {
                    let previous_visible = snapshot.active_tracks | snapshot.attention_tracks;
                    let next_visible = next.active_tracks | next.attention_tracks;
                    send_pane_parameters(
                        &mut connection,
                        previous_visible & !next_visible,
                        config.track_activity_cc,
                        0,
                    );
                    volume_frame = 0;
                    next_volume_frame = std::time::Instant::now();
                }
                snapshot = next;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                send_slot_volumes(&mut connection, u8::MAX, 0);
                send_pane_parameters(&mut connection, 0b0000_1111, config.track_activity_cc, 0);
                let _ = connection.send(&[0xFC]);
                break;
            }
        }
        let now = std::time::Instant::now();
        if sent_fleet != Some(snapshot.fleet) && now >= next_fleet_retry {
            if send_fleet_state(
                &mut connection,
                snapshot.fleet,
                &mut transport_started,
                &config,
            ) {
                sent_fleet = Some(snapshot.fleet);
            } else {
                next_fleet_retry = now + FEEDBACK_RETRY_PERIOD;
            }
        }
        let any_activity = snapshot.active_slots
            | snapshot.attention_slots
            | snapshot.active_tracks
            | snapshot.attention_tracks;
        if any_activity != 0 && now >= next_volume_frame {
            send_activity_frame(
                &mut connection,
                snapshot.active_slots,
                snapshot.attention_slots,
                snapshot.active_tracks,
                snapshot.attention_tracks,
                config.track_activity_cc,
                ACTIVE_MOTION[volume_frame],
                ATTENTION_BOUNCE[volume_frame],
            );
            volume_frame = (volume_frame + 1) % ACTIVE_MOTION.len();
            next_volume_frame = now + feedback_frame_period(snapshot);
        }
    }
}

fn track_volume_message(slot: usize, value: u8) -> Option<[u8; 3]> {
    (slot < 8).then_some([0xB0 | slot as u8, 7, value.min(127)])
}

fn pane_parameter_message(pane: usize, cc: u8, value: u8) -> Option<[u8; 3]> {
    (pane < 4).then_some([0xB0 | pane as u8, cc.min(127), value.min(127)])
}

const SPLIT_ACTIVITY_PARAMETER_COUNT: u8 = 4;

fn split_activity_ccs(first_cc: u8) -> impl Iterator<Item = u8> {
    let first_cc = first_cc.min(127 - (SPLIT_ACTIVITY_PARAMETER_COUNT - 1));
    first_cc..first_cc + SPLIT_ACTIVITY_PARAMETER_COUNT
}

fn slot_volume_packet(slots: u8, value: u8) -> Vec<u8> {
    let mut packet = Vec::with_capacity(slots.count_ones() as usize * 3);
    for slot in 0..8 {
        if slots & (1 << slot) != 0 {
            if let Some(message) = track_volume_message(slot, value) {
                packet.extend_from_slice(&message);
            }
        }
    }
    packet
}

#[cfg(target_os = "macos")]
fn send_slot_volumes(connection: &mut MidiOutputConnection, slots: u8, value: u8) {
    let packet = slot_volume_packet(slots, value);
    if !packet.is_empty() {
        let _ = connection.send(&packet);
    }
}

fn pane_parameter_packet(panes: u8, cc: u8, value: u8) -> Vec<u8> {
    let panes = panes & 0b0000_1111;
    let mut packet = Vec::with_capacity(
        panes.count_ones() as usize * usize::from(SPLIT_ACTIVITY_PARAMETER_COUNT) * 3,
    );
    for pane in 0..4 {
        if panes & (1 << pane) != 0 {
            for parameter_cc in split_activity_ccs(cc) {
                if let Some(message) = pane_parameter_message(pane, parameter_cc, value) {
                    packet.extend_from_slice(&message);
                }
            }
        }
    }
    packet
}

#[cfg(target_os = "macos")]
fn send_pane_parameters(connection: &mut MidiOutputConnection, panes: u8, cc: u8, value: u8) {
    let packet = pane_parameter_packet(panes, cc, value);
    if !packet.is_empty() {
        let _ = connection.send(&packet);
    }
}

fn activity_volume_packet(
    active_slots: u8,
    attention_slots: u8,
    active_value: u8,
    attention_value: u8,
) -> Vec<u8> {
    let mut packet = Vec::with_capacity((active_slots | attention_slots).count_ones() as usize * 3);
    for slot in 0..8 {
        let bit = 1 << slot;
        let value = if attention_slots & bit != 0 {
            Some(attention_value)
        } else if active_slots & bit != 0 {
            Some(active_value)
        } else {
            None
        };
        if let Some(message) = value.and_then(|value| track_volume_message(slot, value)) {
            packet.extend_from_slice(&message);
        }
    }
    packet
}

fn activity_pane_packet(
    active_tracks: u8,
    attention_tracks: u8,
    cc: u8,
    active_value: u8,
    attention_value: u8,
) -> Vec<u8> {
    let visible = (active_tracks | attention_tracks) & 0b0000_1111;
    let mut packet = Vec::with_capacity(
        visible.count_ones() as usize * usize::from(SPLIT_ACTIVITY_PARAMETER_COUNT) * 3,
    );
    for pane in 0..4 {
        let bit = 1 << pane;
        let value = if attention_tracks & bit != 0 {
            Some(attention_value)
        } else if active_tracks & bit != 0 {
            Some(active_value)
        } else {
            None
        };
        if let Some(value) = value {
            for parameter_cc in split_activity_ccs(cc) {
                if let Some(message) = pane_parameter_message(pane, parameter_cc, value) {
                    packet.extend_from_slice(&message);
                }
            }
        }
    }
    packet
}

#[cfg(target_os = "macos")]
fn send_activity_frame(
    connection: &mut MidiOutputConnection,
    active_slots: u8,
    attention_slots: u8,
    active_tracks: u8,
    attention_tracks: u8,
    pane_cc: u8,
    active_value: u8,
    attention_value: u8,
) {
    let mut packet =
        activity_volume_packet(active_slots, attention_slots, active_value, attention_value);
    packet.extend(activity_pane_packet(
        active_tracks,
        attention_tracks,
        pane_cc,
        active_value,
        attention_value,
    ));
    if !packet.is_empty() {
        let _ = connection.send(&packet);
    }
}

#[cfg(target_os = "macos")]
fn send_fleet_state(
    connection: &mut MidiOutputConnection,
    fleet: FeedbackState,
    transport_started: &mut Option<bool>,
    config: &OpXyFeedbackConfig,
) -> bool {
    let scene = if fleet.needs_attention() {
        config.attention_scene
    } else {
        config.normal_scene
    };
    let scene_sent = send_scene(connection, scene);
    let should_start = fleet.is_active();
    let transport_sent = if *transport_started == Some(should_start) {
        true
    } else {
        let status = if should_start { 0xFA } else { 0xFC };
        match connection.send(&[status]) {
            Ok(()) => {
                *transport_started = Some(should_start);
                true
            }
            Err(_) => false,
        }
    };
    scene_sent && transport_sent
}

#[cfg(target_os = "macos")]
fn send_scene(connection: &mut MidiOutputConnection, one_based_scene: u8) -> bool {
    let value = one_based_scene.clamp(1, 99) - 1;
    connection.send(&[0xB0, 85, value]).is_ok()
}

#[cfg(target_os = "macos")]
fn learn(
    action: MidiAction,
    requested_device: Option<&str>,
    trigger: Option<MidiTrigger>,
) -> Result<()> {
    let path = Paths::discover().midi_file();
    let mut config = MidiConfig::load(&path)?;
    let selector = requested_device.or(config.device.as_deref());
    let mut input = MidiInput::new("construct-midi-learn").context("initialize MIDI input")?;
    input.ignore(Ignore::All);
    let port = find_port(&input, selector)?;
    let port_name = input.port_name(&port).context("read MIDI device name")?;
    eprintln!("listening on {port_name:?}");
    eprintln!("move or press the control for `{action}`…");
    let (tx, rx) = std_mpsc::sync_channel(1);
    let _connection = input
        .connect(
            &port,
            "construct-midi-learn",
            move |_timestamp, bytes, _| {
                if let Some(message) = parse_message(bytes) {
                    let _ = tx.try_send(message);
                }
            },
            (),
        )
        .map_err(|e| anyhow::anyhow!(e.to_string()))
        .with_context(|| format!("connect MIDI device {port_name:?}"))?;
    let message = rx.recv().context("MIDI learn channel closed")?;
    let trigger = trigger.unwrap_or_else(|| infer_trigger(&message));
    let mapping = MidiMapping {
        kind: message.kind,
        channel: message.channel,
        number: message.number,
        trigger,
        action,
    };
    config.device = Some(port_name);
    config.mappings.retain(|existing| {
        !(existing.kind == mapping.kind
            && existing.channel == mapping.channel
            && existing.number == mapping.number
            && existing.trigger == mapping.trigger)
    });
    config.mappings.push(mapping.clone());
    config.mappings.sort_by_key(|m| m.action);
    config.save(&path)?;
    println!(
        "learned {}: {} channel {} number {} ({})",
        action, mapping.kind, mapping.channel, mapping.number, mapping.trigger
    );
    println!("saved {}", path.display());
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn learn(
    _action: MidiAction,
    _requested_device: Option<&str>,
    _trigger: Option<MidiTrigger>,
) -> Result<()> {
    anyhow::bail!("native MIDI control is currently supported on macOS")
}

fn forget(action: MidiAction) -> Result<()> {
    let path = Paths::discover().midi_file();
    let mut config = MidiConfig::load(&path)?;
    let before = config.mappings.len();
    config.mappings.retain(|mapping| mapping.action != action);
    let removed = before - config.mappings.len();
    if removed > 0 {
        config.save(&path)?;
    }
    println!("removed {removed} mapping(s) for {action}");
    Ok(())
}

#[cfg(target_os = "macos")]
fn find_port(input: &MidiInput, selector: Option<&str>) -> Result<MidiInputPort> {
    let ports = input.ports();
    if ports.is_empty() {
        bail!("no MIDI input devices found");
    }
    if let Some(selector) = selector {
        let needle = selector.to_lowercase();
        let matches: Vec<_> = ports
            .iter()
            .filter_map(|port| {
                let name = input.port_name(port).ok()?;
                name.to_lowercase()
                    .contains(&needle)
                    .then(|| (port.clone(), name))
            })
            .collect();
        return match matches.as_slice() {
            [(port, _)] => Ok(port.clone()),
            [] => bail!("no MIDI input device matches {selector:?}; run `construct midi devices`"),
            many => bail!(
                "MIDI device selector {selector:?} is ambiguous: {}",
                many.iter()
                    .map(|(_, name)| name.as_str())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        };
    }
    if ports.len() == 1 {
        return Ok(ports[0].clone());
    }
    bail!("multiple MIDI input devices found; pass `--device <name>`")
}

#[cfg(target_os = "macos")]
fn find_output_port(output: &MidiOutput, selector: &str) -> Result<MidiOutputPort> {
    let needle = selector.to_lowercase();
    let matches: Vec<_> = output
        .ports()
        .into_iter()
        .filter_map(|port| {
            let name = output.port_name(&port).ok()?;
            name.to_lowercase().contains(&needle).then_some((port, name))
        })
        .collect();
    match matches.as_slice() {
        [(port, _)] => Ok(port.clone()),
        [] => anyhow::bail!("no MIDI output device matches {selector:?}"),
        many => anyhow::bail!(
            "MIDI output selector {selector:?} is ambiguous: {}",
            many.iter()
                .map(|(_, name)| name.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn parse_message(bytes: &[u8]) -> Option<MidiMessage> {
    let (&status, data) = bytes.split_first()?;
    if status < 0x80 {
        return None;
    }
    let channel = (status & 0x0f) + 1;
    match status & 0xf0 {
        0x80 => Some(MidiMessage {
            kind: MidiMessageKind::Note,
            channel,
            number: *data.first()? & 0x7f,
            value: *data.get(1)? & 0x7f,
            pressed: false,
        }),
        0x90 => {
            let number = *data.first()? & 0x7f;
            let value = *data.get(1)? & 0x7f;
            Some(MidiMessage {
                kind: MidiMessageKind::Note,
                channel,
                number,
                value,
                pressed: value != 0,
            })
        }
        0xb0 => Some(MidiMessage {
            kind: MidiMessageKind::Cc,
            channel,
            number: *data.first()? & 0x7f,
            value: *data.get(1)? & 0x7f,
            pressed: (*data.get(1)? & 0x7f) >= 64,
        }),
        0xc0 => {
            let program = *data.first()? & 0x7f;
            Some(MidiMessage {
                kind: MidiMessageKind::ProgramChange,
                channel,
                number: program,
                value: program,
                pressed: false,
            })
        }
        _ => None,
    }
}

fn infer_trigger(message: &MidiMessage) -> MidiTrigger {
    match message.kind {
        MidiMessageKind::Note if message.pressed => MidiTrigger::Press,
        MidiMessageKind::Note => MidiTrigger::Release,
        MidiMessageKind::Cc if (1..=63).contains(&message.value) => MidiTrigger::Increase,
        MidiMessageKind::Cc if (65..=127).contains(&message.value) => MidiTrigger::Decrease,
        MidiMessageKind::Cc => MidiTrigger::Any,
        MidiMessageKind::ProgramChange => MidiTrigger::Any,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_note_on_note_off_and_cc_channels() {
        let note = parse_message(&[0x9f, 60, 100]).unwrap();
        assert_eq!(note.kind, MidiMessageKind::Note);
        assert_eq!(note.channel, 16);
        assert_eq!(note.number, 60);
        assert!(note.pressed);

        let note_off = parse_message(&[0x90, 60, 0]).unwrap();
        assert!(!note_off.pressed);

        let cc = parse_message(&[0xb2, 14, 127]).unwrap();
        assert_eq!(cc.kind, MidiMessageKind::Cc);
        assert_eq!(cc.channel, 3);
        assert_eq!(infer_trigger(&cc), MidiTrigger::Decrease);
    }

    #[test]
    fn parses_program_change_and_ignores_clock_and_short_messages() {
        assert!(parse_message(&[0xf8]).is_none());
        let program = parse_message(&[0xc4, 2]).unwrap();
        assert_eq!(program.kind, MidiMessageKind::ProgramChange);
        assert_eq!(program.channel, 5);
        assert_eq!(program.value, 2);
        assert!(parse_message(&[]).is_none());
        assert!(parse_message(&[0x90, 60]).is_none());
    }

    #[test]
    fn trigger_matching_supports_relative_encoders_and_note_edges() {
        let mapping = MidiMapping {
            kind: MidiMessageKind::Cc,
            channel: 1,
            number: 7,
            trigger: MidiTrigger::Decrease,
            action: MidiAction::PreviousSession,
        };
        assert!(mapping.matches(&parse_message(&[0xb0, 7, 127]).unwrap()));
        assert!(!mapping.matches(&parse_message(&[0xb0, 7, 1]).unwrap()));

        let press = MidiMapping {
            kind: MidiMessageKind::Note,
            channel: 1,
            number: 48,
            trigger: MidiTrigger::Press,
            action: MidiAction::Approve,
        };
        assert!(press.matches(&parse_message(&[0x90, 48, 80]).unwrap()));
        assert!(!press.matches(&parse_message(&[0x80, 48, 0]).unwrap()));
    }

    #[test]
    fn op_xy_aux_converts_absolute_cc_changes_after_calibration() {
        let config = OpXyAuxConfig::default();
        let mut state = OpXyAuxState::default();

        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 2, 40]).unwrap()),
            None
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 2, 41]).unwrap()),
            Some(OpXyAuxControl::Down)
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 2, 39]).unwrap()),
            Some(OpXyAuxControl::Up)
        );

        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 3, 127]).unwrap()),
            None
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 3, 0]).unwrap()),
            Some(OpXyAuxControl::ScrollDown)
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 3, 127]).unwrap()),
            Some(OpXyAuxControl::ScrollUp)
        );
    }

    #[test]
    fn op_xy_aux_ignores_unassigned_ccs_and_other_channels() {
        let config = OpXyAuxConfig::default();
        let mut state = OpXyAuxState::default();
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 0, 50]).unwrap()),
            None
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb8, 2, 50]).unwrap()),
            None
        );
    }

    #[test]
    fn op_xy_bank_and_program_changes_navigate_per_channel_after_calibration() {
        let config = op_xy_profile();
        let mut state = OpXyNavigationState::default();

        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb0, 0, 4]).unwrap()),
            None
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb0, 0, 5]).unwrap()),
            Some(OpXyAuxControl::Down)
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb0, 0, 3]).unwrap()),
            Some(OpXyAuxControl::Up)
        );

        assert_eq!(
            state.event_for(&config, &parse_message(&[0xc0, 8]).unwrap()),
            None
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xc0, 9]).unwrap()),
            Some(OpXyAuxControl::ScrollDown)
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xc0, 7]).unwrap()),
            Some(OpXyAuxControl::ScrollUp)
        );

        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb1, 0, 50]).unwrap()),
            None,
            "channel 2 has an independent bank baseline"
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xc1, 50]).unwrap()),
            None,
            "channel 2 has an independent program baseline"
        );
    }

    #[test]
    fn op_xy_bank_and_program_navigation_accepts_channels_one_through_eight_and_ten() {
        let config = op_xy_profile();
        let mut state = OpXyNavigationState::default();

        assert!(state
            .event_for(&config, &parse_message(&[0xb8, 0, 1]).unwrap())
            .is_none());
        assert!(state
            .event_for(&config, &parse_message(&[0xb8, 0, 2]).unwrap())
            .is_none());
        assert!(state
            .event_for(&config, &parse_message(&[0xc8, 1]).unwrap())
            .is_none());
        assert!(state
            .event_for(&config, &parse_message(&[0xc8, 2]).unwrap())
            .is_none());
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 0, 1]).unwrap()),
            None
        );
        assert_eq!(
            state.event_for(&config, &parse_message(&[0xb9, 0, 2]).unwrap()),
            Some(OpXyAuxControl::Down)
        );
    }

    #[test]
    fn op_xy_aux_existing_config_defaults_external_midi_note_channel() {
        let config: OpXyAuxConfig = toml::from_str(
            "enabled = true\nchannel = 10\narrow_cc = 2\nscroll_cc = 3\n",
        )
        .unwrap();
        assert_eq!(config.focused_note_channels, vec![10]);
    }

    #[test]
    fn op_xy_existing_config_defaults_bank_and_program_navigation_channels() {
        let profile: OpXyConfig = toml::from_str(
            "enabled = true\nsession_channels = [1]\ntrack_anchor_notes = [54]\nblack_notes = [54]\n",
        )
        .unwrap();
        assert_eq!(
            profile.navigation_channels,
            vec![1, 2, 3, 4, 5, 6, 7, 8, 10]
        );
        assert_eq!(profile.bank_cc, 0);
    }

    #[test]
    fn op_xy_session_profile_accepts_legacy_field_names() {
        let profile: OpXyConfig = toml::from_str(
            "enabled = true\npane_channels = [1, 2]\npane_anchor_notes = [54, 42]\nsession_notes = [54, 56, 58, 61, 63, 66, 68, 70]\n",
        )
        .unwrap();
        assert_eq!(profile.session_channels, vec![1, 2]);
        assert_eq!(profile.track_anchor_notes, vec![54, 42]);
        assert_eq!(profile.black_notes.len(), 8);

        let feedback: OpXyFeedbackConfig =
            toml::from_str("enabled = true\nsplit_activity_cc = 22\n").unwrap();
        assert_eq!(feedback.track_activity_cc, 22);
    }

    #[test]
    fn config_round_trips_as_readable_toml() {
        let config = MidiConfig {
            device: Some("OP-XY".into()),
            mappings: vec![MidiMapping {
                kind: MidiMessageKind::Note,
                channel: 16,
                number: 60,
                trigger: MidiTrigger::Press,
                action: MidiAction::NewSession,
            }],
            op_xy: None,
        };
        let encoded = toml::to_string_pretty(&config).unwrap();
        assert!(encoded.contains("device = \"OP-XY\""));
        assert!(encoded.contains("action = \"new-session\""));
        assert_eq!(toml::from_str::<MidiConfig>(&encoded).unwrap(), config);
    }

    #[test]
    fn config_save_creates_parent_and_replaces_existing_file() {
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("nested/midi.toml");
        let mut config = MidiConfig {
            device: Some("OP-XY".into()),
            mappings: Vec::new(),
            op_xy: None,
        };
        config.save(&path).unwrap();
        assert_eq!(MidiConfig::load(&path).unwrap(), config);

        config.mappings.push(MidiMapping {
            kind: MidiMessageKind::Cc,
            channel: 16,
            number: 14,
            trigger: MidiTrigger::Increase,
            action: MidiAction::NextSession,
        });
        config.save(&path).unwrap();
        assert_eq!(MidiConfig::load(&path).unwrap(), config);
    }

    #[test]
    fn op_xy_profile_round_trips_as_toml() {
        let config = MidiConfig {
            device: Some("OP-XY Bluetooth".into()),
            mappings: Vec::new(),
            op_xy: Some(OpXyConfig {
                enabled: true,
                session_channels: vec![1, 2, 3, 4, 5, 6, 7, 8],
                track_anchor_notes: vec![54, 42, 30, 54, 54, 54, 54, 54],
                black_notes: vec![54, 56, 58, 61, 63, 66, 68, 70],
                prompt_texts: vec!["Review the current changes.".into()],
                left_note: Some(72),
                down_note: Some(74),
                right_note: Some(76),
                up_note: Some(73),
                enter_note: Some(75),
                no_op_note: Some(71),
                navigation_channels: default_op_xy_navigation_channels(),
                bank_cc: default_op_xy_bank_cc(),
                aux: OpXyAuxConfig::default(),
                feedback: OpXyFeedbackConfig::default(),
            }),
        };

        let encoded = toml::to_string_pretty(&config).unwrap();
        assert_eq!(toml::from_str::<MidiConfig>(&encoded).unwrap(), config);
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn feedback_sender_publishes_only_snapshot_transitions() {
        let (tx, rx) = std_mpsc::channel();
        let feedback = MidiFeedback {
            tx,
            last_snapshot: std::cell::Cell::new(FeedbackSnapshot::default()),
        };

        feedback.update(FeedbackSnapshot::default());
        assert!(rx.try_recv().is_err());

        let working = FeedbackSnapshot {
            fleet: FeedbackState::Working,
            active_slots: 0b0000_0001,
            attention_slots: 0,
            active_tracks: 0b0000_0001,
            attention_tracks: 0,
        };
        feedback.update(working);
        assert_eq!(rx.try_recv().unwrap(), working);
        feedback.update(working);
        assert!(rx.try_recv().is_err());

        let attention = FeedbackSnapshot {
            fleet: FeedbackState::AttentionWorking,
            active_slots: 0b0000_0001,
            attention_slots: 0b1000_0001,
            active_tracks: 0b0000_0011,
            attention_tracks: 0b0000_0010,
        };
        feedback.update(attention);
        assert_eq!(rx.try_recv().unwrap(), attention);
    }

    #[test]
    fn feedback_state_encodes_attention_and_activity_independently() {
        for (state, active, attention) in [
            (FeedbackState::Idle, false, false),
            (FeedbackState::Working, true, false),
            (FeedbackState::AttentionIdle, false, true),
            (FeedbackState::AttentionWorking, true, true),
        ] {
            assert_eq!(state.is_active(), active, "{state:?}");
            assert_eq!(state.needs_attention(), attention, "{state:?}");
        }
    }

    #[test]
    fn feedback_frame_rate_caps_decoded_cc_message_throughput() {
        let idle = FeedbackSnapshot::default();
        assert_eq!(feedback_activity_message_count(idle), 0);
        assert_eq!(
            feedback_frame_period(idle),
            std::time::Duration::from_millis(250)
        );

        let one_mixer_track = FeedbackSnapshot {
            active_slots: 0b0000_0001,
            ..idle
        };
        assert_eq!(feedback_activity_message_count(one_mixer_track), 1);
        assert_eq!(
            feedback_frame_period(one_mixer_track),
            FEEDBACK_MIN_FRAME_PERIOD
        );

        let one_session_and_synth_track = FeedbackSnapshot {
            active_slots: 0b0000_0001,
            active_tracks: 0b0000_0001,
            ..idle
        };
        assert_eq!(feedback_activity_message_count(one_session_and_synth_track), 5);
        assert_eq!(
            feedback_frame_period(one_session_and_synth_track),
            std::time::Duration::from_millis(312)
                + std::time::Duration::from_micros(500)
        );

        let all_tracks = FeedbackSnapshot {
            active_slots: u8::MAX,
            active_tracks: 0b0000_1111,
            ..idle
        };
        assert_eq!(feedback_activity_message_count(all_tracks), 24);
        assert_eq!(
            feedback_frame_period(all_tracks),
            std::time::Duration::from_millis(1500)
        );
    }

    #[test]
    fn track_volume_messages_map_slots_to_channels_one_through_eight() {
        assert_eq!(track_volume_message(0, 127), Some([0xB0, 7, 127]));
        assert_eq!(track_volume_message(7, 64), Some([0xB7, 7, 64]));
        assert_eq!(track_volume_message(8, 64), None);
        assert_eq!(
            slot_volume_packet(0b1000_0001, 64),
            vec![0xB0, 7, 64, 0xB7, 7, 64]
        );
        assert_eq!(
            activity_volume_packet(0b0000_0011, 0b0000_0010, 40, 70),
            vec![0xB0, 7, 40, 0xB1, 7, 70]
        );
        assert_eq!(
            pane_parameter_packet(0b0000_1001, 12, 40),
            vec![
                0xB0, 12, 40, 0xB0, 13, 40, 0xB0, 14, 40, 0xB0, 15, 40, 0xB3, 12, 40,
                0xB3, 13, 40, 0xB3, 14, 40, 0xB3, 15, 40,
            ]
        );
        assert_eq!(
            activity_pane_packet(0b0000_1100, 0b0000_1000, 12, 40, 70),
            vec![
                0xB2, 12, 40, 0xB2, 13, 40, 0xB2, 14, 40, 0xB2, 15, 40, 0xB3, 12, 70,
                0xB3, 13, 70, 0xB3, 14, 70, 0xB3, 15, 70,
            ]
        );
    }

    #[test]
    fn every_action_has_a_stable_kebab_case_name() {
        for action in MidiAction::value_variants() {
            assert_eq!(
                MidiAction::from_str(&action.label(), true).unwrap(),
                *action
            );
        }
    }

    fn op_xy_profile() -> OpXyConfig {
        OpXyConfig {
            enabled: true,
            session_channels: (1..=8).collect(),
            track_anchor_notes: vec![49; 8],
            black_notes: vec![49, 51, 54, 56, 58, 61, 63, 66],
            prompt_texts: Vec::new(),
            left_note: Some(60),
            down_note: Some(62),
            right_note: Some(64),
            up_note: Some(68),
            enter_note: Some(70),
            no_op_note: Some(65),
            navigation_channels: default_op_xy_navigation_channels(),
            bank_cc: default_op_xy_bank_cc(),
            aux: OpXyAuxConfig::default(),
            feedback: OpXyFeedbackConfig::default(),
        }
    }

    #[test]
    fn op_xy_maps_channel_to_session_and_black_key_to_split() {
        let profile = op_xy_profile();
        let message = parse_message(&[0x92, 56, 100]).unwrap();
        assert_eq!(
            profile.event_for(&message),
            Some(OpXyEvent {
                session: 2,
                control: OpXyControl::Split(3),
            })
        );
    }

    #[test]
    fn op_xy_external_midi_notes_reuse_controls_for_focused_pane() {
        let mut profile = op_xy_profile();
        profile.prompt_texts = vec!["focused prompt".into()];

        assert_eq!(
            profile.focused_event_for(&parse_message(&[0x99, 56, 100]).unwrap()),
            Some(OpXyControl::Split(3))
        );
        assert_eq!(
            profile.focused_event_for(&parse_message(&[0x99, 48, 100]).unwrap()),
            Some(OpXyControl::Prompt {
                slot: 0,
                text: "focused prompt".into(),
            })
        );
        assert_eq!(
            profile.focused_event_for(&parse_message(&[0x99, 60, 100]).unwrap()),
            Some(OpXyControl::Left)
        );

        // OP-XY Aux 2 is the internal Punch-In FX track and does not emit
        // these notes. Channel 9 is deliberately not enabled by default.
        assert!(profile
            .focused_event_for(&parse_message(&[0x98, 56, 100]).unwrap())
            .is_none());
        assert!(profile
            .focused_event_for(&parse_message(&[0x88, 56, 0]).unwrap())
            .is_none());
        assert!(profile
            .focused_event_for(&parse_message(&[0x98, 65, 100]).unwrap())
            .is_none());
    }

    #[test]
    fn op_xy_normalizes_each_session_tracks_octave() {
        let mut profile = op_xy_profile();
        profile.session_channels = (1..=8).collect();
        profile.track_anchor_notes = vec![54, 42, 30, 54, 54, 54, 54, 54];
        profile.black_notes = vec![54, 56, 58, 61, 63, 66, 68, 70];

        for (status, note, session) in [(0x90, 56, 0), (0x91, 44, 1), (0x92, 32, 2), (0x93, 56, 3)]
        {
            assert_eq!(
                profile.event_for(&parse_message(&[status, note, 100]).unwrap()),
                Some(OpXyEvent {
                    session,
                    control: OpXyControl::Split(1),
                })
            );
        }
    }

    #[test]
    fn op_xy_maps_white_keys_one_through_six_to_custom_prompts() {
        let mut profile = op_xy_profile();
        profile.session_channels = (1..=8).collect();
        profile.track_anchor_notes = vec![54, 42, 30, 54, 54, 54, 54, 54];
        profile.black_notes = vec![54, 56, 58, 61, 63, 66, 68, 70];
        profile.prompt_texts = (1..=6).map(|slot| format!("prompt {slot}")).collect();

        for (slot, note) in [53, 55, 57, 59, 60, 62].into_iter().enumerate() {
            assert_eq!(
                profile.event_for(&parse_message(&[0x90, note, 100]).unwrap()),
                Some(OpXyEvent {
                    session: 0,
                    control: OpXyControl::Prompt {
                        slot,
                        text: format!("prompt {}", slot + 1),
                    },
                })
            );
        }

        assert_eq!(
            profile.event_for(&parse_message(&[0x91, 41, 100]).unwrap()),
            Some(OpXyEvent {
                session: 1,
                control: OpXyControl::Prompt {
                    slot: 0,
                    text: "prompt 1".into(),
                },
            })
        );
    }

    #[test]
    fn op_xy_leaves_empty_prompt_slots_unassigned() {
        let mut profile = op_xy_profile();
        profile.session_channels = (1..=8).collect();
        profile.track_anchor_notes = vec![54; 8];
        profile.black_notes = vec![54, 56, 58, 61, 63, 66, 68, 70];
        profile.prompt_texts = vec![String::new(), "second".into()];

        assert!(profile
            .event_for(&parse_message(&[0x90, 53, 100]).unwrap())
            .is_none());
        assert_eq!(
            profile.event_for(&parse_message(&[0x90, 55, 100]).unwrap()),
            Some(OpXyEvent {
                session: 0,
                control: OpXyControl::Prompt {
                    slot: 1,
                    text: "second".into(),
                },
            })
        );
    }

    #[test]
    fn op_xy_ignores_release_and_unknown_channels_but_consumes_no_op() {
        let profile = op_xy_profile();
        assert!(profile
            .event_for(&parse_message(&[0x80, 49, 0]).unwrap())
            .is_none());
        assert_eq!(
            profile.event_for(&parse_message(&[0x90, 63, 100]).unwrap()),
            Some(OpXyEvent {
                session: 0,
                control: OpXyControl::NoOp,
            })
        );
        assert!(profile
            .event_for(&parse_message(&[0x90, 65, 100]).unwrap())
            .is_none());
        assert!(profile
            .event_for(&parse_message(&[0x98, 49, 100]).unwrap())
            .is_none());
    }

    #[test]
    fn op_xy_maps_arrow_and_enter_notes() {
        let profile = op_xy_profile();
        for (note, control) in [
            (60, OpXyControl::Left),
            (62, OpXyControl::Down),
            (64, OpXyControl::Right),
            (68, OpXyControl::Up),
            (70, OpXyControl::Enter),
        ] {
            assert_eq!(
                profile.event_for(&parse_message(&[0x97, note, 100]).unwrap()),
                Some(OpXyEvent {
                    session: 7,
                    control,
                })
            );
        }
    }

    #[test]
    fn op_xy_maps_black_keys_five_through_eight_to_session_controls() {
        let profile = op_xy_profile();
        for (note, control) in [
            (58, OpXyControl::CycleFocus),
            (61, OpXyControl::Escape),
            (63, OpXyControl::NoOp),
            (66, OpXyControl::Backspace),
        ] {
            assert_eq!(
                profile.event_for(&parse_message(&[0x93, note, 100]).unwrap()),
                Some(OpXyEvent {
                    session: 3,
                    control,
                })
            );
        }
    }
}
