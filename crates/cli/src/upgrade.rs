//! Self-update for the `agent` CLI.
//!
//! `agent upgrade` re-runs the project installer (embedded at build time, so
//! the download + checksum + atomic-replace logic lives in exactly one place)
//! against the directory the running `agent` binary lives in — which, per the
//! install layout, is where all the agentd binaries sit together. A separate,
//! cached, fail-silent check powers the "update available" notice the TUI
//! shows on startup.

use anyhow::{anyhow, Context, Result};
use std::path::{Path, PathBuf};
use std::time::Duration;

use agentd_client::Client;
use agentd_protocol::paths::Paths;

/// GitHub `owner/repo` the release assets and installer come from.
const REPO: &str = "zarvis-ai/agentd";
/// The installer, baked in at build time so `agent upgrade` and `install.sh`
/// can never drift apart.
const INSTALL_SH: &str = include_str!("../../../install.sh");
/// How long a cached update check stays fresh before a background refresh.
const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Parse a `MAJOR.MINOR.PATCH` (optionally `v`-prefixed, optionally with a
/// pre-release suffix on the patch) into a comparable tuple. Returns `None`
/// for anything we can't read as a version.
fn parse_ver(s: &str) -> Option<(u64, u64, u64)> {
    let s = s.trim().trim_start_matches('v');
    let mut it = s.split('.');
    let major = it.next()?.parse().ok()?;
    let minor = it.next()?.parse().ok()?;
    // Patch may carry a `-rc1` / `+meta` suffix — take the leading digits.
    let patch = it
        .next()
        .unwrap_or("0")
        .split(|c: char| !c.is_ascii_digit())
        .next()
        .unwrap_or("0")
        .parse()
        .unwrap_or(0);
    Some((major, minor, patch))
}

/// True when `latest` is a strictly newer version than `current`. Unparseable
/// inputs are treated as "not newer" so a weird tag never nags the user.
fn is_newer(latest: &str, current: &str) -> bool {
    match (parse_ver(latest), parse_ver(current)) {
        (Some(l), Some(c)) => l > c,
        _ => false,
    }
}

// --- update-available notice (cached, background-refreshed) ----------------

#[derive(Debug, Default, serde::Serialize, serde::Deserialize)]
struct UpdateCache {
    checked_at_ms: u64,
    latest: Option<String>,
}

fn cache_path() -> PathBuf {
    Paths::discover().state_dir.join("update-check.json")
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

fn read_cache() -> Option<UpdateCache> {
    let raw = std::fs::read_to_string(cache_path()).ok()?;
    serde_json::from_str(&raw).ok()
}

fn write_cache(c: &UpdateCache) {
    let path = cache_path();
    if let Some(dir) = path.parent() {
        let _ = std::fs::create_dir_all(dir);
    }
    if let Ok(raw) = serde_json::to_string(c) {
        let _ = std::fs::write(path, raw);
    }
}

/// Query GitHub for the latest release tag. Returns `None` on any failure
/// (offline, no release yet, repo still private) — callers must treat the
/// absence as "don't know", never as an error.
async fn fetch_latest() -> Option<String> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(3))
        .build()
        .ok()?;
    let resp = client
        .get(format!("https://api.github.com/repos/{REPO}/releases/latest"))
        .header("User-Agent", "agentd-cli")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await
        .ok()?;
    if !resp.status().is_success() {
        return None;
    }
    let v: serde_json::Value = resp.json().await.ok()?;
    let tag = v.get("tag_name")?.as_str()?;
    Some(tag.trim_start_matches('v').to_string())
}

async fn refresh_cache() {
    // Preserve the last known version if this fetch fails, but still bump the
    // timestamp so we don't hammer the API on every launch while offline.
    let prev = read_cache().unwrap_or_default();
    let latest = fetch_latest().await.or(prev.latest);
    write_cache(&UpdateCache {
        checked_at_ms: now_ms(),
        latest,
    });
}

/// A one-line "newer version available" notice for the TUI to surface, or
/// `None`. Reads only the on-disk cache (instant, never blocks startup); if
/// the cache is stale it kicks off a background refresh for the next launch.
/// Opt out entirely with `AGENTD_NO_UPDATE_CHECK=1`.
///
/// Must be called from within a Tokio runtime (it may spawn the refresh).
pub fn cached_update_notice() -> Option<String> {
    if std::env::var_os("AGENTD_NO_UPDATE_CHECK").is_some() {
        return None;
    }
    let cache = read_cache().unwrap_or_default();
    if now_ms().saturating_sub(cache.checked_at_ms) > CACHE_TTL_MS {
        tokio::spawn(refresh_cache());
    }
    notice_for(cache.latest.as_deref()?, current_version())
}

/// The notice string for `latest` vs `current`, or `None` when `latest` is
/// not strictly newer (or unparseable).
fn notice_for(latest: &str, current: &str) -> Option<String> {
    is_newer(latest, current).then(|| {
        format!("agentd {latest} available (you have {current}) — run `agent upgrade`")
    })
}

// --- `agent upgrade` -------------------------------------------------------

/// Run the `agent upgrade` subcommand.
pub async fn run(
    version: Option<String>,
    bin_dir: Option<PathBuf>,
    restart: bool,
    check: bool,
    socket: &Path,
) -> Result<()> {
    let current = current_version();

    if check {
        match fetch_latest().await {
            Some(latest) if is_newer(&latest, current) => {
                println!("agentd {latest} available (you have {current}). Run `agent upgrade`.");
            }
            Some(latest) => println!("up to date (agentd {current}; latest {latest})."),
            None => {
                println!("could not determine the latest version (offline, or no public release yet).")
            }
        }
        return Ok(());
    }

    // Install into the directory the running `agent` lives in — that's where
    // the whole binary set sits, and the daemon resolves adapters as siblings
    // of its own path.
    let dir = match bin_dir {
        Some(d) => d,
        None => std::env::current_exe()
            .context("resolve current executable")?
            .parent()
            .ok_or_else(|| anyhow!("cannot determine the install directory"))?
            .to_path_buf(),
    };

    // Hand the embedded installer the destination + (optional) version and
    // let it do the download, checksum, and atomic replace.
    let mut script = tempfile::Builder::new()
        .prefix("agentd-install-")
        .suffix(".sh")
        .tempfile()
        .context("create temp installer")?;
    {
        use std::io::Write;
        script
            .write_all(INSTALL_SH.as_bytes())
            .context("write temp installer")?;
    }

    let target = version.as_deref().unwrap_or("latest");
    println!("Upgrading agentd to {target} in {}", dir.display());

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg(script.path()).env("AGENTD_BIN_DIR", &dir);
    if let Some(v) = &version {
        cmd.env("AGENTD_VERSION", v);
    }
    let status = cmd.status().await.context("run installer")?;
    if !status.success() {
        anyhow::bail!("upgrade failed (installer exited with {status})");
    }

    if restart {
        match Client::connect(socket).await {
            Ok(c) => {
                // The daemon re-execs and drops this connection, so a
                // broken-pipe error here means the restart is in flight —
                // both outcomes count as success.
                let _ = c.daemon_restart(None).await;
                println!("Requested daemon restart — the upgrade is now live.");
            }
            Err(_) => println!("Upgraded. (No running daemon to restart.)"),
        }
    } else {
        println!("Upgraded. Run `/agentd restart` in the TUI (or restart the daemon) to apply.");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_versions_with_and_without_prefix() {
        assert_eq!(parse_ver("1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_ver("v0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_ver(" v2.0.1 "), Some((2, 0, 1)));
        assert_eq!(parse_ver("0.2"), Some((0, 2, 0)));
        assert_eq!(parse_ver("1.4.0-rc1"), Some((1, 4, 0)));
        assert_eq!(parse_ver("not-a-version"), None);
    }

    #[test]
    fn newer_only_when_strictly_greater() {
        assert!(is_newer("0.2.0", "0.1.0"));
        assert!(is_newer("v1.0.0", "0.9.9"));
        assert!(is_newer("0.1.1", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.1.0"));
        assert!(!is_newer("0.1.0", "0.2.0"));
        // Unparseable inputs never nag.
        assert!(!is_newer("garbage", "0.1.0"));
        assert!(!is_newer("0.2.0", "garbage"));
    }

    #[test]
    fn notice_only_when_a_newer_version_exists() {
        assert_eq!(
            notice_for("0.2.0", "0.1.0").as_deref(),
            Some("agentd 0.2.0 available (you have 0.1.0) — run `agent upgrade`")
        );
        assert_eq!(notice_for("0.1.0", "0.1.0"), None);
        assert_eq!(notice_for("0.1.0", "0.2.0"), None);
    }
}
