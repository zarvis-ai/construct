//! Slash-command registry — the single source of truth for the harness's
//! `/`-commands, shared across processes by the adapter and the client.
//!
//! ## Why this exists
//!
//! Slash commands used to be described in *three* hand-synchronized places:
//! the smith adapter's popup array (`SLASH_COMMANDS`), the adapter's
//! after-submit `match trimmed { "/reset" => … }` ladder, and the TUI
//! client's `run_slash_command` table. Each consumer re-derived a command's
//! behavior from its *name string*, which is both fragile ("keep these in
//! lockstep") and the reason UI control commands leaked into the model-facing
//! transcript (nobody had a single place to say "this one is model-invisible").
//!
//! This module replaces the strings with **typed dimensions**. A command is
//! parsed into a [`CommandId`] exactly once, at the input edge; every decision
//! after that — routing, persistence, model-visibility, rendering, the popup
//! list — is a match on the id or a read of a [`SlashCommand`] descriptor
//! field, never another string comparison.
//!
//! The *behavior* (what `/compact` actually does) still lives in per-process
//! handlers keyed by [`CommandId`]; this table owns only the cross-cutting
//! *policy* that multiple consumers need to agree on.

use serde::{Deserialize, Serialize};

/// Stable identity of a slash command — the dispatch key. Parsing resolves a
/// name (and aliases) to one of these once; downstream code matches on the
/// id, never the raw string. Travels on the wire inside
/// [`crate::SessionEvent::ClientCommand`], so it derives `Serialize`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum CommandId {
    Agentd,
    Border,
    Compact,
    Help,
    Loop,
    Model,
    New,
    Operator,
    Quit,
    RemoteControl,
    Reset,
    Refresh,
    Rename,
    Send,
    Tasks,
    Zoom,
}

/// Who carries out the command — the first thing the dispatcher branches on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Routing {
    /// Handled inside the adapter's own loop; mutates harness state and never
    /// leaves the process (`/model`, `/reset`, `/compact`).
    Adapter,
    /// Translated into a real tool call that flows through the daemon's tool
    /// machinery (`/loop` → `agentd_loop_create`).
    ToolCall,
    /// Delegated to the attached client as a UI action (`/zoom`, `/quit`,
    /// `/remote-control`, …). Emitted as [`crate::SessionEvent::ClientCommand`].
    Client,
}

/// Whether any model ever sees a trace of this command.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ModelVisibility {
    /// Never — pure control/UI noise. A reading model (via
    /// `agentd_get_transcript`) must not see it.
    Hidden,
    /// The verb itself is invisible, but its *effect* (a domain event such as
    /// `ContextCompacted`) is recorded and may legitimately reach a model.
    EffectOnly,
}

/// How the daemon records the command in the durable transcript.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TranscriptPolicy {
    /// Don't persist the command at all.
    Omit,
    /// Persist as a control event for forensics, but filter it out of
    /// `agentd_get_transcript` so it never reaches a reading model.
    AuditOnly,
    /// The command isn't recorded as such; the resulting domain event
    /// (`Reset`, `ContextCompacted`, `Status`) is the durable record.
    Effect,
}

/// How a client surfaces the command in its UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Render {
    /// Nothing — the user already saw it happen live.
    Hidden,
    /// A dim system breadcrumb row ("› /zoom").
    SystemNote,
    /// A dedicated card/banner (e.g. the compaction summary).
    Banner,
}

/// Argument arity — drives parse validation and the popup hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Args {
    None,
    Optional,
    Required,
}

/// Declarative descriptor for one slash command. The `name`/`aliases` strings
/// are used only for *parsing input* and *labeling the popup*; they are never
/// a branch condition. Everything else is a typed policy a consumer reads.
#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    pub id: CommandId,
    /// Canonical name including the leading slash, e.g. `"/compact"`.
    pub name: &'static str,
    /// Extra accepted spellings (without slash), e.g. `["fullscreen"]`.
    pub aliases: &'static [&'static str],
    pub args: Args,
    pub routing: Routing,
    pub visibility: ModelVisibility,
    pub transcript: TranscriptPolicy,
    pub render: Render,
    /// One-line description for `/help` and the popup.
    pub help: &'static str,
    /// Whether the command appears in the `/` completion popup.
    pub in_popup: bool,
}

/// The registry. One row per [`CommandId`]; consumers iterate or look up by
/// id/name rather than maintaining parallel lists.
///
/// Variants are written fully-qualified on purpose: glob-importing several
/// enums' variants collides on shared names (`None`, `Hidden`), so the table
/// spells each out — verbose, but every cell reads as the policy it sets.
pub const COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        id: CommandId::Model,
        name: "/model",
        aliases: &[],
        args: Args::Optional,
        routing: Routing::Adapter,
        visibility: ModelVisibility::EffectOnly,
        transcript: TranscriptPolicy::Effect, // emits Status with the new provider:model
        render: Render::SystemNote,
        help: "Show or switch the active provider:model",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Reset,
        name: "/reset",
        aliases: &[],
        args: Args::None,
        routing: Routing::Adapter,
        visibility: ModelVisibility::EffectOnly,
        transcript: TranscriptPolicy::Effect, // emits Reset (truncates transcript)
        render: Render::SystemNote,
        help: "Clear the conversation and start fresh",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Compact,
        name: "/compact",
        aliases: &[],
        args: Args::Optional,
        routing: Routing::Adapter,
        visibility: ModelVisibility::EffectOnly,
        transcript: TranscriptPolicy::Effect, // emits ContextCompacted
        render: Render::Banner,
        help: "Summarize older turns to free context (/compact [N])",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Loop,
        name: "/loop",
        aliases: &[],
        args: Args::Optional,
        routing: Routing::ToolCall,
        visibility: ModelVisibility::EffectOnly, // surfaces as agentd_loop_create
        transcript: TranscriptPolicy::Effect,
        render: Render::SystemNote,
        help: "Re-run a prompt on a schedule (/loop [interval] [prompt])",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Operator,
        name: "/operator",
        aliases: &[],
        args: Args::Required,
        routing: Routing::Adapter,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly,
        render: Render::SystemNote,
        help: "Control the operator ambient loop (/operator enable|disable)",
        in_popup: true,
    },
    // --- client-routed UI actions (formerly the `tui` ToolUse hack) ---
    SlashCommand {
        id: CommandId::Zoom,
        name: "/zoom",
        aliases: &["fullscreen"],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly,
        render: Render::SystemNote,
        help: "Toggle full-screen for the focused pane",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::New,
        name: "/new",
        aliases: &["new-session"],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly,
        render: Render::SystemNote,
        help: "Open the new-session prompt",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Send,
        name: "/send",
        aliases: &["send-input"],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly,
        render: Render::SystemNote,
        help: "Open the send-input prompt",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Rename,
        name: "/rename",
        aliases: &[],
        args: Args::Optional,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly,
        render: Render::SystemNote,
        help: "Rename the focused session",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Tasks,
        name: "/tasks",
        aliases: &[],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly,
        render: Render::SystemNote,
        help: "Open the tasks popup",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Refresh,
        name: "/refresh",
        aliases: &[],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::Omit, // pure view refresh, not worth an audit row
        render: Render::Hidden,
        help: "Reload the session list and transcript",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Border,
        name: "/border",
        aliases: &[],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::Omit,
        render: Render::SystemNote,
        help: "Toggle pane side borders",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::RemoteControl,
        name: "/remote-control",
        aliases: &["remote"],
        args: Args::Optional,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly, // forensics: who opened the tunnel, when
        render: Render::SystemNote,
        help: "Start/stop the remote-control tunnel",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Agentd,
        name: "/construct",
        aliases: &[],
        args: Args::Required,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::AuditOnly, // forensics: e.g. `/construct restart`
        render: Render::SystemNote,
        help: "Daemon control (e.g. /construct restart)",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Help,
        name: "/help",
        aliases: &["?"],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::Omit,
        render: Render::Hidden,
        help: "Show the help overlay",
        in_popup: true,
    },
    SlashCommand {
        id: CommandId::Quit,
        name: "/quit",
        aliases: &["exit"],
        args: Args::None,
        routing: Routing::Client,
        visibility: ModelVisibility::Hidden,
        transcript: TranscriptPolicy::Omit,
        render: Render::Hidden,
        help: "Quit the client",
        in_popup: true,
    },
];

impl SlashCommand {
    /// Resolve a typed token (with or without the leading `/`, case-insensitive)
    /// to its descriptor, honoring aliases. Returns `None` for unknown verbs —
    /// the caller decides whether that's an error or passes through to the model.
    pub fn resolve(token: &str) -> Option<&'static SlashCommand> {
        let verb = token.trim().trim_start_matches('/').to_ascii_lowercase();
        if verb.is_empty() {
            return None;
        }
        COMMANDS.iter().find(|c| {
            c.name.trim_start_matches('/').eq_ignore_ascii_case(&verb)
                || c.aliases.iter().any(|a| a.eq_ignore_ascii_case(&verb))
        })
    }

    /// Look up a descriptor by its id. Infallible: every [`CommandId`] has
    /// exactly one row (enforced by [`tests`]).
    pub fn by_id(id: CommandId) -> &'static SlashCommand {
        COMMANDS
            .iter()
            .find(|c| c.id == id)
            .expect("every CommandId has a COMMANDS row")
    }
}

/// Names shown in the `/` completion popup, in table order. Replaces the
/// adapter's standalone `SLASH_COMMANDS` array.
pub fn popup_names() -> impl Iterator<Item = &'static str> {
    COMMANDS.iter().filter(|c| c.in_popup).map(|c| c.name)
}

/// Curated model specs shown after `/model `. These are intentionally
/// explicit `provider:model` strings so choosing one also selects the billing /
/// auth path instead of relying on bare-name heuristics. The list is a UI hint,
/// not a validator: `/model <any valid spec>` still accepts newer provider
/// models that are not listed here yet.
pub const MODEL_COMPLETIONS: &[&str] = &[
    // ChatGPT subscription / Codex CLI OAuth path.
    "codex-oauth:gpt-5.5",
    "codex-oauth:gpt-5.4-mini",
    "codex-oauth:gpt-5.3-codex-spark",
    // OpenAI platform API path.
    "openai:gpt-5.5",
    "openai:gpt-5",
    "openai:gpt-5-mini",
    // Claude Code subscription OAuth path.
    "claude-oauth:sonnet",
    "claude-oauth:opus",
    // Anthropic API path.
    "anthropic:claude-opus-4-8",
    "anthropic:claude-sonnet-4-6",
    "anthropic:claude-haiku-4-5",
    // Google Gemini API path.
    "gemini:gemini-2.5-pro",
    "gemini:gemini-2.5-flash",
    // Local Ollama examples.
    "ollama:llama3.1",
    "ollama:qwen3-coder",
    // Grok / xAI OAuth path.
    "grok-oauth:grok-4.3",
    "grok-oauth:grok-build-0.1",
];

/// Completion rows for the current `/model` input buffer.
pub fn model_completion_matches(buf: &str) -> Vec<String> {
    let Some(rest) = buf.strip_prefix("/model") else {
        return Vec::new();
    };
    // Only offer model specs once the user is editing the argument position.
    if !rest.starts_with(char::is_whitespace) {
        return Vec::new();
    }
    let arg_prefix = rest.trim_start();
    MODEL_COMPLETIONS
        .iter()
        .copied()
        .filter(|spec| spec.starts_with(arg_prefix))
        .map(|spec| format!("/model {spec}"))
        .collect()
}

/// Whether an event must be withheld from a reading model (i.e. filtered out
/// of `agentd_get_transcript`). True for a [`crate::SessionEvent::ClientCommand`]
/// whose registry [`ModelVisibility`] is `Hidden` — the principled, table-driven
/// replacement for the old "string-sniff the `tui` tool" leak. Everything else
/// is model-visible.
pub fn is_model_hidden(ev: &crate::SessionEvent) -> bool {
    match ev {
        crate::SessionEvent::ClientCommand { id, .. } => {
            SlashCommand::by_id(*id).visibility == ModelVisibility::Hidden
        }
        // Legacy `tui` dispatch tool (the unknown-verb fallback) is the same
        // class of UI-control noise — withhold it too, so the leak is closed
        // regardless of which path a slash command took.
        crate::SessionEvent::ToolUse { tool, .. } => tool == crate::TUI_DISPATCH_TOOL,
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The table is the source of truth, so guard its invariants: every id
    /// appears exactly once, names are well-formed and unique, and the
    /// id↔name mapping round-trips through `resolve`.
    #[test]
    fn table_is_consistent() {
        for c in COMMANDS {
            assert!(c.name.starts_with('/'), "{} missing leading slash", c.name);
            assert_eq!(
                COMMANDS.iter().filter(|o| o.id == c.id).count(),
                1,
                "duplicate id for {}",
                c.name
            );
            assert_eq!(
                COMMANDS.iter().filter(|o| o.name == c.name).count(),
                1,
                "duplicate name {}",
                c.name
            );
            // Name and every alias must resolve back to this exact row.
            assert_eq!(SlashCommand::resolve(c.name).map(|r| r.id), Some(c.id));
            for a in c.aliases {
                assert_eq!(
                    SlashCommand::resolve(a).map(|r| r.id),
                    Some(c.id),
                    "alias {a}"
                );
            }
            assert_eq!(SlashCommand::by_id(c.id).name, c.name);
        }
    }

    #[test]
    fn resolve_is_slash_and_case_insensitive() {
        assert_eq!(
            SlashCommand::resolve("/Zoom").map(|c| c.id),
            Some(CommandId::Zoom)
        );
        assert_eq!(
            SlashCommand::resolve("zoom").map(|c| c.id),
            Some(CommandId::Zoom)
        );
        assert_eq!(
            SlashCommand::resolve("fullscreen").map(|c| c.id),
            Some(CommandId::Zoom)
        );
        assert!(SlashCommand::resolve("/definitely-not-a-command").is_none());
        assert!(SlashCommand::resolve("/").is_none());
    }

    #[test]
    fn model_completions_are_offered_after_model_command() {
        assert!(model_completion_matches("/mod").is_empty());
        assert!(model_completion_matches("/model").is_empty());

        let matches = model_completion_matches("/model codex-oauth:gpt-5.");
        assert_eq!(
            matches,
            vec!["/model codex-oauth:gpt-5.5", "/model codex-oauth:gpt-5.4-mini", "/model codex-oauth:gpt-5.3-codex-spark"]
        );

        let matches = model_completion_matches("/model claude-oauth:");
        assert!(matches.contains(&"/model claude-oauth:sonnet".to_string()));
        assert!(matches.contains(&"/model claude-oauth:opus".to_string()));
    }

    /// The leak fix in action: a `/zoom` ClientCommand event is hidden from
    /// the model, while a real assistant message and an effect-bearing command
    /// are not.
    #[test]
    fn is_model_hidden_filters_client_commands() {
        use crate::{MessageRole, SessionEvent};
        assert!(is_model_hidden(&SessionEvent::ClientCommand {
            id: CommandId::Zoom,
            args: None,
        }));
        assert!(!is_model_hidden(&SessionEvent::Message {
            role: MessageRole::Assistant,
            text: "hi".into(),
        }));
        // The legacy `tui` fallback (unknown verbs) is hidden too.
        assert!(is_model_hidden(&SessionEvent::ToolUse {
            tool: crate::TUI_DISPATCH_TOOL.to_string(),
            args: serde_json::json!({ "command": "foo" }),
            call_id: None,
        }));
        assert!(!is_model_hidden(&SessionEvent::ToolUse {
            tool: "read_file".to_string(),
            args: serde_json::json!({}),
            call_id: None,
        }));
        // EffectOnly commands carry their effect via other events, so the
        // (rare) ClientCommand-shaped ones would still be visible — none of
        // ours are client-routed + EffectOnly, but the predicate keys on the
        // visibility field, not the routing, so this stays correct if that
        // ever changes.
    }

    /// The policy that fixes the `agentd_get_transcript` leak: no client-routed
    /// command is ever model-visible.
    #[test]
    fn client_commands_are_model_invisible() {
        for c in COMMANDS.iter().filter(|c| c.routing == Routing::Client) {
            assert_eq!(
                c.visibility,
                ModelVisibility::Hidden,
                "{} is client-routed but model-visible",
                c.name
            );
            assert_ne!(
                c.transcript,
                TranscriptPolicy::Effect,
                "{} should not masquerade as a domain effect",
                c.name
            );
        }
    }
}
