//! Codex OAuth provider — bills against the user's ChatGPT subscription
//! by reading `~/.codex/auth.json` (the credential file the
//! `codex login` command writes) and routing requests through
//! `POST https://chatgpt.com/backend-api/codex/responses`.
//!
//! This is NOT the public platform API at `api.openai.com/v1/responses`:
//!
//!   - Endpoint host + path differ (`chatgpt.com/backend-api/codex/...`).
//!   - Auth is `Authorization: Bearer <oauth-access-token>` from
//!     `auth.json`, plus a `ChatGPT-Account-ID` header carrying the
//!     `account_id` claim from the OAuth `id_token` JWT.
//!   - Required `OpenAI-Beta` + `originator: codex_cli_rs` + non-default
//!     User-Agent — the default `reqwest/*` UA gets blocked by
//!     Cloudflare (verified against `openai/codex` CLI source).
//!   - The request body uses Responses-API shape (`instructions` +
//!     `input` array), not Chat Completions. Models are Codex-specific
//!     strings (`gpt-5`, `gpt-5-codex`, `gpt-5-codex-mini`).
//!
//! Status: scaffolding + auth.json loader / refresh. Request shaping
//! and SSE parsing land in subsequent commits; `complete()` still
//! returns an "implementation pending" error so the rest of the
//! workspace builds.

use anyhow::{anyhow, Context as _, Result};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{LlmProvider, Message, ProviderTurn, TextSink, ToolSpec};

/// Refresh tokens are good for ~30 days but the server-side window can
/// be tighter under load. We refresh when the access_token is within
/// this much of its expected lifetime to avoid edge races against
/// in-flight requests. Codex CLI uses a similar bias.
const ACCESS_TOKEN_REFRESH_LEEWAY_SECS: i64 = 5 * 60;

/// OAuth `client_id` Codex CLI uses against `auth.openai.com`. Lifted
/// from `openai/codex` (`codex-rs/login/src/auth/manager.rs` —
/// `client_id = "app_EMoamEEZ73f0CkXaXp7hrann"`). Public per the
/// source; safe to embed.
const OPENAI_CLIENT_ID: &str = "app_EMoamEEZ73f0CkXaXp7hrann";

/// Token endpoint that mints/refreshes ChatGPT-subscription OAuth
/// tokens. Same host the `codex login` flow points to.
const OPENAI_TOKEN_URL: &str = "https://auth.openai.com/oauth/token";

/// On-disk shape of `~/.codex/auth.json`. Only the fields we read.
/// Every field is optional / defaulted so a slight schema drift in
/// future codex builds doesn't break us.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AuthDotJson {
    /// Some installations have this; we just preserve it on write.
    #[serde(
        rename = "OPENAI_API_KEY",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub openai_api_key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokens: Option<Tokens>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_refresh: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Tokens {
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub id_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub access_token: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub refresh_token: String,
    /// Codex stores this directly in newer builds; older ones leave it
    /// blank and clients fall back to parsing it out of the `id_token`
    /// JWT. JWT-parsing fallback is a follow-up.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_id: String,
}

/// Returns the path to `auth.json`. Honors `$CODEX_HOME` first, then
/// falls back to `$HOME/.codex/auth.json`. Mirrors what
/// `agentd-adapter-codex` already does to find rollouts so the two
/// crates agree on where codex stores its credential file.
pub fn auth_json_path() -> Result<PathBuf> {
    if let Ok(home) = std::env::var("CODEX_HOME") {
        if !home.is_empty() {
            return Ok(PathBuf::from(home).join("auth.json"));
        }
    }
    let home = std::env::var("HOME")
        .map_err(|_| anyhow!("$HOME is not set; cannot locate ~/.codex/auth.json"))?;
    Ok(PathBuf::from(home).join(".codex").join("auth.json"))
}

/// Read + parse `auth.json`. Returns an error if the file doesn't
/// exist, can't be read, or doesn't carry an OAuth `access_token`
/// (i.e. the user is in API-key mode, not the subscription mode this
/// provider serves).
pub fn load_auth_json(path: &std::path::Path) -> Result<AuthDotJson> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("read {}", path.display()))?;
    let auth: AuthDotJson = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse {} as JSON", path.display()))?;
    let tokens = auth.tokens.as_ref().ok_or_else(|| {
        anyhow!(
            "{} has no `tokens` field. Run `codex login` to authenticate \
             via your ChatGPT subscription before using the codex-oauth \
             provider.",
            path.display()
        )
    })?;
    if tokens.access_token.is_empty() || tokens.refresh_token.is_empty() {
        return Err(anyhow!(
            "{} has empty access_token or refresh_token. Re-run \
             `codex login`.",
            path.display()
        ));
    }
    Ok(auth)
}

/// Write `auth.json` atomically: write to a sibling `.tmp` file, fsync
/// it, then rename over the original. Without this, a crash between
/// truncate and write leaves the file empty and the user is logged
/// out. The same dance Codex CLI does in
/// `codex-rs/login/src/auth/manager.rs`.
pub fn save_auth_json_atomic(
    path: &std::path::Path,
    auth: &AuthDotJson,
) -> Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("mkdir -p {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("json.tmp");
    let json = serde_json::to_vec_pretty(auth).context("serialize auth.json")?;
    {
        let mut f = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&tmp_path)
            .with_context(|| format!("open {}", tmp_path.display()))?;
        f.write_all(&json)
            .with_context(|| format!("write {}", tmp_path.display()))?;
        f.sync_all()
            .with_context(|| format!("fsync {}", tmp_path.display()))?;
    }
    std::fs::rename(&tmp_path, path).with_context(|| {
        format!("rename {} -> {}", tmp_path.display(), path.display())
    })?;
    Ok(())
}

/// In-process refresh coordination. Multiple zarvis sessions in the
/// same adapter process share one of these so only one of them at a
/// time performs the rotation-prone refresh. Cross-process (multiple
/// adapter instances) races remain possible; documented as a known
/// limitation since adding a file lock has its own concurrency edges
/// and the refresh window is narrow.
struct AuthState {
    path: PathBuf,
    auth: AuthDotJson,
}

pub struct CodexOauth {
    state: Arc<Mutex<AuthState>>,
    http: reqwest::Client,
}

impl CodexOauth {
    /// Construct from on-disk credentials. Fails with a clear message
    /// when `auth.json` is missing, in API-key mode, or unreadable.
    pub fn from_env() -> Result<Self> {
        let path = auth_json_path()?;
        let auth = load_auth_json(&path)?;
        let http = reqwest::Client::builder()
            // Cloudflare in front of chatgpt.com rejects the default
            // `reqwest/<ver>` User-Agent on the codex backend path.
            // Use the same identity codex CLI uses so we look like a
            // CLI client, not an anonymous Rust HTTP library.
            .user_agent(format!(
                "codex_cli_rs/{} (agentd zarvis)",
                env!("CARGO_PKG_VERSION")
            ))
            // NOTE: a cookie jar would let cf_clearance challenge
            // cookies stick across the auth-refresh ↔ responses-call
            // sequence (matching codex CLI). Reqwest's `cookie_store`
            // feature isn't enabled workspace-wide. If we hit
            // Cloudflare challenges in practice, enable the
            // `cookies` feature on reqwest in the workspace and add
            // `.cookie_store(true)` here.
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            state: Arc::new(Mutex::new(AuthState { path, auth })),
            http,
        })
    }

    /// Force a token refresh and persist the rotated refresh_token.
    /// Caller is responsible for holding the auth-state mutex; this
    /// fn assumes serialized callers (the LlmProvider impl uses the
    /// mutex around the entire refresh-then-request sequence).
    ///
    /// Returns an actionable error when the refresh_token has been
    /// invalidated server-side — the only fix is for the user to
    /// re-run `codex login`.
    async fn refresh_locked(&self, state: &mut AuthState) -> Result<()> {
        let Some(tokens) = state.auth.tokens.as_ref() else {
            return Err(anyhow!(
                "auth.json lost its tokens between load and refresh; \
                 re-run `codex login`"
            ));
        };
        if tokens.refresh_token.is_empty() {
            return Err(anyhow!(
                "auth.json has no refresh_token; re-run `codex login`"
            ));
        }
        let body = serde_json::json!({
            "client_id": OPENAI_CLIENT_ID,
            "grant_type": "refresh_token",
            "refresh_token": tokens.refresh_token,
            "scope": "openid profile email",
        });
        let resp = self
            .http
            .post(OPENAI_TOKEN_URL)
            .header("Content-Type", "application/json")
            .json(&body)
            .send()
            .await
            .context("POST auth.openai.com/oauth/token")?;
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        if !status.is_success() {
            let body_txt = String::from_utf8_lossy(&bytes).to_string();
            // `refresh_token_reused` / `refresh_token_invalidated` from
            // auth.openai.com is fatal — the only recovery is
            // `codex login` again. Surface it as such instead of
            // letting it look like a transient network failure.
            if body_txt.contains("refresh_token_reused")
                || body_txt.contains("refresh_token_invalidated")
            {
                return Err(anyhow!(
                    "ChatGPT OAuth refresh token has been invalidated \
                     ({status}). Re-run `codex login` to mint fresh \
                     credentials. Server body: {body_txt}"
                ));
            }
            return Err(anyhow!(
                "ChatGPT OAuth refresh failed ({status}): {body_txt}"
            ));
        }
        #[derive(Deserialize)]
        struct RefreshResp {
            #[serde(default)]
            id_token: Option<String>,
            #[serde(default)]
            access_token: Option<String>,
            #[serde(default)]
            refresh_token: Option<String>,
        }
        let r: RefreshResp = serde_json::from_slice(&bytes)
            .context("parse refresh response from auth.openai.com")?;
        // Mutate in place; persist atomically. Codex's `persist_tokens`
        // semantics — only update fields the server returned.
        let tokens = state.auth.tokens.get_or_insert_with(Tokens::default);
        if let Some(t) = r.id_token {
            tokens.id_token = t;
        }
        if let Some(t) = r.access_token {
            tokens.access_token = t;
        }
        if let Some(t) = r.refresh_token {
            tokens.refresh_token = t;
        }
        state.auth.last_refresh = Some(chrono::Utc::now().to_rfc3339());
        save_auth_json_atomic(&state.path, &state.auth)?;
        Ok(())
    }

    /// Decide if a refresh is needed right now. v1 heuristic: refresh
    /// if `last_refresh` is missing OR older than (24h - leeway). We
    /// don't introspect the JWT exp claim yet because decoding JWTs
    /// without a verification key just to read `exp` adds a dep, and
    /// `last_refresh` is enough for the common case. Refining this is
    /// a follow-up.
    fn needs_refresh(auth: &AuthDotJson) -> bool {
        let Some(last) = auth.last_refresh.as_deref() else {
            return true;
        };
        let Ok(last) = chrono::DateTime::parse_from_rfc3339(last) else {
            return true;
        };
        let elapsed = chrono::Utc::now().signed_duration_since(last);
        elapsed.num_seconds() + ACCESS_TOKEN_REFRESH_LEEWAY_SECS
            >= 24 * 60 * 60
    }
}

#[async_trait]
impl LlmProvider for CodexOauth {
    fn name(&self) -> &str {
        "codex-oauth"
    }

    async fn complete(
        &self,
        _model: &str,
        _system: &str,
        _messages: &[Message],
        _tools: &[ToolSpec],
        _sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        // Ensure auth is at least loadable + refresh if stale; the
        // actual responses-call lands in a later commit. This keeps
        // the error surface honest: today the failure mode for
        // anyone trying `codex-oauth:` is "request shape not
        // implemented yet", NOT a silent auth bug they'll have to
        // re-diagnose later.
        let mut state = self.state.lock().await;
        if Self::needs_refresh(&state.auth) {
            self.refresh_locked(&mut state).await?;
        }
        drop(state);
        Err(anyhow!(
            "codex-oauth.complete: auth.json loaded and refresh path \
             verified, but the Responses-API request body and SSE \
             parser are still pending. Tracking: \
             feat/codex-oauth-provider branch."
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Reading a well-formed auth.json with OAuth tokens succeeds.
    #[test]
    fn load_auth_json_round_trips_through_tmp_file() {
        let tmp = std::env::temp_dir().join(format!(
            "agentd-codex-oauth-load-{}.json",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&tmp);
        std::fs::write(
            &tmp,
            r#"{
              "OPENAI_API_KEY": "sk-existing-platform-key",
              "tokens": {
                "id_token": "JWT.eyJh.SIG",
                "access_token": "access-abc",
                "refresh_token": "refresh-xyz",
                "account_id": "acct_123"
              },
              "last_refresh": "2026-05-18T00:00:00Z"
            }"#,
        )
        .unwrap();

        let auth = load_auth_json(&tmp).expect("load");
        let tokens = auth.tokens.as_ref().expect("tokens present");
        assert_eq!(tokens.access_token, "access-abc");
        assert_eq!(tokens.refresh_token, "refresh-xyz");
        assert_eq!(tokens.account_id, "acct_123");
        assert_eq!(
            auth.openai_api_key.as_deref(),
            Some("sk-existing-platform-key")
        );

        let _ = std::fs::remove_file(&tmp);
    }

    /// Missing file → clear actionable error mentioning the path.
    #[test]
    fn load_auth_json_missing_file_errors_with_path() {
        let bogus = std::env::temp_dir().join(format!(
            "agentd-codex-oauth-missing-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_file(&bogus);
        let err = load_auth_json(&bogus).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(bogus.to_string_lossy().as_ref()), "{msg}");
    }

    /// `tokens` field absent → message points the user at
    /// `codex login`. This is the API-key-only mode of `auth.json`.
    #[test]
    fn load_auth_json_without_tokens_errors_with_codex_login_hint() {
        let tmp = std::env::temp_dir().join(format!(
            "agentd-codex-oauth-noauth-{}.json",
            std::process::id()
        ));
        std::fs::write(&tmp, r#"{ "OPENAI_API_KEY": "sk-platform-only" }"#)
            .unwrap();
        let err = load_auth_json(&tmp).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("codex login"), "{msg}");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Empty access/refresh tokens are rejected loudly — having a
    /// `tokens` block with blanks would otherwise silently flow into
    /// a refresh attempt that fails with a confusing 4xx.
    #[test]
    fn load_auth_json_with_blank_tokens_errors() {
        let tmp = std::env::temp_dir().join(format!(
            "agentd-codex-oauth-blank-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &tmp,
            r#"{ "tokens": { "id_token": "x", "access_token": "", "refresh_token": "" } }"#,
        )
        .unwrap();
        let err = load_auth_json(&tmp).unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("codex login"), "{msg}");
        let _ = std::fs::remove_file(&tmp);
    }

    /// Atomic write succeeds and the resulting file parses back to
    /// the same fields. Doesn't (yet) crash-test the rename window —
    /// that's an integration concern with a fault-injecting FS.
    #[test]
    fn save_auth_json_atomic_round_trips() {
        let dir = std::env::temp_dir().join(format!(
            "agentd-codex-oauth-save-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("auth.json");

        let auth = AuthDotJson {
            openai_api_key: Some("sk-keep".into()),
            tokens: Some(Tokens {
                id_token: "ID".into(),
                access_token: "A1".into(),
                refresh_token: "R1".into(),
                account_id: "acct".into(),
            }),
            last_refresh: Some("2026-05-18T01:02:03Z".into()),
        };
        save_auth_json_atomic(&target, &auth).expect("save");

        // The temp file we used during the dance must be gone.
        assert!(!target.with_extension("json.tmp").exists());

        let loaded = load_auth_json(&target).expect("load back");
        let t = loaded.tokens.unwrap();
        assert_eq!(t.access_token, "A1");
        assert_eq!(t.refresh_token, "R1");
        assert_eq!(
            loaded.last_refresh.as_deref(),
            Some("2026-05-18T01:02:03Z")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// `needs_refresh` semantics: no `last_refresh` → refresh now;
    /// recent `last_refresh` → don't; >24h ago → refresh.
    #[test]
    fn needs_refresh_uses_last_refresh_age() {
        let mut auth = AuthDotJson::default();
        // No timestamp → refresh.
        assert!(CodexOauth::needs_refresh(&auth));
        // Recent timestamp → no refresh.
        auth.last_refresh = Some(chrono::Utc::now().to_rfc3339());
        assert!(!CodexOauth::needs_refresh(&auth));
        // Old timestamp → refresh.
        auth.last_refresh =
            Some((chrono::Utc::now() - chrono::Duration::days(2)).to_rfc3339());
        assert!(CodexOauth::needs_refresh(&auth));
        // Malformed timestamp → conservatively refresh.
        auth.last_refresh = Some("not-a-date".to_string());
        assert!(CodexOauth::needs_refresh(&auth));
    }
}
