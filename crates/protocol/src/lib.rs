//! agentd wire protocol.
//!
//! Two protocols share JSON-RPC 2.0 over line-delimited JSON:
//!
//! - **AHP** (Agent Harness Protocol): daemon ⇄ adapter, over the adapter's stdio.
//! - **IPC**: client ⇄ daemon, over a Unix socket.
//!
//! Both reuse the envelope types in [`jsonrpc`] and the same [`transport`] helpers.

pub mod adapter;
pub mod jsonrpc;
pub mod paths;
pub mod transport;

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub use jsonrpc::{ErrorObject, Notification, Request, Response};

/// Current AHP protocol version. Bump on breaking wire changes.
pub const AHP_VERSION: &str = "0.1.0";

/// Current IPC protocol version.
pub const IPC_VERSION: &str = "0.1.0";

// ============================================================================
// AHP: methods the adapter implements (daemon → adapter)
// ============================================================================

pub mod ahp_method {
    pub const INITIALIZE: &str = "initialize";
    pub const SESSION_START: &str = "session.start";
    pub const SESSION_INPUT: &str = "session.input";
    pub const SESSION_PTY_INPUT: &str = "session.pty_input";
    pub const SESSION_PTY_RESIZE: &str = "session.pty_resize";
    pub const SESSION_INTERRUPT: &str = "session.interrupt";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SHUTDOWN: &str = "shutdown";
}

pub mod ahp_notif {
    /// Adapter → daemon: a [`super::SessionEvent`] occurred.
    pub const EVENT: &str = "session/event";
    /// Adapter → daemon: free-form log line for the daemon's log.
    pub const LOG: &str = "log";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeParams {
    pub protocol_version: String,
    #[serde(default)]
    pub client_info: ClientInfo,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ClientInfo {
    #[serde(default)]
    pub name: String,
    #[serde(default)]
    pub version: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InitializeResult {
    pub name: String,
    pub version: String,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Capabilities {
    #[serde(default)]
    pub supports_input: bool,
    #[serde(default)]
    pub supports_interrupt: bool,
    #[serde(default)]
    pub supports_diff: bool,
    #[serde(default)]
    pub supports_cost: bool,
    /// Adapter owns a PTY for this session: emits [`SessionEvent::Pty`]
    /// events and accepts `session.pty_input` / `session.pty_resize`.
    #[serde(default)]
    pub supports_pty: bool,
    #[serde(default)]
    pub models: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionStartParams {
    pub session_id: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Adapter-defined session mode (e.g. `"interactive"` / `"headless"`).
    /// Each adapter chooses its own default if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Initial PTY size — set by the client when starting in PTY mode.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_size: Option<PtySize>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub struct PtySize {
    pub cols: u16,
    pub rows: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPtyInputParams {
    pub session_id: String,
    /// Base64-encoded raw bytes to write to the child's PTY.
    pub data: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionPtyResizeParams {
    pub session_id: String,
    pub cols: u16,
    pub rows: u16,
}

impl SessionPtyInputParams {
    pub fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        use base64::Engine;
        base64::engine::general_purpose::STANDARD.decode(&self.data)
    }

    pub fn from_bytes(session_id: impl Into<String>, bytes: &[u8]) -> Self {
        use base64::Engine;
        Self {
            session_id: session_id.into(),
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionIdParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionInputParams {
    pub session_id: String,
    pub text: String,
}

/// Payload carried by the [`ahp_notif::EVENT`] notification from adapter → daemon.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventEnvelope {
    pub session_id: String,
    pub event: SessionEvent,
}

/// A structured event emitted by an adapter while running a session.
///
/// Adapters whose underlying CLI is plain text can lean on
/// [`SessionEvent::Message`] for everything; richer harnesses should emit the
/// more specific variants so the UI can render them distinctively.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum SessionEvent {
    Message {
        role: MessageRole,
        text: String,
    },
    ToolUse {
        tool: String,
        #[serde(default)]
        args: serde_json::Value,
    },
    ToolResult {
        tool: String,
        ok: bool,
        #[serde(default)]
        output: String,
    },
    AwaitingInput {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prompt: Option<String>,
    },
    Status {
        state: SessionState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
    Cost {
        #[serde(default)]
        usd: f64,
        #[serde(default)]
        tokens_in: u64,
        #[serde(default)]
        tokens_out: u64,
    },
    Diff {
        patch: String,
    },
    Error {
        message: String,
    },
    Done {
        #[serde(default)]
        exit_code: i32,
    },
    /// Raw byte chunk from the session's PTY. `data` is base64-encoded so the
    /// JSON transport doesn't have to deal with arbitrary byte sequences.
    Pty {
        data: String,
    },
}

impl SessionEvent {
    /// Build a [`SessionEvent::Pty`] from raw bytes.
    pub fn pty(bytes: &[u8]) -> Self {
        use base64::Engine;
        SessionEvent::Pty {
            data: base64::engine::general_purpose::STANDARD.encode(bytes),
        }
    }

    /// Decode a [`SessionEvent::Pty`] payload back to raw bytes.
    pub fn pty_bytes(&self) -> Option<Vec<u8>> {
        match self {
            SessionEvent::Pty { data } => {
                use base64::Engine;
                base64::engine::general_purpose::STANDARD.decode(data).ok()
            }
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MessageRole {
    User,
    Assistant,
    System,
    Tool,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq, Hash)]
#[serde(rename_all = "snake_case")]
pub enum SessionState {
    Pending,
    Running,
    AwaitingInput,
    Paused,
    Done,
    Errored,
}

impl SessionState {
    pub fn glyph(self) -> &'static str {
        match self {
            SessionState::Pending => "○",
            SessionState::Running => "●",
            SessionState::AwaitingInput => "◐",
            SessionState::Paused => "⏸",
            SessionState::Done => "✓",
            SessionState::Errored => "✗",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            SessionState::Pending => "pending",
            SessionState::Running => "running",
            SessionState::AwaitingInput => "awaiting input",
            SessionState::Paused => "paused",
            SessionState::Done => "done",
            SessionState::Errored => "errored",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, SessionState::Done | SessionState::Errored)
    }
}

// ============================================================================
// IPC: methods the daemon exposes to clients
// ============================================================================

pub mod ipc_method {
    pub const PING: &str = "ping";
    pub const HARNESS_LIST: &str = "harness.list";
    pub const SESSION_LIST: &str = "session.list";
    pub const SESSION_CREATE: &str = "session.create";
    pub const SESSION_GET: &str = "session.get";
    pub const SESSION_INPUT: &str = "session.input";
    pub const SESSION_PTY_INPUT: &str = "session.pty_input";
    pub const SESSION_PTY_RESIZE: &str = "session.pty_resize";
    pub const SESSION_PTY_REPLAY: &str = "session.pty_replay";
    pub const SESSION_INTERRUPT: &str = "session.interrupt";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SESSION_KILL: &str = "session.kill";
    pub const SESSION_DIFF: &str = "session.diff";
    pub const SESSION_TRANSCRIPT: &str = "session.transcript";
    pub const SUBSCRIBE_EVENTS: &str = "subscribe.events";
    pub const UNSUBSCRIBE_EVENTS: &str = "unsubscribe.events";
}

pub mod ipc_notif {
    pub const EVENT: &str = "session/event";
    pub const STATE: &str = "session/state";
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HarnessInfo {
    pub name: String,
    pub available: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default)]
    pub capabilities: Capabilities,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSummary {
    pub id: String,
    pub harness: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    pub state: SessionState,
    pub created_at: chrono::DateTime<chrono::Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_event_at: Option<chrono::DateTime<chrono::Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cost_usd: Option<f64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub worktree: Option<String>,
    #[serde(default)]
    pub pending_input: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    #[serde(default)]
    pub event_count: u64,
    /// True if the session's adapter owns a PTY (clients should default the
    /// view to terminal mode).
    #[serde(default)]
    pub has_pty: bool,
    /// The session's mode, as reported when it was started (e.g. "interactive"
    /// or "headless"). Adapter-defined.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyReplayResult {
    /// Base64-encoded raw bytes representing the recent PTY history (best
    /// effort — the daemon keeps a bounded ring buffer).
    pub data: String,
    /// Most recent known PTY size for the session, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<PtySize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub events: Vec<TimestampedEvent>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TimestampedEvent {
    pub seq: u64,
    pub at: chrono::DateTime<chrono::Utc>,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionParams {
    pub harness: String,
    pub cwd: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// Adapter-defined mode. Conventional values: `"interactive"` (PTY) or
    /// `"headless"` (structured stream). Adapters pick their own default.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Initial PTY size if the adapter is going to allocate a PTY.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pty_size: Option<PtySize>,
    #[serde(default)]
    pub worktree: bool,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub args: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateSessionResult {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DiffResult {
    pub patch: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptParams {
    pub session_id: String,
    #[serde(default)]
    pub from: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TranscriptResult {
    pub events: Vec<TimestampedEvent>,
    pub total: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SubscribeParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EventNotificationPayload {
    pub session_id: String,
    pub at: chrono::DateTime<chrono::Utc>,
    pub event: SessionEvent,
    pub seq: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StateNotificationPayload {
    pub session: SessionSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub pong: bool,
    pub version: String,
}
