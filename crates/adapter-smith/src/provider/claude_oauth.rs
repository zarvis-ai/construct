//! Claude Code OAuth provider — direct Anthropic Messages API.
//!
//! Reads the Claude Code subscription OAuth credentials (macOS keychain item
//! `Claude Code-credentials`, or `~/.claude/.credentials.json`) and calls
//! `https://api.anthropic.com/v1/messages` directly with a `Bearer` access
//! token, the `anthropic-beta: oauth-2025-04-20` header, and a system prompt
//! that leads with the Claude Code identity block (the OAuth credential is
//! scoped to the Claude Code client). Smith's tools are passed as native
//! `tools`, so the model emits real `tool_use` blocks and Smith executes them
//! in its own agent loop — exactly like the API-key `anthropic` provider,
//! which this shares its wire with (see [`super::anthropic`]).
//!
//! This replaces the earlier transport that delegated to the `claude` CLI with
//! `--tools "" --json-schema`; that shim made the model flail in the CLI's
//! internal agent loop and frequently bail without calling any tool.
//!
//! Compliance note: routing the subscription OAuth token straight at the API
//! (rather than via `claude -p` / the Agent SDK) is the user's own
//! subscription on their own machine, but it is NOT the surface Anthropic
//! documents for subscription use. The token endpoint, client id, beta header,
//! and identity-prompt requirement below are reverse-engineered from the
//! Claude Code client and may change without notice.

use super::{LlmProvider, Message, ProviderTurn, TextSink, ToolSpec};
use anyhow::{anyhow, bail, Context, Result};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::process::Command;
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::sync::Mutex;

const MESSAGES_URL: &str = "https://api.anthropic.com/v1/messages";
/// OAuth token mint/refresh endpoint the Claude Code client uses.
const TOKEN_URL: &str = "https://console.anthropic.com/v1/oauth/token";
/// Public Claude Code OAuth client id (reverse-engineered; not a secret).
const OAUTH_CLIENT_ID: &str = "9d1c250a-e61b-44d9-88ed-5944d1962f5e";
/// Beta header that authorizes subscription OAuth tokens on `/v1/messages`.
const OAUTH_BETA: &str = "oauth-2025-04-20";
/// The OAuth credential is scoped to Claude Code; the system prompt must lead
/// with this identity block or the API rejects the request.
const CLAUDE_CODE_IDENTITY: &str = "You are Claude Code, Anthropic's official CLI for Claude.";
/// macOS keychain generic-password service name Claude Code writes its creds to.
const KEYCHAIN_SERVICE: &str = "Claude Code-credentials";
/// Refresh when the access token is within this much of expiry.
const REFRESH_LEEWAY_MS: u64 = 5 * 60 * 1000;

pub struct ClaudeOauth {
    http: reqwest::Client,
    state: Arc<Mutex<AuthState>>,
}

struct AuthState {
    store: CredStore,
    creds: Creds,
}

/// Where the credentials live, so a token refresh can write the rotated
/// tokens back to the same place the official client reads them from.
enum CredStore {
    /// macOS keychain generic password (`security`), with the item's account.
    Keychain { account: String },
    /// A `*.credentials.json` file (Linux, or an explicit override).
    File(PathBuf),
}

struct Creds {
    access_token: String,
    refresh_token: String,
    /// Unix-epoch milliseconds. 0 when unknown (forces a refresh).
    expires_at_ms: u64,
    /// The full credential JSON document, preserved so refresh writes back
    /// every field the official client wrote — only the tokens change.
    doc: Value,
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// Refresh if the expiry is unknown, already past, or inside the leeway window.
fn needs_refresh(expires_at_ms: u64, now: u64) -> bool {
    expires_at_ms == 0 || now + REFRESH_LEEWAY_MS >= expires_at_ms
}

/// Map the short aliases the `/model claude-oauth:<x>` completer offers onto
/// concrete model ids the API accepts; pass through anything already concrete.
fn resolve_model(model: &str) -> String {
    match model.trim() {
        "opus" => "claude-opus-4-8".to_string(),
        "sonnet" => "claude-sonnet-4-6".to_string(),
        "haiku" => "claude-haiku-4-5".to_string(),
        other => other.to_string(),
    }
}

/// Build the `system` field: the required Claude Code identity block first,
/// then Smith's own system prompt as a second block (if any).
fn system_blocks(system: &str) -> Value {
    let mut blocks = vec![json!({ "type": "text", "text": CLAUDE_CODE_IDENTITY })];
    if !system.trim().is_empty() {
        blocks.push(json!({ "type": "text", "text": system }));
    }
    Value::Array(blocks)
}

fn parse_creds(raw: &str) -> Result<Creds> {
    let doc: Value =
        serde_json::from_str(raw.trim()).context("parse Claude Code credentials JSON")?;
    let o = doc.get("claudeAiOauth").unwrap_or(&doc);
    let access_token = o
        .get("accessToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let refresh_token = o
        .get("refreshToken")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let expires_at_ms = o.get("expiresAt").and_then(|v| v.as_u64()).unwrap_or(0);
    if access_token.is_empty() {
        bail!(
            "Claude Code credentials have no accessToken; run `claude` and log in with your \
             Claude subscription before using the claude-oauth provider"
        );
    }
    Ok(Creds {
        access_token,
        refresh_token,
        expires_at_ms,
        doc,
    })
}

fn keychain_read() -> Result<String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE, "-w"])
        .output()
        .context("run `security find-generic-password`")?;
    if !out.status.success() {
        bail!("keychain item `{KEYCHAIN_SERVICE}` not found");
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Read the keychain item's account so a refresh can update the same item.
fn keychain_account() -> Option<String> {
    let out = Command::new("security")
        .args(["find-generic-password", "-s", KEYCHAIN_SERVICE])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&out.stdout);
    for line in text.lines() {
        // e.g. `    "acct"<blob>="user@example.com"`
        if let Some(rest) = line.trim().strip_prefix("\"acct\"<blob>=") {
            let v = rest.trim().trim_matches('"');
            if !v.is_empty() && v != "<NULL>" {
                return Some(v.to_string());
            }
        }
    }
    None
}

impl CredStore {
    fn locate() -> Result<Self> {
        if let Ok(p) = std::env::var("CONSTRUCT_CLAUDE_OAUTH_CREDENTIALS") {
            if !p.is_empty() {
                return Ok(CredStore::File(PathBuf::from(p)));
            }
        }
        if let Ok(home) = std::env::var("HOME") {
            let p = PathBuf::from(&home).join(".claude").join(".credentials.json");
            if p.exists() {
                return Ok(CredStore::File(p));
            }
        }
        if let Some(account) = keychain_account() {
            return Ok(CredStore::Keychain { account });
        }
        // Account unknown but the item may still be readable by service name.
        if keychain_read().is_ok() {
            return Ok(CredStore::Keychain {
                account: std::env::var("USER").unwrap_or_default(),
            });
        }
        Err(anyhow!(
            "could not find Claude Code OAuth credentials (checked \
             $CONSTRUCT_CLAUDE_OAUTH_CREDENTIALS, ~/.claude/.credentials.json, and the macOS \
             keychain service `{KEYCHAIN_SERVICE}`). Run `claude` and log in with your Claude \
             subscription first."
        ))
    }

    fn load(&self) -> Result<Creds> {
        let raw = match self {
            CredStore::File(p) => {
                std::fs::read_to_string(p).with_context(|| format!("read {}", p.display()))?
            }
            CredStore::Keychain { .. } => keychain_read()?,
        };
        parse_creds(&raw)
    }

    fn save(&self, doc: &Value) -> Result<()> {
        let json = serde_json::to_string(doc).context("serialize credentials")?;
        match self {
            CredStore::File(p) => {
                let tmp = p.with_extension("json.tmp");
                std::fs::write(&tmp, json.as_bytes())
                    .with_context(|| format!("write {}", tmp.display()))?;
                std::fs::rename(&tmp, p)
                    .with_context(|| format!("rename {} -> {}", tmp.display(), p.display()))?;
            }
            CredStore::Keychain { account } => {
                // `-U` updates the existing item in place.
                let status = Command::new("security")
                    .args([
                        "add-generic-password",
                        "-U",
                        "-a",
                        account,
                        "-s",
                        KEYCHAIN_SERVICE,
                        "-w",
                        &json,
                    ])
                    .status()
                    .context("run `security add-generic-password`")?;
                if !status.success() {
                    bail!("failed to update keychain item `{KEYCHAIN_SERVICE}` ({status})");
                }
            }
        }
        Ok(())
    }
}

impl ClaudeOauth {
    pub fn from_env() -> Result<Self> {
        let store = CredStore::locate()?;
        let creds = store.load()?;
        let http = reqwest::Client::builder()
            .build()
            .context("build reqwest client")?;
        Ok(Self {
            http,
            state: Arc::new(Mutex::new(AuthState { store, creds })),
        })
    }

    /// Refresh the access token when it's near/at expiry and persist the
    /// rotated tokens back to the credential store. The caller holds the auth
    /// lock across the whole refresh so concurrent turns don't double-rotate
    /// (Anthropic refresh tokens are single-use).
    async fn ensure_fresh(&self, state: &mut AuthState) -> Result<()> {
        if !needs_refresh(state.creds.expires_at_ms, now_ms()) {
            return Ok(());
        }
        if state.creds.refresh_token.is_empty() {
            bail!(
                "Claude Code access token expired and no refresh_token is present; run `claude` \
                 to re-authenticate"
            );
        }
        let body = json!({
            "grant_type": "refresh_token",
            "refresh_token": state.creds.refresh_token,
            "client_id": OAUTH_CLIENT_ID,
        });
        let resp = self
            .http
            .post(TOKEN_URL)
            .header("content-type", "application/json")
            .json(&body)
            .send()
            .await
            .context("POST oauth/token")?;
        let status = resp.status();
        let bytes = resp.bytes().await.unwrap_or_default();
        if !status.is_success() {
            let txt = String::from_utf8_lossy(&bytes);
            bail!(
                "Claude OAuth token refresh failed ({status}): {txt}. Run `claude` to \
                 re-authenticate."
            );
        }
        #[derive(Deserialize)]
        struct RefreshResp {
            #[serde(default)]
            access_token: Option<String>,
            #[serde(default)]
            refresh_token: Option<String>,
            #[serde(default)]
            expires_in: Option<u64>,
        }
        let r: RefreshResp =
            serde_json::from_slice(&bytes).context("parse oauth/token response")?;
        let new_access = r
            .access_token
            .filter(|s| !s.is_empty())
            .ok_or_else(|| anyhow!("oauth/token response had no access_token"))?;
        state.creds.access_token = new_access;
        if let Some(rt) = r.refresh_token.filter(|s| !s.is_empty()) {
            state.creds.refresh_token = rt;
        }
        // Default ~8h if the server omits expires_in.
        state.creds.expires_at_ms = now_ms() + r.expires_in.unwrap_or(8 * 60 * 60) * 1000;

        // Write the rotated tokens back into the preserved doc, then persist.
        let access = state.creds.access_token.clone();
        let refresh = state.creds.refresh_token.clone();
        let exp = state.creds.expires_at_ms;
        let target = if state.creds.doc.get("claudeAiOauth").is_some() {
            state.creds.doc.get_mut("claudeAiOauth").unwrap()
        } else {
            &mut state.creds.doc
        };
        target["accessToken"] = json!(access);
        target["refreshToken"] = json!(refresh);
        target["expiresAt"] = json!(exp);
        // Best-effort: we already hold a valid in-memory token, so a persist
        // failure must not fail the live turn (it only risks a re-refresh next
        // process start).
        if let Err(e) = state.store.save(&state.creds.doc) {
            eprintln!("claude-oauth: warning: failed to persist refreshed token: {e}");
        }
        Ok(())
    }
}

#[async_trait]
impl LlmProvider for ClaudeOauth {
    fn name(&self) -> &str {
        "claude-oauth"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        let access_token = {
            let mut state = self.state.lock().await;
            self.ensure_fresh(&mut state).await?;
            state.creds.access_token.clone()
        };

        let mut body = json!({
            "model": resolve_model(model),
            "max_tokens": 8192,
            "stream": true,
            "system": system_blocks(system),
            "messages": super::anthropic::messages_to_anthropic(messages),
        });
        if !tools.is_empty() {
            body["tools"] = Value::Array(super::anthropic::tools_to_anthropic(tools));
        }

        let resp = self
            .http
            .post(MESSAGES_URL)
            .header("authorization", format!("Bearer {access_token}"))
            .header("anthropic-version", "2023-06-01")
            .header("anthropic-beta", OAUTH_BETA)
            .json(&body)
            .send()
            .await
            .context("claude-oauth POST /v1/messages")?;
        super::anthropic::read_message_stream(resp, sink).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_model_maps_aliases_and_passes_through() {
        assert_eq!(resolve_model("opus"), "claude-opus-4-8");
        assert_eq!(resolve_model("sonnet"), "claude-sonnet-4-6");
        assert_eq!(resolve_model("haiku"), "claude-haiku-4-5");
        // Concrete ids pass through untouched.
        assert_eq!(resolve_model("claude-sonnet-4-6"), "claude-sonnet-4-6");
        assert_eq!(resolve_model("claude-opus-4-8"), "claude-opus-4-8");
    }

    #[test]
    fn parse_creds_reads_oauth_wrapper() {
        let raw = r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat01-abc","refreshToken":"sk-ant-ort01-xyz","expiresAt":1781721246728,"scopes":["user:inference"],"subscriptionType":"max"}}"#;
        let c = parse_creds(raw).unwrap();
        assert_eq!(c.access_token, "sk-ant-oat01-abc");
        assert_eq!(c.refresh_token, "sk-ant-ort01-xyz");
        assert_eq!(c.expires_at_ms, 1781721246728);
    }

    #[test]
    fn parse_creds_rejects_missing_token() {
        assert!(parse_creds(r#"{"claudeAiOauth":{"refreshToken":"x"}}"#).is_err());
    }

    #[test]
    fn needs_refresh_window() {
        let now = 1_000_000u64;
        assert!(needs_refresh(0, now)); // unknown expiry
        assert!(needs_refresh(now, now)); // already expired
        assert!(needs_refresh(now + REFRESH_LEEWAY_MS - 1, now)); // inside leeway
        assert!(!needs_refresh(now + REFRESH_LEEWAY_MS + 1, now)); // comfortably valid
    }

    #[test]
    fn system_blocks_lead_with_identity() {
        let v = system_blocks("smith system prompt");
        let arr = v.as_array().unwrap();
        assert_eq!(arr.len(), 2);
        assert_eq!(arr[0]["text"], CLAUDE_CODE_IDENTITY);
        assert_eq!(arr[1]["text"], "smith system prompt");
        // Empty smith system → just the identity block.
        assert_eq!(system_blocks("").as_array().unwrap().len(), 1);
    }
}
