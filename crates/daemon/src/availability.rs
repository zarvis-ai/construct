//! Real per-harness availability probing (spec 0068).
//!
//! Every built-in wrapper adapter (claude, codex, antigravity, grok)
//! declares `binary = "construct"` — the agentd wrapper itself, which is
//! always the currently-running binary, so it always resolves. Checking
//! *that* binary told a user nothing about whether the CLI it wraps (or a
//! usable smith credential) is actually present. This module probes the
//! thing that actually determines whether a session can start.

use agentd_protocol::adapter::resolve_command_override;
use std::path::PathBuf;
use std::time::{Duration, Instant};

/// Result of probing one harness: whether it's usable right now, plus a
/// short human-readable reason surfaced in the picker, welcome card, and
/// `construct harnesses` CLI output.
#[derive(Debug, Clone)]
pub struct Availability {
    pub available: bool,
    pub detail: String,
}

impl Availability {
    pub(crate) fn ready(detail: impl Into<String>) -> Self {
        Self {
            available: true,
            detail: detail.into(),
        }
    }

    pub(crate) fn missing(detail: impl Into<String>) -> Self {
        Self {
            available: false,
            detail: detail.into(),
        }
    }
}

/// Cache for the two probes that shell out or hit the network (macOS
/// keychain read, Ollama reachability). File-existence and env-var checks
/// are cheap enough to redo on every call and are never cached. There's at
/// most one keychain item and one Ollama endpoint to check daemon-wide, so
/// this is a plain pair of slots rather than a map.
#[derive(Default)]
pub struct AvailabilityCache {
    claude_keychain: Option<(Instant, bool)>,
    ollama: Option<(Instant, bool)>,
}

const CACHE_TTL: Duration = Duration::from_secs(20);
/// Bound on the Ollama TCP probe so a firewalled/unreachable host can never
/// make a `harnesses()` call hang.
const OLLAMA_PROBE_TIMEOUT: Duration = Duration::from_millis(200);

fn cached(slot: Option<(Instant, bool)>) -> Option<bool> {
    slot.and_then(|(at, v)| (at.elapsed() < CACHE_TTL).then_some(v))
}

/// Probe a wrapper adapter that shells out to a named CLI. Honors the same
/// `CONSTRUCT_<H>_CMD` / `CONSTRUCT_<H>_BIN` overrides the adapter itself
/// resolves against at spawn time (see
/// `agentd_protocol::adapter::resolve_command_override`), so the picker
/// never disagrees with what a new session would actually try to run.
pub fn probe_wrapper_cli(command_env: &str, binary_env: &str, default_bin: &str) -> Availability {
    let cmd = resolve_command_override(command_env, binary_env, default_bin);
    if resolve_bin_path(&cmd.bin).is_some() {
        Availability::ready("ready")
    } else {
        Availability::missing(format!("`{}` CLI not found on daemon PATH", cmd.bin))
    }
}

fn resolve_bin_path(bin: &str) -> Option<PathBuf> {
    let p = PathBuf::from(bin);
    if p.is_absolute() {
        return p.exists().then_some(p);
    }
    which::which(bin).ok()
}

/// Probe a non-built-in (community) adapter: available when its configured
/// `binary` resolves, matching the daemon's original (pre-probing) fallback
/// semantics — there's no protocol-level way to ask an arbitrary AHP adapter
/// what it wraps. `resolved` is the same lookup `harnesses()` already runs
/// (via `locate_binary`, which also checks next to the daemon's own exe) to
/// populate `HarnessInfo.binary`; reused here instead of re-resolving.
pub fn probe_generic_adapter(
    binary_spec: &str,
    resolved: Option<&std::path::Path>,
) -> Availability {
    if resolved.is_some() {
        Availability::ready("ready")
    } else {
        Availability::missing(format!("`{binary_spec}` binary not found"))
    }
}

fn env_present(name: &str) -> bool {
    std::env::var(name)
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// Probe the built-in smith harness: available when any credential path
/// smith's own provider selection would pick up actually exists. Mirrors
/// the precedence smith itself uses (explicit model pin, then direct API
/// keys, then OAuth subscriptions, then local Ollama) so the reported
/// status never promises a provider smith wouldn't actually select.
pub async fn probe_smith(cache: &std::sync::Mutex<AvailabilityCache>) -> Availability {
    if env_present("CONSTRUCT_SMITH_MODEL") {
        return Availability::ready("ready (CONSTRUCT_SMITH_MODEL set)");
    }
    if env_present("ANTHROPIC_API_KEY") {
        return Availability::ready("ready (Anthropic API key)");
    }
    if env_present("OPENAI_API_KEY") {
        return Availability::ready("ready (OpenAI API key)");
    }
    if env_present("GEMINI_API_KEY") || env_present("GOOGLE_API_KEY") {
        return Availability::ready("ready (Gemini API key)");
    }
    if env_present("GROK_API_KEY") || env_present("XAI_API_KEY") {
        return Availability::ready("ready (Grok API key)");
    }
    if claude_oauth_credentials_present(cache).await {
        return Availability::ready("ready (Claude subscription)");
    }
    if codex_auth_file().map(|p| p.exists()).unwrap_or(false) {
        return Availability::ready("ready (Codex subscription)");
    }
    if grok_auth_file().map(|p| p.exists()).unwrap_or(false) {
        return Availability::ready("ready (Grok subscription)");
    }
    if ollama_reachable(cache).await {
        return Availability::ready("ready (local Ollama)");
    }
    Availability::missing("no API key or OAuth credential found")
}

/// Existence-only mirror of `CredStore::locate` in
/// `adapter-smith/src/provider/claude_oauth.rs`: explicit file override,
/// then the default credentials file, then the macOS keychain item. No
/// token parsing or refresh here — a probe only needs to know a credential
/// *exists*, not whether it's still valid.
async fn claude_oauth_credentials_present(cache: &std::sync::Mutex<AvailabilityCache>) -> bool {
    if let Ok(p) = std::env::var("CONSTRUCT_CLAUDE_OAUTH_CREDENTIALS") {
        if !p.is_empty() && PathBuf::from(&p).exists() {
            return true;
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if PathBuf::from(home)
            .join(".claude")
            .join(".credentials.json")
            .exists()
        {
            return true;
        }
    }
    if let Some(v) = cached(cache.lock().unwrap().claude_keychain) {
        return v;
    }
    let found = keychain_has_claude_credentials().await;
    cache.lock().unwrap().claude_keychain = Some((Instant::now(), found));
    found
}

const CLAUDE_KEYCHAIN_SERVICE: &str = "Claude Code-credentials";

#[cfg(target_os = "macos")]
async fn keychain_has_claude_credentials() -> bool {
    tokio::process::Command::new("security")
        .args(["find-generic-password", "-s", CLAUDE_KEYCHAIN_SERVICE])
        .output()
        .await
        .map(|out| out.status.success())
        .unwrap_or(false)
}

#[cfg(not(target_os = "macos"))]
async fn keychain_has_claude_credentials() -> bool {
    false
}

fn codex_auth_file() -> Option<PathBuf> {
    if let Ok(dir) = std::env::var("CODEX_HOME") {
        if !dir.is_empty() {
            return Some(PathBuf::from(dir).join("auth.json"));
        }
    }
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".codex").join("auth.json"))
}

fn grok_auth_file() -> Option<PathBuf> {
    let home = std::env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".grok").join("auth.json"))
}

async fn ollama_reachable(cache: &std::sync::Mutex<AvailabilityCache>) -> bool {
    if let Some(v) = cached(cache.lock().unwrap().ollama) {
        return v;
    }
    let base = std::env::var("OLLAMA_HOST")
        .ok()
        .filter(|s| !s.trim().is_empty())
        .unwrap_or_else(|| "http://localhost:11434".to_string());
    let found = match host_port(&base) {
        Some(hostport) => tokio::time::timeout(
            OLLAMA_PROBE_TIMEOUT,
            tokio::net::TcpStream::connect(hostport),
        )
        .await
        .map(|r| r.is_ok())
        .unwrap_or(false),
        None => false,
    };
    cache.lock().unwrap().ollama = Some((Instant::now(), found));
    found
}

/// Strip an optional `scheme://` and path/query suffix, defaulting to port
/// 11434 (Ollama's default) when the remaining host has no explicit port.
fn host_port(base_url: &str) -> Option<String> {
    let without_scheme = base_url.rsplit("://").next().unwrap_or(base_url);
    let host_port = without_scheme.split('/').next().unwrap_or(without_scheme);
    if host_port.is_empty() {
        return None;
    }
    if host_port.contains(':') {
        Some(host_port.to_string())
    } else {
        Some(format!("{host_port}:11434"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_port_defaults_ollama_port() {
        assert_eq!(
            host_port("http://localhost:11434"),
            Some("localhost:11434".to_string())
        );
        assert_eq!(
            host_port("http://example.com"),
            Some("example.com:11434".to_string())
        );
        assert_eq!(
            host_port("example.com:9999"),
            Some("example.com:9999".to_string())
        );
        assert_eq!(host_port(""), None);
    }

    #[test]
    fn probe_wrapper_cli_reports_missing_binary() {
        let unique = format!("construct-test-nonexistent-bin-{}", std::process::id());
        let avail = probe_wrapper_cli(
            "CONSTRUCT_TEST_NONEXISTENT_CMD",
            "CONSTRUCT_TEST_NONEXISTENT_BIN",
            &unique,
        );
        assert!(!avail.available);
        assert!(avail.detail.contains(&unique));
    }

    #[test]
    fn probe_wrapper_cli_finds_binary_on_path() {
        // `sh` is present on every platform this daemon supports.
        let avail = probe_wrapper_cli("CONSTRUCT_TEST_SH_CMD", "CONSTRUCT_TEST_SH_BIN", "sh");
        assert!(avail.available);
    }
}
