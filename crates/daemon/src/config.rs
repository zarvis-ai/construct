//! Config loading. Looks at `~/.config/construct/config.toml`, merging built-in
//! adapter defaults underneath.

use agentd_protocol::paths::Paths;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

pub const DEFAULT_CONFIG_TOML: &str = r#"# construct configuration
# Each [adapters.<name>] entry registers a harness. The daemon looks up the
# binary in PATH or alongside the daemon binary. Use `binary = "/abs/path"`
# to override.

# [adapters.shell]
# binary = "construct"
# args = ["__adapter", "shell"]
# description = "Generic shell command runner"

# [adapters.claude]
# binary = "construct"
# args = ["__adapter", "claude"]
# description = "Claude Code"

# [adapters.codex]
# binary = "construct"
# args = ["__adapter", "codex"]
# description = "OpenAI Codex"

# [adapters.smith.env]
# # Per-harness env vars merged into every spawned session. Lets
# # operators set defaults like the model from config.toml instead
# # of needing to export them in the shell that launches the daemon.
# # Per-session env (`construct new --env KEY=VAL`) takes precedence.
# # CONSTRUCT_SMITH_MODEL = "codex-oauth:gpt-5.5"
# # or: CONSTRUCT_SMITH_MODEL = "grok-oauth:grok-2-latest"
# #
# smith is the native built-in harness.

# [smith.models.<name>]
# # Named smith endpoint profiles. Reference at runtime with `/model @<name>`
# # (or `--model @<name>`), e.g. `/model @deepseek`. Lets several endpoints of
# # the same wire protocol coexist in one session — unlike the single
# # OPENAI_BASE_URL/ANTHROPIC_BASE_URL/... env vars. `provider` is the wire
# # protocol: openai | anthropic | gemini | grok | ollama.
# #
# # [smith.models.deepseek]
# # provider    = "openai"
# # base_url    = "https://api.deepseek.com/v1"
# # api_key_env = "DEEPSEEK_API_KEY"
# # model       = "deepseek-chat"

# [defaults]
# worktree = false   # default value of session.worktree if not specified
"#;

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub adapters: BTreeMap<String, AdapterConfig>,
    #[serde(default)]
    pub defaults: Defaults,
    #[serde(default)]
    pub orchestrator: OrchestratorConfig,
}

#[derive(Debug, Clone, Deserialize)]
pub struct OrchestratorConfig {
    /// Which harness backs the daemon-created orchestrator session.
    /// `None` (TOML: `harness = ""` or `enabled = false`) disables
    /// the orchestrator entirely — clients then fall back to the
    /// static command palette. Default: `"smith"`.
    #[serde(default)]
    pub harness: Option<String>,
    /// Hard kill switch; set to `false` to disable the orchestrator
    /// even when `harness` is configured. Default: `true`.
    #[serde(default = "default_orchestrator_enabled")]
    pub enabled: bool,
}

fn default_orchestrator_enabled() -> bool {
    true
}

impl Default for OrchestratorConfig {
    fn default() -> Self {
        Self {
            harness: Some("smith".to_string()),
            enabled: true,
        }
    }
}

impl OrchestratorConfig {
    /// The effective harness name when the orchestrator is enabled.
    pub fn effective_harness(&self) -> Option<&str> {
        if !self.enabled {
            return None;
        }
        self.harness.as_deref().filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct AdapterConfig {
    #[serde(default)]
    pub binary: Option<String>,
    #[serde(default)]
    pub description: Option<String>,
    #[serde(default)]
    pub args: Vec<String>,
    /// Extra environment variables to inject when spawning this
    /// adapter. Lets operators set per-harness defaults — e.g. a
    /// default model — from `config.toml` without touching the
    /// shell where the daemon launches. Merged INTO the per-session
    /// `env_with_meta` (see `daemon/src/session.rs`); existing
    /// per-session env takes precedence so an explicit
    /// `construct new --env KEY=VAL` still overrides.
    ///
    /// Example: pin every new smith session to use an explicit
    /// subscription-backed OAuth path instead of the heuristic fallback:
    ///
    /// ```toml
    /// [adapters.smith]
    /// env = { CONSTRUCT_SMITH_MODEL = "codex-oauth:gpt-5.5" }
    /// # or: env = { CONSTRUCT_SMITH_MODEL = "claude-oauth:sonnet" }
    /// # or: env = { CONSTRUCT_SMITH_MODEL = "grok-oauth:grok-2-latest" }
    /// ```
    #[serde(default)]
    pub env: HashMap<String, String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct Defaults {
    #[serde(default)]
    pub worktree: Option<bool>,
}

pub struct BuiltinAdapter {
    pub name: &'static str,
    pub binary: &'static str,
    pub args: &'static [&'static str],
    pub description: &'static str,
}

pub const BUILTIN_ADAPTERS: &[BuiltinAdapter] = &[
    BuiltinAdapter {
        name: "shell",
        binary: "construct",
        args: &["__adapter", "shell"],
        description: "Generic shell command runner",
    },
    BuiltinAdapter {
        name: "claude",
        binary: "construct",
        args: &["__adapter", "claude"],
        description: "Claude Code (wraps the `claude` CLI)",
    },
    BuiltinAdapter {
        name: "codex",
        binary: "construct",
        args: &["__adapter", "codex"],
        description: "OpenAI Codex (wraps the `codex` CLI)",
    },
    BuiltinAdapter {
        name: "antigravity",
        binary: "construct",
        args: &["__adapter", "antigravity"],
        description: "Google Antigravity (wraps the `agy` CLI)",
    },
    BuiltinAdapter {
        name: "smith",
        binary: "construct",
        args: &["__adapter", "smith"],
        description: "Built-in multi-provider agent (OpenAI / Anthropic / Gemini / Ollama / Grok)",
    },
];

impl Config {
    pub fn load_or_default(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        let mut cfg = if path.exists() {
            let s = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            toml::from_str::<Config>(&s).with_context(|| format!("parse {}", path.display()))?
        } else {
            Self::default()
        };
        // Layer in built-ins so users don't have to declare them.
        // Important: we layer at the FIELD level, not the entry level
        // — a user who declared `[adapters.smith] env = {...}` (only
        // to set per-harness env defaults) still needs the builtin
        // `binary` + `description` to fill in. Without this,
        // declaring an `[adapters.<name>]` block to set ONE field
        // would silently drop the builtin's binary path and the
        // daemon would fail with "adapter binary not found" on
        // session create.
        for b in BUILTIN_ADAPTERS {
            let entry = cfg.adapters.entry(b.name.to_string()).or_default();
            if entry.binary.is_none() {
                entry.binary = Some(b.binary.to_string());
            }
            if entry.args.is_empty() {
                entry.args = b.args.iter().map(|s| s.to_string()).collect();
            }
            if entry.description.is_none() {
                entry.description = Some(b.description.to_string());
            }
        }
        Ok(cfg)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `[adapters.<name>].env` parses out of TOML and lands on the
    /// resolved AdapterConfig.
    #[test]
    fn adapter_env_table_parses() {
        let toml = r#"
            [adapters.smith]
            binary = "construct-adapter-smith"
            env = { CONSTRUCT_SMITH_MODEL = "codex-oauth:gpt-5.5", DEBUG = "1" }
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let smith = cfg.adapters.get("smith").expect("smith adapter");
        assert_eq!(
            smith.env.get("CONSTRUCT_SMITH_MODEL").map(String::as_str),
            Some("codex-oauth:gpt-5.5"),
        );
        assert_eq!(smith.env.get("DEBUG").map(String::as_str), Some("1"));
    }

    /// Omitting `env` is fine — it defaults to empty rather than
    /// failing the parse.
    #[test]
    fn adapter_env_defaults_to_empty() {
        let toml = r#"
            [adapters.smith]
            binary = "construct-adapter-smith"
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let smith = cfg.adapters.get("smith").expect("smith adapter");
        assert!(smith.env.is_empty());
    }

    /// REGRESSION: declaring `[adapters.smith] env = {…}` (to set
    /// per-harness env defaults — the motivating use case for that
    /// field) must NOT drop the built-in `binary` / `description`
    /// values that the daemon needs to actually spawn the adapter.
    ///
    /// Before this fix, the BUILTIN_ADAPTERS layer used
    /// `or_insert_with`, which only fired when the entry was
    /// missing entirely. A user-supplied entry that lacked `binary`
    /// got NO `binary` field, the daemon fell back to looking up a
    /// binary named bare `smith` (not `construct-adapter-smith`),
    /// and session create failed with "adapter binary not found".
    #[test]
    fn user_partial_adapter_config_keeps_builtin_binary() {
        let toml = r#"
            [adapters.smith]
            env = { CONSTRUCT_SMITH_MODEL = "codex-oauth:gpt-5.5" }
        "#;
        let mut cfg: Config = toml::from_str(toml).expect("parse");
        // Mimic Config::load's builtin-layering step (the actual
        // load() walks the filesystem, which we'd rather not in
        // tests).
        for b in BUILTIN_ADAPTERS {
            let entry = cfg.adapters.entry(b.name.to_string()).or_default();
            if entry.binary.is_none() {
                entry.binary = Some(b.binary.to_string());
            }
            if entry.args.is_empty() {
                entry.args = b.args.iter().map(|s| s.to_string()).collect();
            }
            if entry.description.is_none() {
                entry.description = Some(b.description.to_string());
            }
        }
        let smith = cfg.adapters.get("smith").expect("smith adapter");
        assert_eq!(
            smith.binary.as_deref(),
            Some("construct"),
            "user-supplied [adapters.smith] with only `env` set must still pick up the built-in binary path",
        );
        // And the user's env stays in place.
        assert_eq!(
            smith.env.get("CONSTRUCT_SMITH_MODEL").map(String::as_str),
            Some("codex-oauth:gpt-5.5"),
        );
    }
}
