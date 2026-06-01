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
use eventsource_stream::Eventsource;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use super::{
    Content, LlmProvider, Message, ProviderTurn, ReasoningItem, Role, StopReason, TextSink,
    ToolCall, ToolSpec, Usage,
};

/// Base URL for the OAuth-backed Codex backend. Not the public
/// platform `api.openai.com`.
const CODEX_RESPONSES_URL: &str = "https://chatgpt.com/backend-api/codex/responses";

/// Originator identifier that the codex CLI sends; Cloudflare uses
/// this + the User-Agent + cookies to classify CLI traffic. Don't
/// rebrand it — anonymous third-party identifiers get challenged.
const CODEX_ORIGINATOR: &str = "codex_cli_rs";

/// Required `OpenAI-Beta` header value (from openai/codex source).
const CODEX_OPENAI_BETA: &str = "responses=experimental";

/// Optional env var that overrides the `instructions` field. When
/// set, takes precedence over the agent's `system` prompt — useful
/// when an operator wants to mirror Codex CLI exactly by pasting the
/// upstream `gpt_5_codex_prompt.md`. When unset, we fall back to the
/// `system` arg the agent loop already builds (same shape every
/// other provider uses). The Codex backend rejects empty
/// `instructions`, so the final value must be non-empty either way.
const INSTRUCTIONS_ENV: &str = "AGENTD_ZARVIS_CODEX_INSTRUCTIONS";

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

/// Resolve the `instructions` field for the Codex backend. Order:
///
///   1. `AGENTD_ZARVIS_CODEX_INSTRUCTIONS` env var, if set and
///      non-empty — explicit operator override (e.g. to mirror Codex
///      CLI exactly with the upstream `gpt_5_codex_prompt.md`).
///   2. The `system` argument the agent loop passes — same prompt
///      every other provider (openai / anthropic / ollama) uses for
///      the equivalent slot. This is the default, and the same
///      out-of-box experience the other providers give.
///
/// Errors only if BOTH are empty — the Codex backend rejects an
/// empty `instructions` field with `400`, so failing fast here gives
/// a more actionable message than letting the request go out and
/// get refused.
fn resolve_instructions(system: &str) -> Result<String> {
    if let Ok(raw) = std::env::var(INSTRUCTIONS_ENV) {
        if !raw.trim().is_empty() {
            return Ok(raw);
        }
    }
    if !system.trim().is_empty() {
        return Ok(system.to_string());
    }
    Err(anyhow!(
        "codex-oauth: no `instructions` available. Either set \
         {INSTRUCTIONS_ENV} to a non-empty value, or run zarvis with \
         a non-empty `system` prompt (the agent loop populates this \
         from its system-prompt builder). Empty / unset → Codex \
         backend will reject the request with a 400."
    ))
}

/// Convert one of our `Message`s into Responses-API "input items".
/// One Message can produce multiple items: the AssistantToolCalls
/// variant fans out into one `message` item (for the prose, if any)
/// plus one `function_call` item per tool call. The ToolResult
/// variant produces a `function_call_output` item.
fn message_to_input_items(m: &Message) -> Vec<Value> {
    match &m.content {
        Content::Text { text } => {
            let role = match m.role {
                Role::System => "system",
                Role::User => "user",
                Role::Assistant => "assistant",
                // Tool-role plain text shouldn't really happen (tool
                // results use the dedicated variant), but be
                // defensive — surface it as a `user` message rather
                // than dropping it.
                Role::Tool => "user",
            };
            let typ = if matches!(m.role, Role::Assistant) {
                "output_text"
            } else {
                "input_text"
            };
            vec![json!({
                "type": "message",
                "role": role,
                "content": [{ "type": typ, "text": text }],
            })]
        }
        Content::AssistantToolCalls { text, calls } => {
            let mut out = Vec::with_capacity(calls.len() + 1);
            if let Some(t) = text.as_deref().filter(|t| !t.is_empty()) {
                out.push(json!({
                    "type": "message",
                    "role": "assistant",
                    "content": [{ "type": "output_text", "text": t }],
                }));
            }
            for c in calls {
                out.push(json!({
                    "type": "function_call",
                    "call_id": c.id,
                    "name": c.name,
                    "arguments": serde_json::to_string(&c.input)
                        .unwrap_or_else(|_| "{}".into()),
                }));
            }
            out
        }
        Content::ToolResult {
            call_id,
            output,
            is_error: _,
        } => {
            // Responses API doesn't have a dedicated error flag on
            // function_call_output — error text just rides along in
            // `output`. The model picks up on it from content.
            vec![json!({
                "type": "function_call_output",
                "call_id": call_id,
                "output": output,
            })]
        }
        Content::Summary { text, .. } => {
            // `/compact` artifact — serialize as a `user` message with
            // the standard wire prefix so the model knows this stands
            // in for earlier turns. Same treatment as the other
            // providers.
            let body = format!("{}{}", super::SUMMARY_WIRE_PREFIX, text);
            vec![json!({
                "type": "message",
                "role": "user",
                "content": [{ "type": "input_text", "text": body }],
            })]
        }
        Content::Reasoning(item) => {
            // Echo the reasoning item back verbatim (id + encrypted_content
            // + summary) so the backend caches the prefix and the model
            // keeps its prior reasoning. Dropping it busts the cache.
            let summary: Vec<Value> = item
                .summary
                .iter()
                .map(|t| json!({ "type": "summary_text", "text": t }))
                .collect();
            let mut obj = serde_json::Map::new();
            obj.insert("type".into(), json!("reasoning"));
            obj.insert("id".into(), json!(item.id));
            obj.insert("summary".into(), Value::Array(summary));
            if let Some(enc) = &item.encrypted_content {
                obj.insert("encrypted_content".into(), json!(enc));
            }
            vec![Value::Object(obj)]
        }
    }
}

/// Convert a `ToolSpec` to Responses-API tool definition shape.
/// Responses uses a flatter shape than chat-completions: no
/// `{ "type": "function", "function": {...} }` wrapping.
fn tool_spec_to_value(t: &ToolSpec) -> Value {
    json!({
        "type": "function",
        "name": t.name,
        "description": t.description,
        "parameters": t.schema.clone(),
    })
}

/// Build the request body for `POST /backend-api/codex/responses`.
/// Public for unit testing — keeps the shape pinned independently
/// of the live transport.
pub fn build_responses_body(
    model: &str,
    instructions: &str,
    messages: &[Message],
    tools: &[ToolSpec],
) -> Value {
    let input: Vec<Value> = messages.iter().flat_map(message_to_input_items).collect();
    // Asks the server to emit reasoning summary deltas (surfaced via
    // `sink.reasoning_delta`). Optionally pin an explicit reasoning effort
    // (low|medium|high) via `AGENTD_ZARVIS_REASONING_EFFORT`, mirroring Codex
    // CLI's `model_reasoning_effort`; unset = the backend default.
    let mut reasoning = json!({ "summary": "auto" });
    if let Ok(effort) = std::env::var("AGENTD_ZARVIS_REASONING_EFFORT") {
        let effort = effort.trim();
        if !effort.is_empty() {
            reasoning["effort"] = json!(effort);
        }
    }
    let mut body = json!({
        "model": model,
        "instructions": instructions,
        "input": input,
        "stream": true,
        // ChatGPT-account backend REQUIRES `store: false`. The
        // public Responses API defaults to true (server-side
        // conversation storage), but `chatgpt.com/backend-api/codex`
        // refuses requests where store != false with
        // `400 "Store must be set to false"`. Codex CLI sets this
        // explicitly in `codex-rs/core/src/client.rs`.
        "store": false,
        "reasoning": reasoning,
        // Ask the backend to return reasoning items WITH their encrypted
        // content so we can echo them back next turn (prompt caching +
        // reasoning continuity), matching the Codex CLI.
        "include": ["reasoning.encrypted_content"],
    });
    if !tools.is_empty() {
        body["tools"] = Value::Array(tools.iter().map(tool_spec_to_value).collect());
        body["parallel_tool_calls"] = Value::Bool(true);
    }
    // Stable per-session prompt-cache key (mirrors the Codex CLI). Each turn
    // re-sends a growing, mostly-unchanged prefix; pinning a key routes those
    // requests to the same prompt-cache node so the prefix actually hits.
    // Without it, automatic prefix caching still works but routing is unstable
    // under load — measured as a low/erratic hit-rate (~31% vs Codex's ~97%).
    if let Ok(key) = std::env::var("AGENTD_SESSION_ID") {
        if !key.is_empty() {
            body["prompt_cache_key"] = json!(key);
        }
    }
    body
}

/// Tool-call accumulator for the SSE stream. The Responses API
/// streams function_call items as item-added + arguments-delta
/// events; we collect them keyed by `item_id` here and flush when
/// `output_item.done` fires for that id.
#[derive(Default, Debug)]
struct FnCallAcc {
    call_id: String,
    name: String,
    args: String,
}

#[async_trait]
impl LlmProvider for CodexOauth {
    fn name(&self) -> &str {
        "codex-oauth"
    }

    async fn complete(
        &self,
        model: &str,
        system: &str,
        messages: &[Message],
        tools: &[ToolSpec],
        sink: &mut dyn TextSink,
    ) -> Result<ProviderTurn> {
        // Codex's Responses API uses `instructions` instead of an
        // inline system message. We pass the agent's `system` arg
        // through as-is, matching what every other provider does
        // for the equivalent slot. `AGENTD_ZARVIS_CODEX_INSTRUCTIONS`
        // is an optional operator override (handy for mirroring
        // Codex CLI exactly with the upstream prompt).
        let instructions = resolve_instructions(system)?;
        let body = build_responses_body(model, &instructions, messages, tools);

        // Take the auth lock for the duration of the request so
        // concurrent zarvis turns don't race on token rotation.
        let mut state = self.state.lock().await;
        if Self::needs_refresh(&state.auth) {
            self.refresh_locked(&mut state).await?;
        }
        let (access_token, account_id) = {
            let tokens = state
                .auth
                .tokens
                .as_ref()
                .ok_or_else(|| anyhow!("auth.json tokens missing after refresh"))?;
            (tokens.access_token.clone(), tokens.account_id.clone())
        };
        // Release the lock for the duration of the HTTP call — we
        // don't need exclusive access to the auth state while the
        // request streams, and holding the lock would serialize all
        // codex-oauth turns within a process.
        drop(state);

        let mut req = self
            .http
            .post(CODEX_RESPONSES_URL)
            .bearer_auth(&access_token)
            .header("OpenAI-Beta", CODEX_OPENAI_BETA)
            .header("originator", CODEX_ORIGINATOR)
            .header("Content-Type", "application/json")
            .header("Accept", "text/event-stream")
            .json(&body);
        if !account_id.is_empty() {
            req = req.header("ChatGPT-Account-ID", account_id);
        }
        let resp = req
            .send()
            .await
            .context("POST chatgpt.com/backend-api/codex/responses")?;
        let status = resp.status();
        if !status.is_success() {
            let raw = resp.text().await.unwrap_or_default();
            if status.as_u16() == 400 {
                if let Some(extracted) = super::parse_overflow(&raw) {
                    return Err(anyhow::Error::new(super::ContextOverflow {
                        extracted,
                        raw,
                    }));
                }
            }
            // Auth failures on this endpoint usually mean the access
            // token expired between our refresh check and now (or
            // the server invalidated it). Surface that distinctly so
            // the agent loop can stop retrying — re-running `codex
            // login` is the only fix.
            if status.as_u16() == 401 {
                return Err(anyhow!(
                    "codex-oauth: 401 from chatgpt.com (access token \
                     rejected). Try re-running `codex login`. Body: {raw}"
                ));
            }
            return Err(anyhow!("codex-oauth: {status} from chatgpt.com: {raw}"));
        }

        let mut stream = resp.bytes_stream().eventsource();
        let mut assistant_text = String::new();
        let mut fn_calls: std::collections::HashMap<String, FnCallAcc> =
            std::collections::HashMap::new();
        let mut fn_call_order: Vec<String> = Vec::new();
        let mut stop_reason = StopReason::EndTurn;
        let mut usage = Usage::default();
        let mut terminal_event_seen = false;
        let mut reasoning_items: Vec<ReasoningItem> = Vec::new();

        while let Some(ev) = stream.next().await {
            let ev = ev.context("codex-oauth SSE stream")?;
            // Liveness: any received SSE event (even text-less) keeps the
            // idle watchdog from firing mid-stream.
            sink.progress();
            // The Codex Responses transport uses `event:` SSE names;
            // `data:` carries the JSON body. eventsource_stream
            // surfaces both as fields on the event.
            let kind = ev.event.as_str();
            let chunk: Value = match serde_json::from_str(&ev.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            match kind {
                // Streaming assistant text.
                "response.output_text.delta" => {
                    if let Some(text) = chunk.get("delta").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            sink.delta(text);
                            assistant_text.push_str(text);
                        }
                    }
                }
                // Streaming reasoning-summary text. We requested
                // `"reasoning": { "summary": "auto" }` in the body, so
                // the server emits these whenever the model produced
                // a reasoning trace. Surface them via the sink's
                // reasoning channel so the TUI can render them dim
                // italic and the headless transcript records them as
                // distinct `SessionEvent::Reasoning` events.
                "response.reasoning_summary_text.delta" => {
                    if let Some(text) = chunk.get("delta").and_then(|v| v.as_str()) {
                        if !text.is_empty() {
                            sink.reasoning_delta(text);
                        }
                    }
                }
                // A new item appeared. We care about function_call
                // items — record the id/name so subsequent
                // arguments-delta events can find the accumulator.
                "response.output_item.added" => {
                    let Some(item) = chunk.get("item") else { continue };
                    if item.get("type").and_then(|v| v.as_str()) != Some("function_call") {
                        continue;
                    }
                    let item_id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let call_id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default()
                        .to_string();
                    if !item_id.is_empty() {
                        fn_call_order.push(item_id.clone());
                        fn_calls.insert(
                            item_id,
                            FnCallAcc {
                                call_id,
                                name,
                                args: String::new(),
                            },
                        );
                    }
                }
                // Completed output item. We capture `reasoning` items: they
                // carry `encrypted_content` (because we requested
                // include: reasoning.encrypted_content) which must be echoed
                // back next turn for prompt caching + reasoning continuity.
                "response.output_item.done" => {
                    if let Some(item) = chunk.get("item") {
                        if item.get("type").and_then(|v| v.as_str()) == Some("reasoning") {
                            let id = item
                                .get("id")
                                .and_then(|v| v.as_str())
                                .unwrap_or_default()
                                .to_string();
                            if !id.is_empty() {
                                let encrypted_content = item
                                    .get("encrypted_content")
                                    .and_then(|v| v.as_str())
                                    .map(|s| s.to_string());
                                let summary = item
                                    .get("summary")
                                    .and_then(|v| v.as_array())
                                    .map(|a| {
                                        a.iter()
                                            .filter_map(|s| {
                                                s.get("text").and_then(|t| t.as_str())
                                            })
                                            .map(|t| t.to_string())
                                            .collect()
                                    })
                                    .unwrap_or_default();
                                reasoning_items.push(ReasoningItem {
                                    id,
                                    encrypted_content,
                                    summary,
                                });
                            }
                        }
                    }
                }
                // Tool-call arguments stream a piece at a time.
                "response.function_call_arguments.delta" => {
                    let item_id = chunk
                        .get("item_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if let Some(acc) = fn_calls.get_mut(item_id) {
                        if let Some(delta) =
                            chunk.get("delta").and_then(|v| v.as_str())
                        {
                            acc.args.push_str(delta);
                        }
                    }
                }
                // Final arguments value — some servers send this in
                // addition to the deltas. Be defensive: overwrite if
                // we got it, since `delta`s + final is a known
                // shape.
                "response.function_call_arguments.done" => {
                    let item_id = chunk
                        .get("item_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or_default();
                    if let Some(acc) = fn_calls.get_mut(item_id) {
                        if let Some(args) =
                            chunk.get("arguments").and_then(|v| v.as_str())
                        {
                            if !args.is_empty() {
                                acc.args = args.to_string();
                            }
                        }
                    }
                }
                // End of stream. Pull usage + stop reason if the
                // server reported them.
                "response.completed" | "response.incomplete" | "response.failed" => {
                    terminal_event_seen = true;
                    if let Some(u) = chunk.pointer("/response/usage") {
                        usage.input_tokens = u
                            .get("input_tokens")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(usage.input_tokens);
                        usage.output_tokens = u
                            .get("output_tokens")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(usage.output_tokens);
                        usage.cached_tokens = u
                            .pointer("/input_tokens_details/cached_tokens")
                            .and_then(|n| n.as_u64())
                            .unwrap_or(usage.cached_tokens);
                    }
                    if let Some(reason) = chunk
                        .pointer("/response/incomplete_details/reason")
                        .and_then(|v| v.as_str())
                    {
                        if reason == "max_output_tokens" {
                            stop_reason = StopReason::MaxTokens;
                        }
                    }
                    if kind == "response.failed" {
                        return Err(failed_response_error(&chunk));
                    }
                    break;
                }
                _ => {
                    // Other events (reasoning summaries, status,
                    // etc.) are not surfaced to the agent in v1.
                }
            }
        }
        if !terminal_event_seen {
            return Err(anyhow!(
                "codex-oauth stream ended before response.completed"
            ));
        }

        // If the model emitted tool calls, that's the turn's stop
        // reason regardless of what `response.completed` said.
        let calls: Vec<ToolCall> = fn_call_order
            .iter()
            .filter_map(|id| fn_calls.remove(id))
            .filter(|a| !a.name.is_empty())
            .map(|a| {
                let input = if a.args.is_empty() {
                    json!({})
                } else {
                    serde_json::from_str::<Value>(&a.args).unwrap_or_else(|_| json!({}))
                };
                ToolCall {
                    id: if a.call_id.is_empty() {
                        format!("tool_{}", short_hash(&a.name))
                    } else {
                        a.call_id
                    },
                    name: a.name,
                    input,
                }
            })
            .collect();
        if !calls.is_empty() {
            stop_reason = StopReason::ToolUse;
        }

        Ok(ProviderTurn {
            text: if assistant_text.is_empty() {
                None
            } else {
                Some(assistant_text)
            },
            tool_calls: calls,
            stop_reason,
            usage,
            reasoning_items,
        })
    }
}

fn response_error_message(chunk: &Value) -> String {
    chunk
        .pointer("/response/error/message")
        .and_then(|v| v.as_str())
        .or_else(|| chunk.pointer("/error/message").and_then(|v| v.as_str()))
        .or_else(|| {
            chunk
                .pointer("/response/incomplete_details/reason")
                .and_then(|v| v.as_str())
        })
        .unwrap_or("unknown error")
        .to_string()
}

/// Build the error for a `response.failed` SSE event. A context-window
/// overflow can arrive this way (HTTP 200 + a streamed failure) rather
/// than as an HTTP 400, so check the message for an overflow signature
/// and surface the typed [`super::ContextOverflow`] when it matches —
/// that's what routes the agent loop to relearn the cap, prune/compact,
/// and retry, exactly like the HTTP-400 path. Anything else stays a plain
/// provider error that ends the turn.
fn failed_response_error(chunk: &Value) -> anyhow::Error {
    let msg = response_error_message(chunk);
    if let Some(extracted) = super::parse_overflow(&msg) {
        return anyhow::Error::new(super::ContextOverflow { extracted, raw: msg });
    }
    anyhow!("codex-oauth response failed: {msg}")
}

fn short_hash(s: &str) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    s.hash(&mut h);
    format!("{:x}", h.finish())[..8].to_string()
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

    fn user(text: &str) -> Message {
        Message {
            role: Role::User,
            content: Content::Text {
                text: text.into(),
            },
        }
    }

    fn assistant(text: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: Content::Text {
                text: text.into(),
            },
        }
    }

    /// Smoke test: plain user→assistant exchange maps to the
    /// Responses-API `input` shape with `message` items, correct
    /// roles, and `input_text` / `output_text` content kinds.
    #[test]
    fn build_body_emits_responses_message_shape() {
        let body = build_responses_body(
            "gpt-5-codex",
            "system-prompt-here",
            &[user("hi"), assistant("hello back")],
            &[],
        );
        assert_eq!(body["model"], "gpt-5-codex");
        assert_eq!(body["instructions"], "system-prompt-here");
        assert_eq!(body["stream"], true);
        // ChatGPT-account backend rejects requests with `store: true`
        // (the public-API default). Locking this in here so the
        // field never silently gets dropped — re-adding it would
        // produce a 400 "Store must be set to false" that's
        // confusing without context.
        assert_eq!(body["store"], false);
        let input = body["input"].as_array().expect("input array");
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
        assert_eq!(input[0]["content"][0]["text"], "hi");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["content"][0]["text"], "hello back");
        // No tools were passed; the field should be absent rather
        // than emitting `tools: []` (server-side has historically
        // disliked empty arrays here).
        assert!(body.get("tools").is_none());
        assert!(body.get("parallel_tool_calls").is_none());
    }

    /// Tools are emitted in the flatter Responses shape (no
    /// `function` wrapper) and `parallel_tool_calls: true` is set.
    #[test]
    fn build_body_emits_responses_tool_shape() {
        let tools = vec![ToolSpec {
            name: "shell".into(),
            description: "run a shell command".into(),
            schema: json!({
                "type": "object",
                "properties": { "cmd": { "type": "string" } },
                "required": ["cmd"],
            }),
        }];
        let body = build_responses_body("gpt-5-codex", "sys", &[user("ls")], &tools);
        let tools_v = body["tools"].as_array().expect("tools present");
        assert_eq!(tools_v.len(), 1);
        assert_eq!(tools_v[0]["type"], "function");
        assert_eq!(tools_v[0]["name"], "shell");
        assert_eq!(tools_v[0]["description"], "run a shell command");
        assert!(tools_v[0]["parameters"]["properties"]["cmd"].is_object());
        assert_eq!(body["parallel_tool_calls"], true);
    }

    /// An AssistantToolCalls message fans out into one `message`
    /// item (for the pre-tool prose, if any) plus one `function_call`
    /// item per call. Tool results map to `function_call_output`.
    /// This is the long pole on round-trip correctness: getting it
    /// wrong means the model can't see its own prior tool calls and
    /// goes into a re-call loop.
    #[test]
    fn build_body_round_trips_tool_call_history() {
        let msgs = vec![
            user("list files"),
            Message {
                role: Role::Assistant,
                content: Content::AssistantToolCalls {
                    text: Some("calling shell".into()),
                    calls: vec![ToolCall {
                        id: "call_abc".into(),
                        name: "shell".into(),
                        input: json!({"cmd": "ls"}),
                    }],
                },
            },
            Message {
                role: Role::Tool,
                content: Content::ToolResult {
                    call_id: "call_abc".into(),
                    output: "Cargo.toml\nsrc".into(),
                    is_error: false,
                },
            },
        ];
        let body = build_responses_body("gpt-5-codex", "sys", &msgs, &[]);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4, "user + assistant-prose + fn_call + fn_output");
        assert_eq!(input[0]["role"], "user");
        // Assistant prose item.
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "calling shell");
        // function_call item.
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "call_abc");
        assert_eq!(input[2]["name"], "shell");
        // Arguments must be a JSON-encoded STRING, not an object —
        // that's the Responses API contract.
        let args_str = input[2]["arguments"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(args_str).unwrap();
        assert_eq!(parsed["cmd"], "ls");
        // function_call_output item.
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "call_abc");
        assert_eq!(input[3]["output"], "Cargo.toml\nsrc");
    }

    /// An AssistantToolCalls with empty `text` does NOT emit an
    /// empty-text message — the Responses API treats `output_text:
    /// ""` as a malformed item.
    #[test]
    fn build_body_skips_empty_assistant_prose_before_tool_call() {
        let msgs = vec![Message {
            role: Role::Assistant,
            content: Content::AssistantToolCalls {
                text: None,
                calls: vec![ToolCall {
                    id: "x".into(),
                    name: "shell".into(),
                    input: json!({}),
                }],
            },
        }];
        let body = build_responses_body("gpt-5-codex", "sys", &msgs, &[]);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "function_call");
    }

    /// `instructions` resolution: env var wins when set,
    /// `system` arg is the fallback, both empty errors out. Uses a
    /// scoped env-guard to keep the global env clean for other
    /// tests running in parallel.
    #[test]
    fn resolve_instructions_prefers_env_then_system() {
        // Save/restore the env var around the test body so parallel
        // tests don't see our state.
        struct EnvGuard(Option<String>);
        impl Drop for EnvGuard {
            fn drop(&mut self) {
                match &self.0 {
                    Some(v) => std::env::set_var(INSTRUCTIONS_ENV, v),
                    None => std::env::remove_var(INSTRUCTIONS_ENV),
                }
            }
        }
        let _g = EnvGuard(std::env::var(INSTRUCTIONS_ENV).ok());

        // env wins when set & non-empty.
        std::env::set_var(INSTRUCTIONS_ENV, "from-env");
        assert_eq!(resolve_instructions("from-system").unwrap(), "from-env");

        // env blank → falls back to system.
        std::env::set_var(INSTRUCTIONS_ENV, "   ");
        assert_eq!(resolve_instructions("from-system").unwrap(), "from-system");

        // env unset → system is used.
        std::env::remove_var(INSTRUCTIONS_ENV);
        assert_eq!(resolve_instructions("from-system").unwrap(), "from-system");

        // Both empty → actionable error mentioning the env var.
        let err = resolve_instructions("").unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains(INSTRUCTIONS_ENV), "{msg}");
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

    #[test]
    fn response_error_message_extracts_failed_response_reason() {
        let chunk = json!({
            "response": {
                "error": {
                    "message": "context too large"
                }
            }
        });
        assert_eq!(response_error_message(&chunk), "context too large");

        let chunk = json!({
            "response": {
                "incomplete_details": {
                    "reason": "max_output_tokens"
                }
            }
        });
        assert_eq!(response_error_message(&chunk), "max_output_tokens");
    }

    #[test]
    fn failed_response_overflow_surfaces_context_overflow() {
        // The exact wording the codex backend streams on a context-window
        // overflow (HTTP 200 + `response.failed`, not an HTTP 400). It must
        // downcast to `ContextOverflow` so the agent loop relearns the cap,
        // prunes/compacts, and retries — instead of just ending the turn.
        let chunk = json!({
            "response": {
                "error": {
                    "message": "Your input exceeds the context window of this \
                                model. Please adjust your input and try again."
                }
            }
        });
        let err = failed_response_error(&chunk);
        assert!(
            err.downcast_ref::<crate::provider::ContextOverflow>().is_some(),
            "context-window overflow via response.failed must be a ContextOverflow; got: {err:#}"
        );
    }

    #[test]
    fn failed_response_non_overflow_stays_plain_error() {
        let chunk = json!({
            "response": { "error": { "message": "internal server error" } }
        });
        let err = failed_response_error(&chunk);
        assert!(
            err.downcast_ref::<crate::provider::ContextOverflow>().is_none(),
            "a non-overflow failure must not be misclassified as ContextOverflow"
        );
        assert_eq!(
            format!("{err}"),
            "codex-oauth response failed: internal server error"
        );
    }
}
