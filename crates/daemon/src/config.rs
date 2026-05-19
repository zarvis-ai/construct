//! Config loading. Looks at `~/.config/agentd/config.toml`, merging built-in
//! adapter defaults underneath.

use agentd_protocol::paths::Paths;
use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::{BTreeMap, HashMap};

pub const DEFAULT_CONFIG_TOML: &str = r#"# agentd configuration
# Each [adapters.<name>] entry registers a harness. The daemon looks up the
# binary in PATH or alongside the daemon binary. Use `binary = "/abs/path"`
# to override.

# [adapters.shell]
# binary = "agentd-adapter-shell"
# description = "Generic shell command runner"

# [adapters.claude]
# binary = "agentd-adapter-claude"
# description = "Claude Code"

# [adapters.codex]
# binary = "agentd-adapter-codex"
# description = "OpenAI Codex"

# [adapters.zarvis.env]
# # Per-harness env vars merged into every spawned session. Lets
# # operators set defaults like the model from config.toml instead
# # of needing to export them in the shell that launches the daemon.
# # Per-session env (`agent new --env KEY=VAL`) takes precedence.
# # AGENTD_ZARVIS_MODEL = "codex-oauth:gpt-5.5"

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
    /// static command palette. Default: `"zarvis"`.
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
            harness: Some("zarvis".to_string()),
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
    /// `agent new --env KEY=VAL` still overrides.
    ///
    /// Example: pin every new zarvis session to use the Codex OAuth
    /// path (subscription-billed) instead of the heuristic fallback:
    ///
    /// ```toml
    /// [adapters.zarvis]
    /// env = { AGENTD_ZARVIS_MODEL = "codex-oauth:gpt-5.5" }
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
    pub description: &'static str,
}

pub const BUILTIN_ADAPTERS: &[BuiltinAdapter] = &[
    BuiltinAdapter {
        name: "shell",
        binary: "agentd-adapter-shell",
        description: "Generic shell command runner",
    },
    BuiltinAdapter {
        name: "claude",
        binary: "agentd-adapter-claude",
        description: "Claude Code (wraps the `claude` CLI)",
    },
    BuiltinAdapter {
        name: "codex",
        binary: "agentd-adapter-codex",
        description: "OpenAI Codex (wraps the `codex` CLI)",
    },
    BuiltinAdapter {
        name: "zarvis",
        binary: "agentd-adapter-zarvis",
        description: "Built-in multi-provider agent (OpenAI / Anthropic / Ollama)",
    },
];

impl Config {
    pub fn load_or_default(paths: &Paths) -> Result<Self> {
        let path = paths.config_file();
        let mut cfg = if path.exists() {
            let s = std::fs::read_to_string(&path)
                .with_context(|| format!("read {}", path.display()))?;
            toml::from_str::<Config>(&s)
                .with_context(|| format!("parse {}", path.display()))?
        } else {
            Self::default()
        };
        // Layer in built-ins so users don't have to declare them.
        for b in BUILTIN_ADAPTERS {
            cfg.adapters
                .entry(b.name.to_string())
                .or_insert_with(|| AdapterConfig {
                    binary: Some(b.binary.to_string()),
                    description: Some(b.description.to_string()),
                    args: Vec::new(),
                    env: HashMap::new(),
                });
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
            [adapters.zarvis]
            binary = "agentd-adapter-zarvis"
            env = { AGENTD_ZARVIS_MODEL = "codex-oauth:gpt-5.5", DEBUG = "1" }
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let zarvis = cfg.adapters.get("zarvis").expect("zarvis adapter");
        assert_eq!(
            zarvis.env.get("AGENTD_ZARVIS_MODEL").map(String::as_str),
            Some("codex-oauth:gpt-5.5"),
        );
        assert_eq!(zarvis.env.get("DEBUG").map(String::as_str), Some("1"));
    }

    /// Omitting `env` is fine — it defaults to empty rather than
    /// failing the parse.
    #[test]
    fn adapter_env_defaults_to_empty() {
        let toml = r#"
            [adapters.zarvis]
            binary = "agentd-adapter-zarvis"
        "#;
        let cfg: Config = toml::from_str(toml).expect("parse");
        let zarvis = cfg.adapters.get("zarvis").expect("zarvis adapter");
        assert!(zarvis.env.is_empty());
    }
}
