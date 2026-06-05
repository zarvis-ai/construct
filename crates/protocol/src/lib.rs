//! agentd wire protocol.
//!
//! Two protocols share JSON-RPC 2.0 over line-delimited JSON:
//!
//! - **AHP** (Agent Harness Protocol): daemon ⇄ adapter, over the adapter's stdio.
//! - **IPC**: client ⇄ daemon, over a Unix socket.
//!
//! Both reuse the envelope types in [`jsonrpc`] and the same [`transport`] helpers.

pub mod adapter;
pub mod agent_context;
pub mod jsonrpc;
pub mod paths;
pub mod slash;
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
    pub const SESSION_SET_APPROVAL_MODE: &str = "session.set_approval_mode";
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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAttachClipboardParams {
    pub session_id: String,
    /// Base64-encoded clipboard/file bytes.
    pub data: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filename: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mime: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionAttachClipboardResult {
    /// Absolute path written on the daemon host.
    pub path: String,
    /// Short text clients can insert into the prompt.
    pub reference: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionEmitEventParams {
    pub session_id: String,
    pub event: SessionEvent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionWidgetDeleteParams {
    pub session_id: String,
    pub panel_id: String,
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

/// Browser preview image for UI-only overlays. This is emitted by
/// browser-aware tools so clients can show what the agent is viewing
/// without stuffing screenshots into the textual tool result that the
/// model consumes.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BrowserPreview {
    pub url: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    /// PNG bytes encoded as base64.
    pub image: String,
    #[serde(default)]
    pub width: u32,
    #[serde(default)]
    pub height: u32,
}

/// Declarative, adapter-owned session UI panel. This is intentionally a
/// small semantic vocabulary for the first dynamic-UI pass: renderers own
/// layout/resize/focus while harnesses own the task state being displayed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiPanel {
    pub id: String,
    /// Source filename for file-backed widgets. Used as title fallback and
    /// omitted for ephemeral event-backed panels.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub placement: UiPlacement,
    /// Safe agentd-markdown: normal markdown plus semantic links such as
    /// `[Run checks](agentd:action/run-checks)`. Renderers parse the subset
    /// they understand and degrade the rest as text.
    #[serde(default)]
    pub markdown: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum UiPlacement {
    Inline,
    #[default]
    Sticky,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiAction {
    pub id: String,
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub style: Option<String>,
    /// Close the containing widget after dispatch. Parsed from action links
    /// such as `[OK](agentd:action/ok?close=1)`.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub close: bool,
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
    /// Free-form "thinking" / "reasoning" text streamed from the
    /// model — separate from `Message` so the TUI can render it
    /// distinctively (dim italic) and so harnesses that don't surface
    /// reasoning don't see it in their normal message stream.
    /// Adapters that don't have access to reasoning content can
    /// simply never emit this.
    Reasoning {
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
    /// UI-only browser preview. Clients may render this as an overlay;
    /// it is not intended to be fed back to the model as tool output.
    BrowserPreview(BrowserPreview),
    /// Declarative session UI panel created or replaced by an adapter.
    UiPanel(UiPanel),
    /// Delete a previously-created session UI panel by id.
    UiDelete {
        id: String,
    },
    Cost {
        #[serde(default)]
        usd: f64,
        #[serde(default)]
        tokens_in: u64,
        #[serde(default)]
        tokens_out: u64,
        /// Cached input tokens (subset of `tokens_in` served from the provider's
        /// prompt cache). 0 when unknown or unsupported by the provider.
        #[serde(default)]
        tokens_cached: u64,
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
    /// The session's single PTY was resized to `cols` x `rows` (by whichever
    /// client last claimed geometry — TUI or web). UI-only and transient
    /// (never persisted/replayed): lets a passive viewer whose viewport is
    /// narrower render at the real width instead of wrapping the output.
    PtyResize {
        cols: u16,
        rows: u16,
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
        /// Whether the UI should offer an auto-review retry action. Adapters
        /// set this to false when auto-review already vetted this call and
        /// deferred to the user; showing the same action again is redundant.
        #[serde(default = "default_true")]
        allow_auto_review: bool,
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
    TaskBackgrounded {
        call_id: String,
    },
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
    /// Adapter compacted older conversation turns into an
    /// LLM-generated summary so the rolling context stays under the
    /// model's input-token budget without silently dropping history.
    /// Emitted by interactive adapters (currently zarvis) when either
    /// the user runs `/compact` or auto-compact fires near the budget
    /// ceiling. Carries enough info for the TUI to render a banner
    /// card; full summary text is also in the transcript as the new
    /// synthetic user turn the adapter inserted at the head.
    ContextCompacted {
        /// Number of turn pairs preserved verbatim after the summary.
        #[serde(default)]
        kept_turns: u32,
        /// Number of turn pairs that were folded into the summary.
        #[serde(default)]
        dropped_turns: u32,
        /// Approximate token count of the conversation *before* the
        /// compaction ran (chars/3.5 heuristic, same as the rolling
        /// budget check).
        #[serde(default)]
        tokens_before: u64,
        /// Approximate token count *after* the compaction. Always
        /// less than `tokens_before` on a successful compact.
        #[serde(default)]
        tokens_after: u64,
        /// First ~160 chars of the summary, for the TUI banner. Full
        /// text lives in the inserted user turn.
        #[serde(default)]
        summary_preview: String,
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
    /// A client-routed slash command, dispatched by an adapter for the
    /// attached client to execute (`/zoom`, `/new`, `/remote-control`, …).
    ///
    /// Replaces the old `tui` `ToolUse` convention: instead of encoding the
    /// command as a fake tool call (which forced every consumer to
    /// string-sniff `"tui"` and accidentally leaked UI noise into
    /// `agentd_get_transcript`), it carries the typed [`slash::CommandId`].
    /// The daemon and the transcript-read path look the command's
    /// [`slash::TranscriptPolicy`] / [`slash::ModelVisibility`] up in
    /// [`slash::COMMANDS`] and honor it — persistence and model-visibility
    /// become a property of the command, not a special case.
    ClientCommand {
        id: slash::CommandId,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        args: Option<String>,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalMode {
    #[default]
    Manual,
    AutoReview,
    UnsafeAuto,
}

impl ApprovalMode {
    pub fn badge(self) -> Option<&'static str> {
        match self {
            ApprovalMode::Manual => None,
            ApprovalMode::AutoReview => Some("auto-review"),
            ApprovalMode::UnsafeAuto => Some("unsafe-auto"),
        }
    }
}

/// Coarse risk classification used by adapters that gate tool calls behind
/// user approval. Two tiers are intentional; finer-grained policies can be
/// layered without changing the protocol (the `decision` field on
/// [`SessionToolDecisionParams`] is an open string).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolRisk {
    /// Read-only / observation. Adapter runs these without prompting.
    Safe,
    /// Mutates filesystem / sessions / external state. Adapter gates these
    /// according to the session's approval mode.
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
    pub const SESSION_ATTACH_CLIPBOARD: &str = "session.attach_clipboard";
    pub const SESSION_PTY_INPUT: &str = "session.pty_input";
    pub const SESSION_PTY_RESIZE: &str = "session.pty_resize";
    pub const SESSION_PTY_REPLAY: &str = "session.pty_replay";
    pub const SESSION_INTERRUPT: &str = "session.interrupt";
    pub const SESSION_STOP: &str = "session.stop";
    pub const SESSION_KILL: &str = "session.kill";
    pub const SESSION_DELETE: &str = "session.delete";
    pub const SESSION_WIDGET_DELETE: &str = "session.widget.delete";
    /// Respawn a session's adapter — typically used to bring a `Done`
    /// session back to life so the user can continue typing. The
    /// adapter is launched with `AGENTD_RESUME=1` so harnesses that
    /// persist conversation state (e.g. zarvis) can pick up where
    /// they left off.
    pub const SESSION_RESTART: &str = "session.restart";
    pub const SESSION_SET_PINNED: &str = "session.set_pinned";
    pub const SESSION_SET_TITLE: &str = "session.set_title";
    pub const SESSION_SET_APPROVAL_MODE: &str = "session.set_approval_mode";
    pub const SESSION_TOOL_DECISION: &str = "session.tool_decision";
    pub const SESSION_TOOL_ACTION: &str = "session.tool_action";
    pub const SESSION_LIST_TASKS: &str = "session.list_tasks";
    /// Append/broadcast a structured event for a session. Intended for trusted
    /// local helpers such as agentd-mcp that run outside an adapter but need to
    /// surface UI-only state (for example browser previews) in the caller's
    /// session.
    pub const SESSION_EMIT_EVENT: &str = "session.emit_event";
    pub const LOOP_CREATE: &str = "loop.create";
    pub const LOOP_LIST: &str = "loop.list";
    pub const LOOP_UPDATE: &str = "loop.update";
    pub const LOOP_REMOVE: &str = "loop.remove";
    pub const SESSION_MOVE: &str = "session.move";
    pub const SESSION_SET_GROUP: &str = "session.set_group";
    pub const SESSION_SET_PROJECT: &str = "session.set_project";
    pub const GROUP_LIST: &str = "group.list";
    pub const GROUP_CREATE: &str = "group.create";
    pub const GROUP_RENAME: &str = "group.rename";
    pub const GROUP_DELETE: &str = "group.delete";
    pub const GROUP_SET_COLLAPSED: &str = "group.set_collapsed";
    pub const GROUP_MOVE: &str = "group.move";
    pub const PROJECT_LIST: &str = "project.list";
    pub const PROJECT_CREATE: &str = "project.create";
    pub const PROJECT_RENAME: &str = "project.rename";
    pub const PROJECT_DELETE: &str = "project.delete";
    pub const PROJECT_SET_COLLAPSED: &str = "project.set_collapsed";
    pub const PROJECT_MOVE: &str = "project.move";
    pub const SESSION_DIFF: &str = "session.diff";
    pub const SESSION_TRANSCRIPT: &str = "session.transcript";
    pub const SUBSCRIBE_EVENTS: &str = "subscribe.events";
    pub const UNSUBSCRIBE_EVENTS: &str = "unsubscribe.events";
    /// Start the remote WS listener + cloudflared tunnel (idempotent)
    /// and return a URL + QR the caller can show the user. Lets the
    /// TUI's `/remote-control` slash command surface the QR on
    /// demand instead of requiring the env-var-at-startup flow.
    pub const REMOTE_START: &str = "remote.start";
    /// Take the remote WS listener back down: kill the cloudflared
    /// subprocess (URL stops resolving), drop the listener (no new
    /// connections accepted), and clear the daemon-side
    /// `RemoteState` so the next `/remote-control` invocation mints
    /// a fresh token. Existing in-flight connections die naturally
    /// once cloudflared exits. Idempotent — calling stop when
    /// nothing is running returns Ok with `was_running: false`.
    pub const REMOTE_STOP: &str = "remote.stop";
    /// Restart the daemon process in place — persist remote state
    /// (token, password, port, tunnel URL) to disk, exec() the
    /// current executable so any on-disk binary upgrade is picked
    /// up. cloudflared subprocess is left running (detached process
    /// group) so the public URL survives the restart; the new
    /// daemon binds the same local port and adopts the still-alive
    /// tunnel without re-spawning. Returns the new exe path and
    /// timestamp the new daemon will report on startup; the IPC
    /// connection is closed by the kernel during exec(), so clients
    /// detect the restart as a socket close and reconnect.
    pub const DAEMON_RESTART: &str = "daemon.restart";
    /// Dev tooling: point the running daemon at a directory of web-UI
    /// assets (`index.html`, `static/*`) to serve from disk instead of
    /// the binary's embedded copies, and inject a live-reload poller so
    /// edits show up on browser refresh / automatically. `dir: None`
    /// reverts to the embedded assets. Lets you iterate on `index.html`
    /// in a worktree against an already-running daemon — no rebuild or
    /// restart.
    pub const DEV_SET_ASSETS: &str = "dev.set_assets";
}

pub mod ipc_notif {
    pub const EVENT: &str = "session/event";
    pub const STATE: &str = "session/state";
    pub const DELETED: &str = "session/deleted";
    pub const GROUP_STATE: &str = "group/state";
    pub const GROUP_DELETED: &str = "group/deleted";
    pub const PROJECT_STATE: &str = "project/state";
    pub const PROJECT_DELETED: &str = "project/deleted";
    /// Aggregate state for the daemon's remote WebSocket transport:
    /// how many remote clients are currently attached. Broadcast on
    /// every connect / disconnect so the local TUI can render a
    /// "● remote attached" badge as a visible signal that another
    /// surface is also driving the daemon.
    pub const REMOTE_STATE: &str = "remote/state";
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
    Subagent,
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
    /// Parent session for internal child sessions such as Zarvis subagents.
    /// Clients can render these under the owning user session instead of as
    /// ordinary top-level sessions.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
    /// Unix epoch ms of the most recent PTY byte received from the adapter,
    /// or `None` if this session has never produced PTY output. Clients use
    /// `now - last_pty_at_ms < quiescence_window` as a "session looks busy"
    /// heuristic (drives the TUI's spinner; useful for MCP-driven agents to
    /// avoid sending input while the agent is mid-turn).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_pty_at_ms: Option<i64>,
    /// How adapters that gate tools handle Risky tool calls.
    #[serde(default)]
    pub approval_mode: ApprovalMode,
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

/// Project-named compatibility shape for `session.set_project`.
/// Internally this still maps to the session's persisted `group_id`
/// until the storage model is renamed.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionSetProjectParams {
    pub session_id: String,
    /// `None` removes the session from its project.
    #[serde(default, alias = "group_id", skip_serializing_if = "Option::is_none")]
    pub project_id: Option<String>,
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
pub struct SessionSetApprovalModeParams {
    pub session_id: String,
    pub mode: ApprovalMode,
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
    /// One of `"approve"`, `"deny"`, `"auto_review"`, `"unsafe_auto"`.
    /// Open string so finer decisions can be added without a protocol break.
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
    /// Base64-encoded raw bytes representing the requested PTY log range.
    pub data: String,
    /// Absolute byte offset where `data` starts in `pty.log`.
    #[serde(default)]
    pub start_offset: u64,
    /// Absolute byte offset where `data` ends in `pty.log`.
    #[serde(default)]
    pub end_offset: u64,
    /// Current total byte length of `pty.log`.
    #[serde(default)]
    pub total_bytes: u64,
    /// Most recent known PTY size for the session, if available.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size: Option<PtySize>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PtyReplayParams {
    pub session_id: String,
    /// Return up to this many bytes before `before_offset`. When omitted,
    /// defaults to the historical replay cap from the current log tail.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_bytes: Option<usize>,
    /// Exclusive end offset for the requested range. When omitted, uses the
    /// current end of `pty.log`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub before_offset: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionDetail {
    pub summary: SessionSummary,
    pub events: Vec<TimestampedEvent>,
    /// Current durable, file-backed UI widgets for this session. Kept separate
    /// from `events` so widgets persist without entering model-facing history.
    #[serde(default)]
    pub ui_panels: Vec<UiPanel>,
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
    /// Parent session for internal child sessions such as Zarvis subagents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session_id: Option<String>,
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
    /// Return the most-recent `tail` events instead of paginating forward
    /// from `from`. When set, `from` and `limit` are ignored. Used by the
    /// webui to render the live tail of long histories immediately while it
    /// background-loads older pages.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tail: Option<usize>,
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

/// Payload of the `remote/state` notification — number of remote WS
/// clients currently attached to the daemon. Local clients (Unix
/// socket) don't count; this is specifically the "is someone else
/// also driving this daemon over the phone web client" signal.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStateNotificationPayload {
    pub clients: u32,
}

/// Params for the `remote.start` IPC method. Default = "set up
/// the full public tunnel" (what the user typing `/remote-control`
/// gets). `local_only` flips to localhost-only mode for the
/// `/remote-control-debug` flow and CI smoke tests — no
/// cloudflared spawn, no tunnel wait, just bind + return.
///
/// `password` is the optional user-supplied override for the HTTP
/// Basic auth gate. When unset, the daemon auto-generates a
/// memorable 3-token password (`swift-fox-42` shape). The chosen
/// password is returned in `RemoteStartResult` so the popup can
/// display it.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RemoteStartParams {
    #[serde(default)]
    pub local_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub password: Option<String>,
    /// Tunnel mode normally waits for cloudflared's public URL before
    /// replying. Interactive clients can set this false to get an
    /// immediate localhost result, paint a progress dialog, then poll
    /// again with waiting enabled in the background.
    #[serde(default = "default_true")]
    pub wait_for_tunnel: bool,
}

fn default_true() -> bool {
    true
}

/// Params for `dev.set_assets`. `dir: None` reverts to the embedded
/// web-UI assets.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevSetAssetsParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Result of `dev.set_assets`: the directory now in effect (`None` =
/// embedded assets).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DevAssetsResult {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dir: Option<String>,
}

/// Result of the `remote.start` IPC method. Always reflects what
/// the daemon was *asked* for (via `RemoteStartParams`), never a
/// "best effort fallback" — that asymmetry is what makes the two
/// slash verbs feel distinct. In tunnel mode the daemon waits up
/// to ~15s for cloudflared to publish its URL and returns a
/// JSON-RPC *error* if it doesn't; in local-only mode the URL is
/// always the `ws://127.0.0.1:<port>` form with `tunnel_ready =
/// false`, no hint, no waiting.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStartResult {
    /// The URL to share. `wss://<rand>.trycloudflare.com/t/<token>`
    /// in tunnel mode (always — failure produces a JSON-RPC error
    /// instead of a degraded URL), `ws://127.0.0.1:<port>/t/<token>`
    /// in local-only mode.
    pub url: String,
    /// Multi-line Unicode QR rendering of `url`. Empty when the
    /// encoder rejected the input (rare); callers should fall back
    /// to printing the URL alone.
    pub qr: String,
    /// True iff `url` is the public tunnel URL. Always true in
    /// successful tunnel mode; always false in local-only mode.
    pub tunnel_ready: bool,
    /// HTTP Basic auth password required to load the URL. Either
    /// the caller's override (from `RemoteStartParams.password`)
    /// or the daemon's auto-generated 3-token string. Username on
    /// the wire is ignored by the daemon — only the password
    /// matters.
    pub password: String,
    /// Optional user-readable hint. Reserved for unusual states —
    /// typically `None` because callers can infer from
    /// `tunnel_ready` whether they're looking at a public or
    /// local URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// Result of the `remote.stop` IPC method. `was_running: false` is
/// not an error — it means the caller invoked stop while nothing
/// was running, which is the natural state after fresh daemon
/// boot or a prior stop. The CLI surfaces this distinction so the
/// status line can say "remote stopped" vs "remote wasn't
/// running".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RemoteStopResult {
    pub was_running: bool,
}

/// Params for `daemon.restart`. `exe: None` re-execs the daemon's own
/// binary (the upgrade-in-place case); `Some(path)` execs a different
/// binary instead — e.g. a freshly-built one from a worktree. The path
/// is validated (must exist + be executable) before the restart fires,
/// so a typo returns an error rather than bricking the daemon.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DaemonRestartParams {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exe: Option<String>,
}

/// Result of `daemon.restart`. Echoed back to the caller right
/// before the daemon exec()s itself — clients see this reply, then
/// observe the IPC socket close as the new process replaces the
/// old. `exe` is the path the new process will load from (typically
/// the same `current_exe()`, but useful for the CLI to show "now
/// running build X" once it reconnects).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonRestartResult {
    pub exe: String,
    pub pid: u32,
    /// True iff a still-running cloudflared subprocess was found
    /// and the new daemon will adopt it rather than spawn a fresh
    /// tunnel. Lets the CLI emit "remote URL preserved" vs
    /// "remote URL will rotate" in the status line.
    pub tunnel_preserved: bool,
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

/// Project is the product name for the same persisted organizer that
/// older clients know as a group. Keep the wire payload identical
/// while clients migrate method and notification names.
pub type ProjectSummary = GroupSummary;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCreateParams {
    pub name: String,
}

pub type ProjectCreateParams = GroupCreateParams;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupCreateResult {
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectCreateResult {
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupIdParams {
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectIdParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
}

/// Parameters for `group.delete`. `delete_members` defaults to false
/// so a client that sends the older `GroupIdParams` shape (just
/// `{"group_id": "…"}`) still deserializes cleanly with the original
/// "orphan members" semantics.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDeleteParams {
    pub group_id: String,
    /// When true, cascade-delete every member session before removing
    /// the group. When false (default), members are orphaned —
    /// `group_id` on their summary clears to `None` and they survive.
    #[serde(default)]
    pub delete_members: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeleteParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    /// When true, cascade-delete every member session before removing
    /// the project. When false (default), members are orphaned.
    #[serde(default)]
    pub delete_members: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupRenameParams {
    pub group_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectRenameParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    pub name: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupSetCollapsedParams {
    pub group_id: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectSetCollapsedParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    pub collapsed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupMoveParams {
    pub group_id: String,
    pub direction: MoveDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectMoveParams {
    #[serde(alias = "group_id")]
    pub project_id: String,
    pub direction: MoveDirection,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupStateNotificationPayload {
    pub group: GroupSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectStateNotificationPayload {
    pub project: ProjectSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GroupDeletedNotificationPayload {
    pub group_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProjectDeletedNotificationPayload {
    pub project_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PingResult {
    pub pong: bool,
    pub version: String,
}

#[cfg(test)]
mod project_compat_tests {
    use super::*;

    #[test]
    fn project_params_accept_legacy_group_id_fields() {
        let p: SessionSetProjectParams = serde_json::from_value(serde_json::json!({
            "session_id": "s1",
            "group_id": "g1"
        }))
        .unwrap();
        assert_eq!(p.project_id.as_deref(), Some("g1"));

        let p: ProjectRenameParams = serde_json::from_value(serde_json::json!({
            "group_id": "g1",
            "name": "Project"
        }))
        .unwrap();
        assert_eq!(p.project_id, "g1");
    }

    #[test]
    fn project_results_use_project_id_on_the_wire() {
        let v = serde_json::to_value(ProjectCreateResult {
            project_id: "p1".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "project_id": "p1" }));

        let v = serde_json::to_value(ProjectDeletedNotificationPayload {
            project_id: "p1".into(),
        })
        .unwrap();
        assert_eq!(v, serde_json::json!({ "project_id": "p1" }));
    }
}
