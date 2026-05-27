# Unified tool layer

agentd exposes one shared tool surface across supported agents. The same
capabilities are available to built-in Zarvis directly and to MCP-capable
harnesses through `agentd-mcp`, so agents can coordinate the fleet without
shelling out to ad-hoc CLI commands.

## MCP server injection

| Harness | Server/config | Status | Notes |
|---|---|---|---|
| Claude Code | `agentd-mcp` via generated MCP config | Enabled by default | Adapter writes `$STATE_DIR/mcp/<session_id>.json` and passes `--mcp-config <path>`. |
| Codex | `agentd-mcp` via generated MCP config | Enabled by default | Adapter writes `$STATE_DIR/mcp/<session_id>.json` and passes `--mcp-config <path>`. |
| Zarvis | Native tool layer | Built in | Uses the same tool surface without an external MCP process. |
| Antigravity | Pending upstream support | Not injected yet | Receives `AGENTD_SESSION_ID`; browser/tools can be injected once `agy` exposes an MCP config flag. |

Opt out of MCP injection with `AGENTD_INJECT_MCP=0` in the daemon environment.

## Environment passed to child agents

| Variable | Purpose |
|---|---|
| `AGENTD_SESSION_ID` | Identifies the calling session, so tools can avoid acting on themselves. |
| `AGENTD_RUNTIME_DIR` / `AGENTD_STATE_DIR` / `AGENTD_DATA_DIR` / `AGENTD_CONFIG_DIR` | Point tools at the same daemon and storage layout as the parent session. |
| `AGENTD_GLOBAL_MEMORY_FILE` / `AGENTD_PROJECT_MEMORY_FILE` / `AGENTD_PROJECT_ID` | Point `agentd_context` at the Markdown memory files for the session. |
| `AGENTD_SESSION_WIDGETS_DIR` | Points agents at the current session's file-backed widget directory. Prefer reading it from `agentd_context` so the agent also sees widget policy and supported Markdown extensions. |

## Fleet-control tools

| Tool | Purpose |
|---|---|
| `agentd_context` | Load the calling session's agentd context, including global/project memory, memory file paths, session widget paths, widget policy, and memory maintenance policy. Agents should call this before starting a user task. See [Memory](memory.md) and [Generative widgets](generative-widgets.md). |
| `agentd_whoami` | Return the current session id. |
| `agentd_list_sessions` | List sessions, status, cwd, pinned state, automode, and grouping metadata. |
| `agentd_get_session` | Fetch summary and structured transcript for one session. |
| `agentd_get_transcript` | Fetch a slice of a session event log. |
| `agentd_get_output` | Read recent PTY scrollback. |
| `agentd_get_diff` | Read `git diff HEAD` for the session worktree. |
| `agentd_list_harnesses` | Show available harness adapters. |
| `agentd_create_session` | Spawn a new session, optionally in an isolated worktree. |
| `agentd_send_input` | Send line-oriented input to a session. |
| `agentd_send_keys` | Send raw PTY bytes, such as control keys. |
| `agentd_interrupt_session` | Interrupt the active turn/process. |
| `agentd_stop_session` | Ask a session to wind down cleanly. |
| `agentd_kill_session` | Kill a session immediately. |
| `agentd_delete_session` | Delete a session and its stored transcript/worktree. |
| `agentd_pin_session` | Toggle the pinned flag. |
| `agentd_rename_session` | Set a user-facing title. |

## Browser tools

| Tool | Purpose |
|---|---|
| `browser_open` | Open a URL in Chrome through DevTools. |
| `browser_inspect` | Read page title, URL, visible text, and links. |
| `browser_screenshot` | Capture a tab screenshot and emit a TUI preview. |
| `browser_eval` | Evaluate JavaScript for automation or DOM extraction. |

Browser tools emit a `BrowserPreview` event back to the calling session, so the
TUI thumbnail updates for MCP-capable harnesses the same way it does for
Zarvis-native browser calls.

## Generative widgets

Agents can create session-scoped UI widgets by writing Markdown files into the
`session_widgets.dir` returned by `agentd_context`. See
[Generative widgets](generative-widgets.md) for the file format, lifecycle,
rendering behavior, and action-link semantics.
