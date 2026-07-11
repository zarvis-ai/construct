//! Self-update for the `construct` CLI.
//!
//! `construct upgrade` re-runs the project installer (embedded at build time, so
//! the download + checksum + atomic-replace logic lives in exactly one place)
//! against the directory the running `construct` binary lives in — which, per the
//! install layout, is where all the agentd binaries sit together. A separate,
//! cached, fail-silent check powers the "update available" notice the TUI
//! shows on startup.

use anyhow::{anyhow, Context, Result};
use std::io::{self, IsTerminal, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use construct_client::Client;
use construct_protocol::paths::Paths;

/// GitHub `owner/repo` the release assets and installer come from.
const REPO: &str = "zarvis-ai/construct";
/// The installer, baked in at build time so `construct upgrade` and `install.sh`
/// can never drift apart.
const INSTALL_SH: &str = include_str!("../../../install.sh");
/// How long a cached update check stays fresh before a background refresh.
const CACHE_TTL_MS: u64 = 24 * 60 * 60 * 1000;

fn current_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

fn update_check_disabled() -> bool {
    std::env::var_os("CONSTRUCT_NO_UPDATE_CHECK").is_some()
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
        .get(format!(
            "https://api.github.com/repos/{REPO}/releases/latest"
        ))
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

/// The latest released version, if it is strictly newer than the version
/// this binary was built from, or `None`. Reads only the on-disk cache
/// (instant, never blocks startup); if the cache is stale it kicks off a
/// background refresh for the next launch. Opt out entirely with
/// `CONSTRUCT_NO_UPDATE_CHECK=1`.
///
/// Must be called from within a Tokio runtime (it may spawn the refresh).
pub fn cached_latest_version() -> Option<String> {
    if update_check_disabled() {
        return None;
    }
    let cache = read_cache().unwrap_or_default();
    if now_ms().saturating_sub(cache.checked_at_ms) > CACHE_TTL_MS {
        tokio::spawn(refresh_cache());
    }
    let latest = cache.latest?;
    is_newer(&latest, current_version()).then_some(latest)
}

/// Where a background upgrade spawned via [`spawn_detached_upgrade`]
/// redirects its stdout/stderr. The caller is typically a live TUI whose own
/// stdout is the alternate screen, so the child must never inherit it.
pub fn log_path() -> PathBuf {
    Paths::discover().state_dir.join("upgrade.log")
}

/// Install `version` and restart the daemon at `socket`, out-of-process: this
/// spawns `construct --socket <socket> upgrade --version <version> --restart`
/// as a detached child (stdin null, stdout/stderr redirected to
/// [`log_path`]) and waits for it to exit. Meant to be called from inside a
/// live TUI, where running the installer in-process would print over the
/// alternate screen. Returns a one-line status message describing the
/// outcome; only I/O failures setting up the child (not the child's own exit
/// status) surface as `Err`.
pub async fn spawn_detached_upgrade(version: &str, socket: &Path) -> Result<String> {
    let exe = std::env::current_exe().context("resolve current executable")?;
    let log_out = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(log_path())
        .context("open upgrade log")?;
    let log_err = log_out.try_clone().context("clone upgrade log handle")?;
    let mut child = tokio::process::Command::new(&exe)
        .arg("--socket")
        .arg(socket)
        .arg("upgrade")
        .arg("--version")
        .arg(version)
        .arg("--restart")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log_out))
        .stderr(Stdio::from(log_err))
        .spawn()
        .context("spawn construct upgrade")?;
    let status = child
        .wait()
        .await
        .context("wait for construct upgrade to exit")?;
    if status.success() {
        Ok(format!(
            "upgraded to {version} — daemon restart requested, reconnect when ready"
        ))
    } else {
        Ok(format!(
            "upgrade to {version} failed ({status}) — see {}",
            log_path().display()
        ))
    }
}

// --- interactive startup check -----------------------------------------------

fn release_tag(latest: &str) -> String {
    if latest.trim_start().starts_with('v') {
        latest.trim().to_string()
    } else {
        format!("v{}", latest.trim())
    }
}

/// The version string to show on the right side of the `FROM -> TO` upgrade
/// summary. An explicit `--version` wins; otherwise we use the fetched latest
/// tag, falling back to the literal `"latest"` when we couldn't resolve it
/// (offline / no public release). This is display-only — the installer does the
/// authoritative resolution and download.
fn target_display(explicit: Option<&str>, fetched_latest: Option<&str>) -> String {
    match explicit {
        Some(v) => release_tag(v),
        None => match fetched_latest {
            Some(l) => release_tag(l),
            None => "latest".to_string(),
        },
    }
}

fn wants_yes(answer: &str) -> bool {
    matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Check for a newer release and, when running interactively, ask whether to
/// upgrade now. Returns the original executable path after a successful upgrade
/// so the caller can re-exec and continue under the newly installed binary.
pub async fn prompt_and_upgrade_if_available(socket: &Path) -> Result<Option<PathBuf>> {
    if update_check_disabled() || !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Ok(None);
    }

    let current = current_version();
    let Some(latest) = fetch_latest().await else {
        return Ok(None);
    };
    write_cache(&UpdateCache {
        checked_at_ms: now_ms(),
        latest: Some(latest.clone()),
    });

    if !is_newer(&latest, current) {
        return Ok(None);
    }

    print!(
        "construct {latest} is available (you have {current}). Upgrade now and restart the daemon if it is running? [y/N] "
    );
    let _ = io::stdout().flush();

    let mut answer = String::new();
    if io::stdin().read_line(&mut answer).is_err() || !wants_yes(&answer) {
        return Ok(None);
    }

    let exe = std::env::current_exe().context("resolve current executable before upgrade")?;
    run(Some(release_tag(&latest)), None, true, false, socket).await?;
    Ok(Some(exe))
}

// --- `construct upgrade` -------------------------------------------------------

/// Run the `construct upgrade` subcommand.
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
                println!(
                    "construct {latest} available (you have {current}). Run `construct upgrade`."
                );
            }
            Some(latest) => {
                println!("up to date (construct {current}; latest {latest}).")
            }
            None => {
                println!(
                    "could not determine the latest version (offline, or no public release yet)."
                )
            }
        }
        return Ok(());
    }

    // Install into the directory the running `construct` lives in — that's where
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

    // Resolve the version we're moving TO so the summary line is concrete. An
    // explicit `--version` wins; otherwise resolve "latest" to a real tag (best
    // effort — the installer below does the authoritative fetch). Print the
    // FROM -> TO summary up front, before the download starts.
    let to_display = target_display(version.as_deref(), fetch_latest().await.as_deref());
    let from_display = release_tag(current);
    println!("construct upgrade: {from_display} -> {to_display}");
    println!("Installing into {}", dir.display());

    let mut cmd = tokio::process::Command::new("sh");
    cmd.arg(script.path()).env("CONSTRUCT_BIN_DIR", &dir);
    if let Some(v) = &version {
        cmd.env("CONSTRUCT_VERSION", v);
    }
    let status = cmd.status().await.context("run installer")?;
    if !status.success() {
        anyhow::bail!("upgrade failed (installer exited with {status})");
    }

    // The new binary is on disk, but any already-running daemon/TUI is still
    // executing the old one. Tell the user exactly how to start using the new
    // version, tailored to whether a daemon is currently up.
    println!();
    if restart {
        match Client::connect(socket).await {
            Ok(c) => {
                // The daemon re-execs and drops this connection, so a
                // broken-pipe error here means the restart is in flight —
                // both outcomes count as success.
                let _ = c.daemon_restart(None, false).await;
                println!("Upgraded to {to_display}. Restarted the daemon — the new binary is now live.");
            }
            Err(_) => println!(
                "Upgraded to {to_display}. No daemon was running; the new binary will be used the next time you start one."
            ),
        }
    } else if Client::connect(socket).await.is_ok() {
        println!("Upgraded to {to_display}. A daemon is still running the previous binary.");
        println!("To apply the upgrade, restart the daemon with one of:");
        println!("    construct upgrade --restart   (restart it now)");
        println!("    construct daemon restart      (restart it directly)");
        println!("    /construct restart            (from inside the TUI)");
    } else {
        println!(
            "Upgraded to {to_display}. No daemon is running; the new binary will be used the next time you start one (e.g. `construct`)."
        );
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
    fn yes_prompt_accepts_only_affirmative_answers() {
        assert!(wants_yes("y"));
        assert!(wants_yes("YES\n"));
        assert!(!wants_yes(""));
        assert!(!wants_yes("n"));
        assert!(!wants_yes("sure"));
    }

    #[test]
    fn release_tag_adds_v_prefix_once() {
        assert_eq!(release_tag("0.11.2"), "v0.11.2");
        assert_eq!(release_tag("v0.11.2"), "v0.11.2");
        assert_eq!(release_tag(" 0.11.2 "), "v0.11.2");
    }

    #[test]
    fn target_display_prefers_explicit_then_latest_then_literal() {
        // Explicit --version wins and is normalized to a `v`-prefixed tag.
        assert_eq!(target_display(Some("0.12.0"), Some("0.13.0")), "v0.12.0");
        assert_eq!(target_display(Some("v0.12.0"), None), "v0.12.0");
        // No explicit version: use the fetched latest tag.
        assert_eq!(target_display(None, Some("0.12.0")), "v0.12.0");
        // Offline / unresolved latest falls back to the literal "latest".
        assert_eq!(target_display(None, None), "latest");
    }
}
