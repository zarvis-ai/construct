//! Remote-control state: the auth token that gates the WebSocket
//! transport, and (in Phase 1C) the public tunnel URL once it's
//! discovered. Lives behind an `Arc` so the WS upgrade handler, the
//! tunnel subprocess monitor, and any future status-display path can
//! all share one view of "what is the current remote URL + token?".
//!
//! Token model is intentionally simple for Phase 1: one daemon-
//! lifetime token, minted at startup, required in the WS upgrade URL
//! path (`/t/<token>`). No per-session scoping yet, no rotation yet.
//! Both are reasonable follow-ups once we have a web client driving
//! real usage and can see what the access patterns are.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU16, AtomicU32, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use uuid::Uuid;

/// Fixed HTTP Basic auth username. Browsers prompt for both a
/// username and password; rather than ignoring whichever the user
/// types (which leaves them guessing whether it matters), we pin
/// it to a known value and the popup tells the user exactly what
/// to type. Same value for every daemon — only the password
/// rotates per `RemoteState`.
pub const REMOTE_USERNAME: &str = "remote";

/// Shared handle to remote-control state. Cheap to clone (one `Arc`
/// for the inner state). The tunnel URL (set after cloudflared starts)
/// needs async access.
#[derive(Clone)]
pub struct RemoteState {
    /// Legacy URL token retained in snapshots for backward-compatible
    /// deserialization and restart adoption. The web UI is gated by Basic auth.
    token: Arc<String>,
    /// HTTP Basic auth password. Auto-generated in the `swift-fox-77`
    /// shape so it's easy to read out loud / type on a phone. User can
    /// override via `/remote-control <password>` to set their own.
    password: Arc<String>,
    tunnel_url: Arc<RwLock<Option<String>>>,
    /// Active remote WS connection count. Bumped on accept,
    /// decremented when the connection task drops. Read by the
    /// `remote/state` broadcast path on every change so local
    /// clients (e.g. the desktop TUI) can show a "remote attached"
    /// badge without polling.
    clients: Arc<AtomicUsize>,
    /// PID of the cloudflared subprocess (or 0 when unknown / not
    /// running). Captured at spawn time and persisted to
    /// `remote.json` so a restart-and-adopt path can check whether
    /// the still-running tunnel can be reused. Atomic because the
    /// tunnel supervisor may respawn cloudflared mid-life.
    tunnel_pid: Arc<AtomicU32>,
    /// Local WS port the listener is bound to. Set once at install
    /// time (`with_port`) and read by `persist()` so each
    /// snapshot write knows the port without callers having to
    /// thread it through every mutator. Atomic so the install
    /// step doesn't need a `&mut self`.
    port: Arc<AtomicU16>,
    /// On-disk snapshot file. Cloning is cheap (Arc<PathBuf>) and
    /// every mutator (`set_tunnel_url`, `set_tunnel_pid`) writes
    /// through to this path so an `exec()`-and-restart picks up a
    /// fresh snapshot. `None` means "don't persist" — used by
    /// the unit tests that don't want a touched filesystem.
    snapshot_path: Arc<Option<PathBuf>>,
}

/// On-disk representation of `RemoteState`. Loaded at startup
/// before any new state is minted — if a recent snapshot exists
/// AND the cloudflared PID it names is still alive, the new
/// daemon restores the token/password/URL/port instead of minting
/// fresh ones. That's what makes `/agentd restart` preserve the
/// remote URL + password across the restart gap.
///
/// Versioned so future field additions (e.g. per-session token
/// scopes) can be migrated.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteSnapshot {
    pub version: u32,
    pub token: String,
    pub password: String,
    pub port: u16,
    #[serde(default)]
    pub tunnel_url: Option<String>,
    /// PID of the cloudflared subprocess at snapshot time. 0 means
    /// "no tunnel was running" (debug-mode `/remote-control debug`).
    /// The restoring daemon `kill(pid, 0)`s this to verify
    /// liveness before adopting.
    #[serde(default)]
    pub tunnel_pid: u32,
    /// Unix seconds when the snapshot was last written. Snapshots
    /// older than the daemon-defined freshness window are ignored
    /// at startup — a stale `remote.json` from a long-dead daemon
    /// shouldn't grant access on next boot.
    pub generated_at: u64,
}

impl RemoteSnapshot {
    pub const CURRENT_VERSION: u32 = 1;

    /// Read a snapshot from `path`. Returns `Ok(None)` if the file
    /// doesn't exist (a non-error: fresh daemon). Returns `Err` if
    /// the file exists but is malformed.
    pub fn read(path: &Path) -> std::io::Result<Option<Self>> {
        let bytes = match std::fs::read(path) {
            Ok(b) => b,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(e),
        };
        let snap: RemoteSnapshot = serde_json::from_slice(&bytes)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        Ok(Some(snap))
    }

    /// Atomic write via `tmp + rename`. Failures are best-effort
    /// logged at the call site — losing a snapshot only degrades
    /// the next restart to "mint fresh credentials", never breaks
    /// anything.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = path.with_extension("json.tmp");
        let bytes = serde_json::to_vec_pretty(self)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
        std::fs::write(&tmp, &bytes)?;
        std::fs::rename(&tmp, path)
    }

    pub fn fresh_enough(&self, now: u64, max_age_secs: u64) -> bool {
        // generated_at is recorded by us; if the clock skews
        // backwards between snapshot + read, treat as fresh
        // (saturating sub).
        now.saturating_sub(self.generated_at) <= max_age_secs
    }
}

impl RemoteState {
    /// Mint fresh state with an auto-generated password. Called once
    /// per active remote-control session (re-minted after
    /// `/remote-stop` + `/remote-control`). A legacy URL token is still
    /// generated for snapshot compatibility, but Basic auth is the web
    /// UI gate. Snapshot path defaults to `None` — call
    /// `with_snapshot_path` to install one if persistence is desired.
    pub fn new() -> Self {
        Self::with_password(None)
    }

    /// Mint with a caller-provided password (the user-supplied
    /// override from `/remote-control <password>`) — or `None` to
    /// auto-generate a memorable 3-token string.
    pub fn with_password(password: Option<String>) -> Self {
        let token = Uuid::new_v4().simple().to_string();
        let password = password.unwrap_or_else(generate_memorable_password);
        Self {
            token: Arc::new(token),
            password: Arc::new(password),
            tunnel_url: Arc::new(RwLock::new(None)),
            clients: Arc::new(AtomicUsize::new(0)),
            tunnel_pid: Arc::new(AtomicU32::new(0)),
            port: Arc::new(AtomicU16::new(0)),
            snapshot_path: Arc::new(None),
        }
    }

    /// Restore from a previously-persisted snapshot. Used by the
    /// `/agentd restart` path: when the new daemon starts and finds
    /// a fresh `remote.json` whose `tunnel_pid` is still alive, it
    /// constructs a `RemoteState` from the snapshot instead of
    /// minting a new one — that's what preserves the URL + password
    /// across the restart.
    pub fn from_snapshot(snap: &RemoteSnapshot) -> Self {
        Self {
            token: Arc::new(snap.token.clone()),
            password: Arc::new(snap.password.clone()),
            tunnel_url: Arc::new(RwLock::new(snap.tunnel_url.clone())),
            clients: Arc::new(AtomicUsize::new(0)),
            tunnel_pid: Arc::new(AtomicU32::new(snap.tunnel_pid)),
            port: Arc::new(AtomicU16::new(snap.port)),
            snapshot_path: Arc::new(None),
        }
    }

    /// Record the listening port. Called once at bind time.
    /// Triggers a persist so the snapshot file is up-to-date
    /// immediately after the listener comes up.
    pub async fn set_port(&self, port: u16) {
        self.port.store(port, Ordering::SeqCst);
        self.persist().await;
    }

    pub fn port(&self) -> u16 {
        self.port.load(Ordering::SeqCst)
    }

    /// Install a snapshot path on an existing state. The state
    /// writes through to this path on every mutator call. Called
    /// once by the supervisor when a `RemoteState` is installed
    /// (boot, `/remote-control` start, or restore-from-snapshot).
    pub fn with_snapshot_path(mut self, path: PathBuf) -> Self {
        self.snapshot_path = Arc::new(Some(path));
        self
    }

    /// Build a snapshot capturing the current in-memory state.
    /// `tunnel_url` is read async-locked.
    pub async fn snapshot(&self) -> RemoteSnapshot {
        let url = self.tunnel_url.read().await.clone();
        RemoteSnapshot {
            version: RemoteSnapshot::CURRENT_VERSION,
            token: (*self.token).clone(),
            password: (*self.password).clone(),
            port: self.port.load(Ordering::SeqCst),
            tunnel_url: url,
            tunnel_pid: self.tunnel_pid.load(Ordering::SeqCst),
            generated_at: unix_now(),
        }
    }

    /// Best-effort persist of the current state to the installed
    /// snapshot path. Logs and swallows IO errors — failing to
    /// persist only degrades the next restart to "mint fresh
    /// credentials". Returns immediately if no snapshot path is
    /// installed or the port is still 0 (not yet bound).
    pub async fn persist(&self) {
        let Some(path) = self.snapshot_path.as_ref().clone() else {
            return;
        };
        if self.port.load(Ordering::SeqCst) == 0 {
            return;
        }
        let snap = self.snapshot().await;
        if let Err(e) = snap.write(&path) {
            tracing::warn!(error = %e, path = %path.display(), "remote snapshot write failed");
        }
    }

    /// Remove any persisted snapshot. Called from `/remote-control
    /// stop` so a subsequent boot doesn't try to adopt a stale
    /// URL. Errors are logged-only.
    pub fn clear_persisted(&self) {
        let Some(path) = self.snapshot_path.as_ref().clone() else {
            return;
        };
        match std::fs::remove_file(&path) {
            Ok(()) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                tracing::warn!(error = %e, path = %path.display(), "remote snapshot delete failed")
            }
        }
    }

    /// Record the PID of the cloudflared subprocess + persist.
    pub async fn set_tunnel_pid(&self, pid: u32) {
        self.tunnel_pid.store(pid, Ordering::SeqCst);
        self.persist().await;
    }

    pub fn tunnel_pid(&self) -> u32 {
        self.tunnel_pid.load(Ordering::SeqCst)
    }

    /// Atomically increment the active-client counter and return the
    /// new value. Called immediately after a WS upgrade succeeds.
    pub fn add_client(&self) -> u32 {
        // `fetch_add` returns the previous value, so `+ 1` is the
        // new count. Saturating to u32::MAX is fine — the daemon
        // would die from socket exhaustion long before that.
        let prev = self.clients.fetch_add(1, Ordering::SeqCst);
        u32::try_from(prev.saturating_add(1)).unwrap_or(u32::MAX)
    }

    /// Atomically decrement the active-client counter and return the
    /// new value. Called from the connection task's `Drop` so it
    /// runs no matter how the task ended (normal close, panic,
    /// network error).
    pub fn sub_client(&self) -> u32 {
        // Underflow guard: a corrupted increment elsewhere shouldn't
        // wrap us to usize::MAX. `fetch_sub` then floor at 0.
        let prev = self.clients.fetch_sub(1, Ordering::SeqCst);
        let new = prev.saturating_sub(1);
        u32::try_from(new).unwrap_or(u32::MAX)
    }

    pub fn client_count(&self) -> u32 {
        u32::try_from(self.clients.load(Ordering::SeqCst)).unwrap_or(u32::MAX)
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    /// Constant-time password compare. Length-mismatch short-circuit isn't
    /// a meaningful leak against the user-chosen passwords we'd accept
    /// (auto-gen passwords are a fixed pattern; user overrides are
    /// already-known length to an attacker observing the wire).
    pub fn password_matches(&self, candidate: &str) -> bool {
        let real = &self.password;
        if candidate.len() != real.len() {
            return false;
        }
        let mut diff: u8 = 0;
        for (a, b) in candidate.bytes().zip(real.bytes()) {
            diff |= a ^ b;
        }
        diff == 0
    }

    /// Update the public tunnel URL. Called by the cloudflared
    /// monitor once it reads the `*.trycloudflare.com` URL out of
    /// the subprocess output. Persists if a snapshot path + port
    /// have been installed.
    pub async fn set_tunnel_url(&self, url: Option<String>) {
        *self.tunnel_url.write().await = url;
        self.persist().await;
    }

    pub async fn tunnel_url(&self) -> Option<String> {
        self.tunnel_url.read().await.clone()
    }
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// `kill(pid, 0)`: does this process exist + is it signalable by
/// us? Returns false on PID==0 (sentinel) and any error. Used by
/// the boot-time restore path to confirm the `cloudflared`
/// subprocess named in the snapshot is still alive before
/// adopting its URL.
///
/// A pid that exists but is owned by a different user surfaces as
/// `EPERM`; we treat that as "not adoptable" because we can't
/// kill it later for `/remote-control stop` either.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    kill(Pid::from_raw(pid as i32), None).is_ok()
}

impl Default for RemoteState {
    fn default() -> Self {
        Self::new()
    }
}

/// Memorable-password vocabulary. Short, lowercase ASCII, no
/// homophones, no awkward-to-type characters. ~40 × ~40 × 90 =
/// ~144 000 combinations (≈ 17 bits) — *combined* with the
/// 122-bit URL token that's ample (~139 bits). On its own this
/// would be brute-forceable without rate limiting, which is why
/// we don't allow access on password alone — the token must also
/// match on the URL path.
const WORD_ADJ: &[&str] = &[
    "swift", "calm", "bold", "wise", "kind", "lucky", "quick", "merry", "brave", "happy", "sunny",
    "neat", "tidy", "smart", "fresh", "clear", "bright", "warm", "cool", "gentle", "honest",
    "humble", "loyal", "noble", "polite", "quiet", "sharp", "strong", "tall", "tame", "true",
    "tough", "vivid", "fair", "fine", "free", "good", "grand", "great", "jolly",
];

const WORD_NOUN: &[&str] = &[
    "fox", "owl", "cat", "dog", "elk", "bee", "ant", "bug", "wolf", "bear", "hawk", "moth", "frog",
    "duck", "swan", "lark", "deer", "lynx", "mole", "newt", "otter", "robin", "shark", "skunk",
    "snail", "spider", "tiger", "vole", "whale", "yak", "eagle", "raven", "salmon", "panda",
    "moose", "lemur", "horse", "goat", "crab", "hare",
];

/// Build a `<adj>.<noun>.<NN>` style password. Uses random bytes
/// from a fresh UUID-v4 as the index source — we already depend on
/// `uuid` for the token and getting a separate `rand` crate just
/// for this would be overkill.
///
/// Dot (`.`) separator instead of dash (`-`) because most mobile
/// soft keyboards put the dot on the main letter layout while
/// the dash hides behind a numeric layout switch — saves three
/// taps per password entry on a phone.
fn generate_memorable_password() -> String {
    let id = Uuid::new_v4();
    let bytes = id.as_bytes();
    let adj = WORD_ADJ[bytes[0] as usize % WORD_ADJ.len()];
    let noun = WORD_NOUN[bytes[1] as usize % WORD_NOUN.len()];
    let n = (u32::from_be_bytes([bytes[2], bytes[3], bytes[4], bytes[5]]) % 90) + 10;
    format!("{adj}.{noun}.{n}")
}

/// Render `content` as a multi-line Unicode QR code suitable for
/// terminal display. Uses `Dense1x2` half-block cells so the result
/// is roughly square in a typical 1:2-aspect-ratio terminal cell.
/// Returns `None` (with the message logged) if the QR encoder
/// rejects the input — keep callers' code paths simple by letting
/// them substitute a textual fallback.
pub fn render_qr_dense1x2(content: &str) -> Option<String> {
    let code = match qrcode::QrCode::new(content) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!(error = %e, "failed to render QR code");
            return None;
        }
    };
    Some(
        code.render::<qrcode::render::unicode::Dense1x2>()
            .quiet_zone(true)
            .module_dimensions(1, 1)
            .build(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Tunnel URL is settable + readable.
    #[tokio::test]
    async fn tunnel_url_round_trip() {
        let s = RemoteState::new();
        assert_eq!(s.tunnel_url().await, None);
        s.set_tunnel_url(Some("https://x.trycloudflare.com".into()))
            .await;
        assert_eq!(
            s.tunnel_url().await.as_deref(),
            Some("https://x.trycloudflare.com"),
        );
        s.set_tunnel_url(None).await;
        assert_eq!(s.tunnel_url().await, None);
    }
}
