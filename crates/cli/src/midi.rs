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
use midir::{Ignore, MidiInput, MidiInputConnection, MidiInputPort};
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

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(default)]
pub struct MidiConfig {
    /// Case-insensitive device-name substring. Learn stores the full name.
    pub device: Option<String>,
    pub mappings: Vec<MidiMapping>,
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

#[cfg(target_os = "macos")]
pub(crate) fn start_listener() -> Result<Option<(MidiListener, mpsc::UnboundedReceiver<MidiAction>)>>
{
    let path = Paths::discover().midi_file();
    let config = MidiConfig::load(&path)?;
    if config.mappings.is_empty() {
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
    let (tx, rx) = mpsc::unbounded_channel();
    let connection = input
        .connect(
            &port,
            "construct-midi-control",
            move |_timestamp, bytes, _| {
                let Some(message) = parse_message(bytes) else {
                    return;
                };
                for mapping in &mappings {
                    if mapping.matches(&message) {
                        let _ = tx.send(mapping.action);
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
pub(crate) fn start_listener() -> Result<Option<(MidiListener, mpsc::UnboundedReceiver<MidiAction>)>>
{
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
        return Ok(());
    }
    for mapping in config.mappings {
        println!(
            "{:<18} {} ch={} number={} trigger={}",
            mapping.action, mapping.kind, mapping.channel, mapping.number, mapping.trigger
        );
    }
    Ok(())
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
    fn every_action_has_a_stable_kebab_case_name() {
        for action in MidiAction::value_variants() {
            assert_eq!(
                MidiAction::from_str(&action.label(), true).unwrap(),
                *action
            );
        }
    }
}
