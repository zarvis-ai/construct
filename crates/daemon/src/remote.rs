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

use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
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
/// for the inner state). The token field is immutable for the
/// lifetime of the daemon, so it's accessible synchronously; only
/// the tunnel URL (set after cloudflared starts) needs async access.
#[derive(Clone)]
pub struct RemoteState {
    /// Auth token. Required (in the URL path) for any WS upgrade.
    /// Constant for the lifetime of one daemon process. Public
    /// because the upgrade callback runs synchronously and needs to
    /// read this without `.await`.
    token: Arc<String>,
    /// HTTP Basic auth password. Defense-in-depth on top of the
    /// 122-bit URL token: a screenshot or terminal-scrollback leak
    /// of the URL alone doesn't grant access without also typing
    /// this. Auto-generated in the `swift-fox-77` shape so it's
    /// easy to read out loud / type on a phone. User can override
    /// via `/remote-control <password>` to set their own.
    password: Arc<String>,
    tunnel_url: Arc<RwLock<Option<String>>>,
    /// Active remote WS connection count. Bumped on accept,
    /// decremented when the connection task drops. Read by the
    /// `remote/state` broadcast path on every change so local
    /// clients (e.g. the desktop TUI) can show a "remote attached"
    /// badge without polling.
    clients: Arc<AtomicUsize>,
}

impl RemoteState {
    /// Mint a fresh state with a new token and an auto-generated
    /// password. Called once per active remote-control session
    /// (re-minted after `/remote-stop` + `/remote-control`).
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
        }
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

    pub fn token(&self) -> &str {
        &self.token
    }

    pub fn password(&self) -> &str {
        &self.password
    }

    /// Constant-time password compare. Same shape as `token_matches`
    /// — length-mismatch short-circuit isn't a meaningful leak
    /// against the user-chosen passwords we'd accept (auto-gen
    /// passwords are a fixed pattern; user overrides are
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

    /// Compare a candidate token to the stored one in constant time.
    /// Returns true only on exact match. Length-mismatch short-
    /// circuits — the wire shape leaks "wrong length" but that's
    /// not a real attacker advantage against 122 bits of UUID-v4
    /// randomness in a known-length token.
    pub fn token_matches(&self, candidate: &str) -> bool {
        let real = &self.token;
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
    /// the subprocess output.
    pub async fn set_tunnel_url(&self, url: Option<String>) {
        *self.tunnel_url.write().await = url;
    }

    pub async fn tunnel_url(&self) -> Option<String> {
        self.tunnel_url.read().await.clone()
    }
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
    "swift", "calm", "bold", "wise", "kind", "lucky", "quick", "merry",
    "brave", "happy", "sunny", "neat", "tidy", "smart", "fresh", "clear",
    "bright", "warm", "cool", "gentle", "honest", "humble", "loyal", "noble",
    "polite", "quiet", "sharp", "strong", "tall", "tame", "true", "tough",
    "vivid", "fair", "fine", "free", "good", "grand", "great", "jolly",
];

const WORD_NOUN: &[&str] = &[
    "fox", "owl", "cat", "dog", "elk", "bee", "ant", "bug",
    "wolf", "bear", "hawk", "moth", "frog", "duck", "swan", "lark",
    "deer", "lynx", "mole", "newt", "otter", "robin", "shark", "skunk",
    "snail", "spider", "tiger", "vole", "whale", "yak", "eagle",
    "raven", "salmon", "panda", "moose", "lemur", "horse", "goat", "crab",
    "hare",
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

/// Extract the token from an HTTP request URI path. Accepts the
/// shape `/t/<token>` (and trailing path segments, ignored). Returns
/// `None` when the path doesn't match.
pub fn token_from_uri_path(path: &str) -> Option<&str> {
    let rest = path.strip_prefix("/t/")?;
    let token = rest.split('/').next()?;
    if token.is_empty() {
        None
    } else {
        Some(token)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Token extraction from URI paths. Strict on the `/t/` prefix
    /// so we don't accidentally accept a request to `/token` (or
    /// any other route the web client might add later).
    #[test]
    fn extracts_token_from_t_path() {
        assert_eq!(token_from_uri_path("/t/abc123"), Some("abc123"));
        assert_eq!(token_from_uri_path("/t/abc123/some/extra"), Some("abc123"));
        assert_eq!(token_from_uri_path("/"), None);
        assert_eq!(token_from_uri_path("/t/"), None);
        assert_eq!(token_from_uri_path("/t"), None);
        assert_eq!(token_from_uri_path("/token/abc123"), None);
        assert_eq!(token_from_uri_path(""), None);
    }

    /// `token_matches` is exact-match only. Empty / wrong-length /
    /// off-by-one inputs all reject.
    #[test]
    fn token_matches_is_exact_only() {
        let s = RemoteState::new();
        let real = s.token().to_string();
        assert!(s.token_matches(&real));
        // Length mismatch.
        assert!(!s.token_matches(&format!("{real}x")));
        assert!(!s.token_matches(&real[..real.len() - 1]));
        // Wrong content of same length.
        let mut wrong = real.clone();
        let first = wrong.remove(0);
        wrong.push(first); // rotate one char
        assert!(!s.token_matches(&wrong));
        // Empty.
        assert!(!s.token_matches(""));
    }

    /// Fresh `RemoteState`s mint independent tokens — no static /
    /// shared state.
    #[test]
    fn fresh_state_mints_unique_tokens() {
        let a = RemoteState::new();
        let b = RemoteState::new();
        assert_ne!(a.token(), b.token());
        // UUID-v4 simple form is 32 hex chars.
        assert_eq!(a.token().len(), 32);
    }

    /// Tunnel URL is settable + readable.
    #[tokio::test]
    async fn tunnel_url_round_trip() {
        let s = RemoteState::new();
        assert_eq!(s.tunnel_url().await, None);
        s.set_tunnel_url(Some("https://x.trycloudflare.com".into())).await;
        assert_eq!(
            s.tunnel_url().await.as_deref(),
            Some("https://x.trycloudflare.com"),
        );
        s.set_tunnel_url(None).await;
        assert_eq!(s.tunnel_url().await, None);
    }
}
