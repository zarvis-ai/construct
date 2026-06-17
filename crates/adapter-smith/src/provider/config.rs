//! User-defined named model endpoints ("profiles") for smith.
//!
//! Read from `[smith.models.<name>]` tables in the shared `config.toml`.
//! Each profile pins a wire protocol (`openai` / `anthropic` / `gemini` /
//! `ollama` / `grok`) to its own base URL, credential, and default model — so a
//! single session can switch between many distinct endpoints at runtime
//! via `/model @<name>`, including several OpenAI-compatible vendors plus
//! the real OpenAI API at the same time. The single `OPENAI_BASE_URL`
//! env var can only bind one endpoint; profiles lift that limit.
//!
//! Profiles are always referenced with the explicit `@` prefix and never
//! win bare-name routing — consistent with spec 0028, switching the
//! endpoint/billing path is an explicit act. OAuth-backed providers
//! (`codex-oauth` / `claude-oauth` / `grok-oauth`) are intentionally not
//! configurable here; they have no base-URL/key surface and keep their own
//! prefixes.
//!
//! Example `config.toml`:
//!
//! ```toml
//! [smith.models.deepseek]
//! provider    = "openai"
//! base_url    = "https://api.deepseek.com/v1"
//! api_key_env = "DEEPSEEK_API_KEY"
//! model       = "deepseek-chat"
//!
//! [smith.models.groq-llama]
//! provider    = "openai"
//! base_url    = "https://api.groq.com/openai/v1"
//! api_key_env = "GROQ_API_KEY"
//! model       = "llama-3.3-70b-versatile"
//! ```

use anyhow::{Context, Result};
use serde::Deserialize;
use std::collections::BTreeMap;

/// One `[smith.models.<name>]` entry.
#[derive(Debug, Clone, Deserialize)]
pub struct ModelProfile {
    /// Wire protocol to speak: `openai` | `anthropic` | `gemini` |
    /// `ollama` | `grok`.
    pub provider: String,
    /// Endpoint base URL. Falls back to the wire protocol's default when unset.
    #[serde(default)]
    pub base_url: Option<String>,
    /// Name of the env var holding the API key (preferred — keeps secrets
    /// out of the config file).
    #[serde(default)]
    pub api_key_env: Option<String>,
    /// Inline API key (discouraged; `api_key_env` is preferred).
    #[serde(default)]
    pub api_key: Option<String>,
    /// Default bare model name to send. Overridable per call with
    /// `@<name>:<model>`.
    #[serde(default)]
    pub model: Option<String>,
}

/// Only the `[smith]` table is deserialized; every other top-level key in
/// `config.toml` (`adapters`, `defaults`, `orchestrator`, …) is ignored by
/// serde's default unknown-field handling.
#[derive(Debug, Default, Deserialize)]
struct Root {
    #[serde(default)]
    smith: SmithSection,
}

#[derive(Debug, Default, Deserialize)]
struct SmithSection {
    #[serde(default)]
    models: BTreeMap<String, ModelProfile>,
}

/// Parse the `[smith.models.*]` profiles out of a `config.toml` string.
/// Kept separate from the filesystem read so it is unit-testable.
fn parse(toml_str: &str) -> Result<BTreeMap<String, ModelProfile>> {
    let root: Root = toml::from_str(toml_str).context("parse config.toml")?;
    Ok(root.smith.models)
}

/// Load every declared profile from `config.toml`. Returns an empty map
/// when the file is absent (the common case for users who never declared
/// any). A malformed file surfaces an error so a typo isn't silently
/// swallowed into "no profiles".
pub fn load_all() -> Result<BTreeMap<String, ModelProfile>> {
    let path = agentd_protocol::paths::Paths::discover().config_file();
    if !path.exists() {
        return Ok(BTreeMap::new());
    }
    let s =
        std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    parse(&s).with_context(|| format!("parse {}", path.display()))
}

/// Look up a single profile by name.
pub fn load_profile(name: &str) -> Result<Option<ModelProfile>> {
    let mut all = load_all()?;
    Ok(all.remove(name))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_openai_compatible_profile() {
        let toml = r#"
            [smith.models.deepseek]
            provider    = "openai"
            base_url    = "https://api.deepseek.com/v1"
            api_key_env = "DEEPSEEK_API_KEY"
            model       = "deepseek-chat"
        "#;
        let models = parse(toml).expect("parse");
        let p = models.get("deepseek").expect("deepseek profile");
        assert_eq!(p.provider, "openai");
        assert_eq!(p.base_url.as_deref(), Some("https://api.deepseek.com/v1"));
        assert_eq!(p.api_key_env.as_deref(), Some("DEEPSEEK_API_KEY"));
        assert_eq!(p.model.as_deref(), Some("deepseek-chat"));
        assert!(p.api_key.is_none());
    }

    #[test]
    fn ignores_unrelated_top_level_tables() {
        // A real config.toml carries adapters/defaults/orchestrator — none
        // of which should trip up the [smith.models.*] parse.
        let toml = r#"
            [adapters.smith]
            binary = "construct-adapter-smith"
            env = { CONSTRUCT_SMITH_MODEL = "openai:gpt-5" }

            [defaults]
            worktree = false

            [smith.models.local]
            provider = "ollama"
            base_url = "http://localhost:8080"
            model    = "llama3.1"
        "#;
        let models = parse(toml).expect("parse");
        assert_eq!(models.len(), 1);
        let p = models.get("local").expect("local profile");
        assert_eq!(p.provider, "ollama");
        assert_eq!(p.base_url.as_deref(), Some("http://localhost:8080"));
        // ollama needs no key
        assert!(p.api_key_env.is_none() && p.api_key.is_none());
    }

    #[test]
    fn empty_config_yields_no_profiles() {
        assert!(parse("").expect("parse empty").is_empty());
        assert!(parse("[defaults]\nworktree = true\n")
            .expect("parse")
            .is_empty());
    }

    #[test]
    fn multiple_profiles_coexist() {
        let toml = r#"
            [smith.models.deepseek]
            provider = "openai"
            base_url = "https://api.deepseek.com/v1"
            model    = "deepseek-chat"

            [smith.models.groq]
            provider = "openai"
            base_url = "https://api.groq.com/openai/v1"
            model    = "llama-3.3-70b-versatile"
        "#;
        let models = parse(toml).expect("parse");
        assert_eq!(models.len(), 2);
        assert!(models.contains_key("deepseek"));
        assert!(models.contains_key("groq"));
    }
}
