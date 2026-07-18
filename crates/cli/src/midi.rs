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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum OpXyControl {
    Session(usize),
    Left,
    Down,
    Right,
    Up,
    Enter,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct OpXyEvent {
    /// Zero-based visual pane position: left-to-right, then top-to-bottom.
    pub pane: usize,
    pub control: OpXyControl,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum MidiInputEvent {
    Action(MidiAction),
    OpXy(OpXyEvent),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FeedbackState {
    Idle,
    Working,
    Attention,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct FeedbackSnapshot {
    pub focused: FeedbackState,
    /// Bit 0 is session slot `[1]`; bit 7 is session slot `[8]`.
    pub active_slots: u8,
    pub attention_slots: u8,
}

impl Default for FeedbackSnapshot {
    fn default() -> Self {
        Self {
            focused: FeedbackState::Idle,
            active_slots: 0,
            attention_slots: 0,
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
}

impl Default for OpXyFeedbackConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            normal_scene: 1,
            attention_scene: 2,
        }
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(default)]
pub struct OpXyConfig {
    pub enabled: bool,
    /// One-based MIDI channels corresponding to visual panes 1 through 4.
    pub pane_channels: Vec<u8>,
    /// First black-key note on each pane track, used to normalize track octaves.
    pub pane_anchor_notes: Vec<u8>,
    pub session_notes: Vec<u8>,
    pub left_note: Option<u8>,
    pub down_note: Option<u8>,
    pub right_note: Option<u8>,
    pub up_note: Option<u8>,
    pub enter_note: Option<u8>,
    /// A sequenced display note that Construct must consume without action.
    pub no_op_note: Option<u8>,
    pub feedback: OpXyFeedbackConfig,
}

impl OpXyConfig {
    fn event_for(&self, message: &MidiMessage) -> Option<OpXyEvent> {
        if !self.enabled || message.kind != MidiMessageKind::Note || !message.pressed {
            return None;
        }
        let pane = self
            .pane_channels
            .iter()
            .position(|channel| *channel == message.channel)?;
        let reference_anchor = self.session_notes.first().copied()?;
        let pane_anchor = self
            .pane_anchor_notes
            .get(pane)
            .copied()
            .unwrap_or(reference_anchor);
        let normalized_note = i16::from(message.number) + i16::from(reference_anchor)
            - i16::from(pane_anchor);
        let normalized_note = u8::try_from(normalized_note).ok().filter(|note| *note <= 127)?;
        if self.no_op_note == Some(normalized_note) {
            return None;
        }
        let control = if let Some(slot) = self
            .session_notes
            .iter()
            .position(|note| *note == normalized_note)
        {
            OpXyControl::Session(slot)
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
        Some(OpXyEvent { pane, control })
    }
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
}

impl fmt::Display for MidiMessageKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Note => f.write_str("note"),
            Self::Cc => f.write_str("cc"),
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
        println!("op-xy: {}", if op_xy.enabled { "enabled" } else { "disabled" });
        println!("  pane channels: {:?}", op_xy.pane_channels);
        println!("  session notes: {:?}", op_xy.session_notes);
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
    let first = capture(
        &rx,
        "Select track 1 and press the first black session key…",
    )?;
    let mut pane_channels = vec![first.channel];
    let mut pane_anchor_notes = vec![first.number];
    for pane in 2..=4 {
        let message = capture(
            &rx,
            &format!("Select track {pane} and press the same first black session key…"),
        )?;
        pane_channels.push(message.channel);
        pane_anchor_notes.push(message.number);
    }
    let mut session_notes = vec![first.number];
    eprintln!("Return to track 1.");
    for slot in 2..=8 {
        session_notes.push(
            capture(&rx, &format!("Press black session key {slot}…"))?.number,
        );
    }
    let left_note = capture(&rx, "Press the LEFT arrow key…")?.number;
    let down_note = capture(&rx, "Press the DOWN arrow key…")?.number;
    let right_note = capture(&rx, "Press the RIGHT arrow key…")?.number;
    let up_note = capture(&rx, "Press the UP arrow key…")?.number;
    let enter_note = capture(&rx, "Press the final black ENTER key…")?.number;
    let no_op_note = capture(
        &rx,
        "Press the white key reserved as the sequencer display no-op…",
    )?
    .number;

    let unique_channels: std::collections::HashSet<_> = pane_channels.iter().copied().collect();
    if unique_channels.len() != 4 {
        anyhow::bail!(
            "the four OP-XY tracks did not produce four distinct MIDI channels; check the linked external-track channel settings and learn again"
        );
    }
    let all_notes = session_notes
        .iter()
        .copied()
        .chain([left_note, down_note, right_note, up_note, enter_note, no_op_note]);
    let unique_notes: std::collections::HashSet<_> = all_notes.clone().collect();
    if unique_notes.len() != all_notes.count() {
        anyhow::bail!(
            "the OP-XY profile captured a key more than once; stop its sequencer and learn again"
        );
    }

    config.device = Some(port_name);
    config.op_xy = Some(OpXyConfig {
        enabled: true,
        pane_channels,
        pane_anchor_notes,
        session_notes,
        left_note: Some(left_note),
        down_note: Some(down_note),
        right_note: Some(right_note),
        up_note: Some(up_note),
        enter_note: Some(enter_note),
        no_op_note: Some(no_op_note),
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

#[cfg(target_os = "macos")]
fn feedback_loop(
    mut connection: MidiOutputConnection,
    rx: std_mpsc::Receiver<FeedbackSnapshot>,
    config: OpXyFeedbackConfig,
) {
    const VOLUME_FRAME_PERIOD: std::time::Duration = std::time::Duration::from_millis(200);
    // CC 7 is 0–127. Active work moves gently through 25–40%; attention
    // performs a two-stage damped bounce through 30–70%.
    const ACTIVE_MOTION: [u8; 8] = [32, 38, 45, 51, 45, 38, 34, 32];
    const ATTENTION_BOUNCE: [u8; 8] = [38, 62, 89, 58, 38, 56, 44, 38];
    let mut snapshot = FeedbackSnapshot::default();
    let mut started = false;
    let mut volume_frame = 0usize;
    let mut next_volume_frame = std::time::Instant::now();
    send_scene(&mut connection, config.normal_scene);
    let _ = connection.send(&[0xFC]);
    send_slot_volumes(&mut connection, u8::MAX, 0);
    loop {
        let timeout = if snapshot.active_slots | snapshot.attention_slots != 0 {
            VOLUME_FRAME_PERIOD
        } else {
            std::time::Duration::from_millis(250)
        };
        match rx.recv_timeout(timeout) {
            Ok(next) => {
                if next.focused != snapshot.focused {
                    match next.focused {
                        FeedbackState::Idle => {
                            send_scene(&mut connection, config.normal_scene);
                            let _ = connection.send(&[0xFC]);
                            started = false;
                        }
                        FeedbackState::Working => {
                            send_scene(&mut connection, config.normal_scene);
                            if !started {
                                let _ = connection.send(&[0xFA]);
                                started = true;
                            }
                        }
                        FeedbackState::Attention => {
                            send_scene(&mut connection, config.attention_scene);
                            if !started {
                                let _ = connection.send(&[0xFA]);
                                started = true;
                            }
                        }
                    }
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
                snapshot = next;
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {}
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                send_slot_volumes(&mut connection, u8::MAX, 0);
                let _ = connection.send(&[0xFC]);
                break;
            }
        }
        let now = std::time::Instant::now();
        if snapshot.active_slots | snapshot.attention_slots != 0 && now >= next_volume_frame {
            send_activity_volumes(
                &mut connection,
                snapshot.active_slots,
                snapshot.attention_slots,
                ACTIVE_MOTION[volume_frame],
                ATTENTION_BOUNCE[volume_frame],
            );
            volume_frame = (volume_frame + 1) % ACTIVE_MOTION.len();
            next_volume_frame = now + VOLUME_FRAME_PERIOD;
        }
    }
}

fn track_volume_message(slot: usize, value: u8) -> Option<[u8; 3]> {
    (slot < 8).then_some([0xB0 | slot as u8, 7, value.min(127)])
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

#[cfg(target_os = "macos")]
fn send_activity_volumes(
    connection: &mut MidiOutputConnection,
    active_slots: u8,
    attention_slots: u8,
    active_value: u8,
    attention_value: u8,
) {
    let packet = activity_volume_packet(
        active_slots,
        attention_slots,
        active_value,
        attention_value,
    );
    if !packet.is_empty() {
        let _ = connection.send(&packet);
    }
}

#[cfg(target_os = "macos")]
fn send_scene(connection: &mut MidiOutputConnection, one_based_scene: u8) {
    let value = one_based_scene.clamp(1, 99) - 1;
    let _ = connection.send(&[0xB0, 85, value]);
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
    if status < 0x80 || data.len() < 2 {
        return None;
    }
    let channel = (status & 0x0f) + 1;
    let number = data[0] & 0x7f;
    let value = data[1] & 0x7f;
    match status & 0xf0 {
        0x80 => Some(MidiMessage {
            kind: MidiMessageKind::Note,
            channel,
            number,
            value,
            pressed: false,
        }),
        0x90 => Some(MidiMessage {
            kind: MidiMessageKind::Note,
            channel,
            number,
            value,
            pressed: value != 0,
        }),
        0xb0 => Some(MidiMessage {
            kind: MidiMessageKind::Cc,
            channel,
            number,
            value,
            pressed: value >= 64,
        }),
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
    fn ignores_clock_program_change_and_short_messages() {
        assert!(parse_message(&[0xf8]).is_none());
        assert!(parse_message(&[0xc0, 2]).is_none());
        assert!(parse_message(&[]).is_none());
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
                pane_channels: vec![5, 6, 7, 8],
                pane_anchor_notes: vec![54, 42, 30, 54],
                session_notes: vec![54, 56, 58, 61, 63, 66, 68, 70],
                left_note: Some(72),
                down_note: Some(74),
                right_note: Some(76),
                up_note: Some(73),
                enter_note: Some(75),
                no_op_note: Some(71),
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
            focused: FeedbackState::Working,
            active_slots: 0b0000_0001,
            attention_slots: 0,
        };
        feedback.update(working);
        assert_eq!(rx.try_recv().unwrap(), working);
        feedback.update(working);
        assert!(rx.try_recv().is_err());

        let attention = FeedbackSnapshot {
            focused: FeedbackState::Working,
            active_slots: 0b0000_0001,
            attention_slots: 0b1000_0001,
        };
        feedback.update(attention);
        assert_eq!(rx.try_recv().unwrap(), attention);
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
            pane_channels: vec![13, 14, 15, 16],
            pane_anchor_notes: vec![49; 4],
            session_notes: vec![49, 51, 54, 56, 58, 61, 63, 66],
            left_note: Some(60),
            down_note: Some(62),
            right_note: Some(64),
            up_note: Some(68),
            enter_note: Some(70),
            no_op_note: Some(65),
            feedback: OpXyFeedbackConfig::default(),
        }
    }

    #[test]
    fn op_xy_maps_channel_to_pane_and_note_to_session_slot() {
        let profile = op_xy_profile();
        let message = parse_message(&[0x9e, 56, 100]).unwrap();
        assert_eq!(
            profile.event_for(&message),
            Some(OpXyEvent {
                pane: 2,
                control: OpXyControl::Session(3),
            })
        );
    }

    #[test]
    fn op_xy_normalizes_each_pane_tracks_octave() {
        let mut profile = op_xy_profile();
        profile.pane_channels = vec![5, 6, 7, 8];
        profile.pane_anchor_notes = vec![54, 42, 30, 54];
        profile.session_notes = vec![54, 56, 58, 61, 63, 66, 68, 70];

        for (status, note, pane) in [(0x94, 56, 0), (0x95, 44, 1), (0x96, 32, 2), (0x97, 56, 3)] {
            assert_eq!(
                profile.event_for(&parse_message(&[status, note, 100]).unwrap()),
                Some(OpXyEvent {
                    pane,
                    control: OpXyControl::Session(1),
                })
            );
        }
    }

    #[test]
    fn op_xy_ignores_release_no_op_and_unknown_channels() {
        let profile = op_xy_profile();
        assert!(profile
            .event_for(&parse_message(&[0x8c, 49, 0]).unwrap())
            .is_none());
        assert!(profile
            .event_for(&parse_message(&[0x9c, 65, 100]).unwrap())
            .is_none());
        assert!(profile
            .event_for(&parse_message(&[0x90, 49, 100]).unwrap())
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
                profile.event_for(&parse_message(&[0x9f, note, 100]).unwrap()),
                Some(OpXyEvent { pane: 3, control })
            );
        }
    }
}
