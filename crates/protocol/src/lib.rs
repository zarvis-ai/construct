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
    pub const SESSION_TOOL_DECISION: &str = "session.tool_decision";
    pub const SESSION_TOOL_ACTION: &str = "session.tool_action";
    pub const SESSION_SET_AUTOMODE: &str = "session.set_automode";
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
    /// Adapter emits no startup escapes (banner, screen-clear, cursor
    /// reset) on resume — so the daemon can safely keep the prior
    /// incarnation's PTY ring + on-disk `pty.log` instead of clearing
    /// them. Without this guarantee, mixing old bytes with a fresh
    /// adapter's start-up sequence leaves vt100 in a half-rendered
    /// state, so the daemon defaults to clearing on respawn.
    #[serde(default)]
    pub supports_silent_resume: bool,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Live, adapter-owned turn status shown near the input editor while
/// an agent is working. Inactive statuses are ephemeral completion
/// notices; clients may render them locally without persisting them
/// into transcript or PTY data.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentStatus {
    pub active: bool,
    #[serde(default)]
    pub started_at_ms: i64,
    pub status: String,
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
    /// Live or just-completed agent turn status. The TUI renders this
    /// above queued input while active and may render inactive statuses
    /// as display-only history rows.
    AgentStatus(AgentStatus),
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
    /// Adapter requests that the daemon clear this session's persisted
    /// transcript and PTY replay history. Used by interactive adapters for
    /// slash commands such as `/reset`.
    Reset,
    Done {
        #[serde(default)]
        exit_code: i32,
    },
    /// Raw byte chunk from the session's PTY. `data` is base64-encoded so the
    /// JSON transport doesn't have to deal with arbitrary byte sequences.
    Pty {
        data: String,
    },
    /// Adapter is asking the user to approve (or deny) a pending tool call.
    /// The adapter parks the agent loop until a [`SessionToolDecisionParams`]
    /// arrives on the inbox referencing the same `call_id`. The `risk` field
    /// lets the UI render a badge; `args_summary` is pre-formatted by the
    /// adapter so the UI doesn't have to interpret each tool's schema.
    ToolApprovalRequest {
        call_id: String,
        tool: String,
        #[serde(default)]
        args_summary: String,
        risk: ToolRisk,
    },
    /// Tool lifecycle: adapter started running a tool. Carries the
    /// canonical `call_id` (unlike [`ToolUse`] which doesn't) so the
    /// daemon's per-session task registry can match this against the
    /// later [`TaskBackgrounded`] / [`TaskEnd`] events. Emitted in
    /// addition to `ToolUse`, not in place of it — the structured
    /// transcript and the model's tool-call stream still rely on
    /// `ToolUse` / `ToolResult`.
    TaskStart {
        call_id: String,
        tool: String,
        #[serde(default)]
        args_summary: String,
    },
    /// The tool's foreground budget elapsed (auto-bg at
    /// `AGENTD_TOOL_BG_AFTER_MS`) or the user clicked `[bg]` /
    /// invoked `session.tool_action { action: "background" }`. The
    /// adapter detached the join handle into its background pool;
    /// the agent's conversation got a placeholder result and moved
    /// on. A [`TaskEnd`] event will follow once the detached task
    /// actually completes.
    TaskBackgrounded { call_id: String },
    /// The tool finished (whichever path it took). `ok` and
    /// `output_preview` mirror the corresponding `ToolResult` event
    /// — `output_preview` is truncated for `/tasks` display; the
    /// full output is in the transcript via `ToolResult`.
    TaskEnd {
        call_id: String,
        ok: bool,
        #[serde(default)]
        output_preview: String,
    },
    /// State of an adapter's input editor, rendered by the TUI in a
    /// fixed pane below the chat scrollback. Emitted by the adapter
    /// (currently zarvis interactive) whenever the editor buffer,
    /// cursor, or pending-input queue changes. Lets the client paint
    /// a true bottom-anchored prompt that doesn't compete with the
    /// agent's stream for PTY rows.
    EditorState {
        /// Pending user submissions waiting for the agent — each
        /// shown as a "queued" row above the active prompt. The
        /// adapter coalesces consecutive submissions but exposes the
        /// per-submission strings so the TUI can render one line per
        /// pending entry if it likes.
        #[serde(default)]
        queued: Vec<String>,
        /// Current text in the editor buffer.
        #[serde(default)]
        buf: String,
        /// Character index of the cursor within `buf` (0 = before
        /// first char).
        #[serde(default)]
        cursor: usize,
        /// Completion suggestions for the current editor buffer.
        /// Rendered above the fixed prompt by clients so completions
        /// stay out of PTY scrollback.
        #[serde(default)]
        completions: Vec<String>,
    },
}

/// Lifecycle state surfaced by `session.list_tasks`. Derived by the
/// daemon from `SessionEvent::TaskStart` / `TaskBackgrounded` /
/// `TaskEnd` flowing from the adapter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Running,
    Backgrounded,
    Completed,
    Failed,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TaskInfo {
    pub call_id: String,
    pub tool: String,
    #[serde(default)]
    pub args_summary: String,
    pub state: TaskState,
    pub started_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backgrounded_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended_at_ms: Option<i64>,
    /// Truncated end-state preview (only meaningful for terminal
    /// states; `None` while still running / backgrounded).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_preview: Option<String>,
    /// `true` for a successful terminal state — distinguishes
    /// `Completed { ok: true }` from `Completed { ok: false }`.
    #[serde(default)]
    pub ok: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTasksParams {
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ListTasksResult {
    pub tasks: Vec<TaskInfo>,
}

/// Conventional tool name that adapters use to forward client-targeted
/// slash commands. The adapter emits a [`SessionEvent::ToolUse`] with
/// this name and an `args` object shaped like
/// `{"command": "<verb>", "args": "<rest>"}` (the `args` field is
/// omitted when the command was bare); it immediately follows up with
/// a synthetic [`SessionEvent::ToolResult`] so the transcript stays
/// balanced. The client TUI / CLI subscribes to `ToolUse` events,
/// recognizes this tool name, and dispatches its own slash table.
/// Defining it as a protocol constant (instead of a separate event
/// variant) keeps the wire format small and lets the LLM-initiated
/// path use the same tool when we later register it in zarvis's
/// catalog for natural-language UI actions.
pub const TUI_DISPATCH_TOOL: &str = "tui";

/// Coarse risk classification used by adapters that gate tool calls behind
/// user approval. Two tiers are intentional; finer-grained policies can be
/// layered without changing the protocol (the `decision` field on
/// [`SessionToolDecisionParams`] is an open string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    /// Read-only / observation. Adapter runs these without prompting even
    /// when automode is off.
    Safe,
    /// Mutates filesystem / sessions / external state. Adapter prompts the
    /// user when automode is off.
    Risky,
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
            SessionState::AwaitingInput => "●",
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
    pub const SESSION_DELETE: &str = "session.delete";
    /// Respawn a session's adapter — typically used to bring a `Done`
    /// session back to life so the user can continue typing. The
    /// adapter is launched with `AGENTD_RESUME=1` so harnesses that
    /// persist conversation state (e.g. zarvis) can pick up where
    /// they left off.
    pub const SESSION_RESTART: &str = "session.restart";
    pub const SESSION_SET_PINNED: &str = "session.set_pinned";
    pub const SESSION_SET_TITLE: &str = "session.set_title";
    pub const SESSION_SET_AUTOMODE: &str = "session.set_automode";
    pub const SESSION_TOOL_DECISION: &str = "session.tool_decision";
    pub const SESSION_TOOL_ACTION: &str = "session.tool_action";
    pub const SESSION_LIST_TASKS: &str = "session.list_tasks";
    pub const LOOP_CREATE: &str = "loop.create";
    pub const LOOP_LIST: &str = "loop.list";
    pub const LOOP_UPDATE: &str = "loop.update";
    pub const LOOP_REMOVE: &str = "loop.remove";
    pub const SESSION_MOVE: &str = "session.move";
    pub const SESSION_SET_GROUP: &str = "session.set_group";
    pub const GROUP_LIST: &str = "group.list";
    pub const GROUP_CREATE: &str = "group.create";
    pub const GROUP_RENAME: &str = "group.rename";
    pub const GROUP_DELETE: &str = "group.delete";
    pub const GROUP_SET_COLLAPSED: &str = "group.set_collapsed";
    pub const GROUP_MOVE: &str = "group.move";
    pub const SESSION_DIFF: &str = "session.diff";
    pub const SESSION_TRANSCRIPT: &str = "session.transcript";
    pub const SUBSCRIBE_EVENTS: &str = "subscribe.events";
    pub const UNSUBSCRIBE_EVENTS: &str = "unsubscribe.events";
}

pub mod ipc_notif {
    pub const EVENT: &str = "session/event";
    pub const STATE: &str = "session/state";
    pub const DELETED: &str = "session/deleted";
    pub const GROUP_STATE: &str = "group/state";
    pub const GROUP_DELETED: &str = "group/deleted";
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

/// Distinguishes the implicit orchestrator session — created by the
/// daemon at startup as the user's always-available command surface —
/// from ordinary user-created sessions. The orchestrator session is
/// otherwise identical to a user session; clients use this flag to
/// render it differently (hidden from the session list, surfaced in
/// the TUI's minibuffer).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionKind {
    #[default]
    User,
    Orchestrator,
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
    /// User-marked: should this session always be shown as a live tile in
    /// the TUI's pin strip, regardless of which session is currently selected?
    #[serde(default)]
    pub pinned: bool,
    /// Sort key for the list view. Sessions are ordered by `position`
    /// ascending; the daemon assigns `-created_at_ms` at creation so newer
    /// sessions appear at the top, and reorder operations swap the values.
    /// For a grouped session this is the position *within* its group.
    #[serde(default)]
    pub position: i64,
    /// Group membership. `None` = ungrouped (rendered at the top of the
    /// list); `Some(id)` = belongs to that group (rendered indented under
    /// the group's header, below the ungrouped region).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    /// Unix epoch ms of the most recent PTY byte received from the adapter,
    /// or `None` if this session has never produced PTY output. Clients use
    /// `now - last_pty_at_ms < quiescence_window` as a "session looks busy"
    /// heuristic (drives the TUI's spinner; useful for MCP-driven agents to
    /// avoid sending input while the agent is mid-turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pty_at_ms: Option<i64>,
    /// When true, the adapter auto-approves tool calls instead of pausing
    /// for user confirmation. Only meaningful for adapters that gate tool
    /// calls (zarvis today; future agent harnesses). Toggle via
    /// `session.set_automode`.
    #[serde(default)]
    pub automode: bool,
    /// Distinguishes the orchestrator session (daemon-created, hidden
    /// from the session list) from ordinary user sessions. Persisted
    /// in `meta.json` so the daemon recognizes the orchestrator
    /// across restarts.
    #[serde(default)]
    pub kind: SessionKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MoveDirection {
    Up,
    Down,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMoveParams {
    pub session_id: String,
    pub direction: MoveDirection,
}

/// Move a session into a group (or ungroup it by passing
/// `group_id: None`). `position` controls where in the target region
/// the session lands — `Top` of the list or `Bottom`. Default is
/// `Bottom` so a newly-grouped session appears at the end.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionGroupPosition {
    Top,
    Bottom,
}

impl Default for SessionGroupPosition {
    fn default() -> Self {
        Self::Bottom
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetGroupParams {
    pub session_id: String,
    /// `None` ungroups the session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
    #[serde(default)]
    pub position: SessionGroupPosition,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetPinnedParams {
    pub session_id: String,
    pub pinned: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetTitleParams {
    pub session_id: String,
    /// `None` clears any user-set title (display falls back to the hash).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetAutomodeParams {
    pub session_id: String,
    /// `true` = adapter runs all tools without prompting.
    /// `false` = adapter pauses on Risky tools (default).
    pub on: bool,
}

/// Params for `session.tool_action` — the client asks the adapter
/// to take an action on a running tool call (the user clicked a
/// `[kill]` / `[bg]` button, or pressed an override key). The
/// adapter looks up `call_id` in its running-tasks registry and
/// either aborts the task (`"kill"`) or moves it to the
/// background pool (`"background"`); unknown actions are ignored
/// with a warning.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToolActionParams {
    pub session_id: String,
    pub call_id: String,
    /// One of `"kill"`, `"background"`. Open string so finer
    /// actions can be added without a protocol break.
    pub action: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionToolDecisionParams {
    pub session_id: String,
    /// Matches the `call_id` in the originating
    /// [`SessionEvent::ToolApprovalRequest`].
    pub call_id: String,
    /// One of `"approve"`, `"deny"`, `"automode"`. Open string so finer
    /// decisions can be added without a protocol break.
    pub decision: String,
}

/// Schedule for a recurring prompt injection. v1 supports interval
/// only; the wire format is open so future cron expression support
/// is additive.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum LoopSpec {
    /// Fire every `seconds` seconds.
    Interval { seconds: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Loop {
    pub id: String,
    pub session_id: String,
    pub spec: LoopSpec,
    pub prompt: String,
    pub created_at_ms: i64,
    pub next_fire_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_fired_at_ms: Option<i64>,
    #[serde(default)]
    pub fire_count: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopCreateParams {
    pub session_id: String,
    pub spec: LoopSpec,
    pub prompt: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopListParams {
    /// `None` = list every session's loops; `Some` = scope to one
    /// session. The MCP / CLI surface uses the unscoped form; the
    /// zarvis tool defaults to the calling session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopListResult {
    pub loops: Vec<Loop>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopUpdateParams {
    pub loop_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<LoopSpec>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoopRemoveParams {
    pub loop_id: String,
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
    /// Marks an internal daemon caller as creating the orchestrator
    /// session. Public IPC clients should leave this as
    /// `SessionKind::User`; daemon-internal `ensure_orchestrator`
    /// passes `Orchestrator`.
    #[serde(default)]
    pub kind: SessionKind,
    /// Group to file the new session under. `None` (default) creates
    /// an ungrouped session. The TUI uses this to auto-join the new
    /// session to whichever group is currently selected (or contains
    /// the selected session) so creating a session from inside a
    /// group keeps the user's mental grouping intact.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group_id: Option<String>,
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
pub struct DeletedNotificationPayload {
    pub session_id: String,
}

/// A user-created group used to organize sessions in the list view.
/// Sessions can belong to at most one group; ungrouped sessions render at
/// the top of the list, groups render below them in `position` order.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupSummary {
    pub id: String,
    pub name: String,
    pub created_at: chrono::DateTime<chrono::Utc>,
    /// Sort key among groups (smaller = nearer the top of the groups
    /// region). Groups never sort above ungrouped sessions.
    #[serde(default)]
    pub position: i64,
    /// When true, the group's members are hidden from the list view —
    /// only the header is shown. Toggled by `Space` in the TUI.
    #[serde(default)]
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCreateParams {
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCreateResult {
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupIdParams {
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRenameParams {
    pub group_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupSetCollapsedParams {
    pub group_id: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMoveParams {
    pub group_id: String,
    pub direction: MoveDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupStateNotificationPayload {
    pub group: GroupSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDeletedNotificationPayload {
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub pong: bool,
    pub version: String,
}
