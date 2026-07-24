# Unified tool layer

This page is the detailed reference for the **Unified tools** capability in
[Harnesses](harnesses.md#what-construct-gives-every-harness).

Unified tools let agents inspect, control, and coordinate the construct fleet. For
example:

- A review agent can read the implementer's diff before commenting.
- A coordinator can spawn a shell session to run tests.
- An agent can open a browser, inspect the page, and show the preview in the TUI.
- A session can publish a status widget without knowing whether the user is in
  the TUI or Web UI.

Smith uses these tools natively. MCP-capable harnesses receive the same tools
through the construct MCP server (`construct __mcp`), so they can coordinate the
fleet without shelling out to ad-hoc `construct` CLI commands.

## Using unified tools

There is usually nothing to configure. smith sees these tools natively. Claude
Code and Codex receive them automatically via `construct __mcp`; set
`CONSTRUCT_INJECT_MCP=0` in the daemon environment to opt out.

Agents invoke these tools during tasks, just like their other tools. A quick way
to verify injection is to ask a Claude or Codex session to list available construct
sessions; it should be able to use `agentd_list_sessions` without running the
`construct` CLI in a shell.

## Harness support

| Harness | User-facing status | Implementation notes |
|---|---|---|
| Smith | Built in. | Uses the same tool set without an external MCP process. |
| Claude Code | Enabled by default. | Adapter writes a config under `CONSTRUCT_STATE_DIR` and passes `--mcp-config <path>` pointing to `construct __mcp`. |
| Codex | Enabled by default. | Adapter passes Codex a `-c mcp_servers.construct=...` TOML override pointing to `construct __mcp`. |
| Antigravity | Not injected yet. | Receives `CONSTRUCT_SESSION_ID`; browser/tools can be injected once `agy` exposes an MCP config flag. |
| Hermes | Not injected yet. | Hermes supports MCP in persistent config, but the adapter does not mutate user configuration; injection waits for a per-invocation config surface. |

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
| `agentd_send_input` | Send a line of text to a session. |
| `agentd_send_keys` | Send raw PTY bytes, such as control keys. |
| `agentd_interrupt_session` | Interrupt the active turn/process. |
| `agentd_stop_session` | Ask a session to wind down cleanly. |
| `agentd_kill_session` | Kill a session immediately. |
| `agentd_delete_session` | Delete a session and its stored transcript/worktree. |
| `agentd_pin_session` | Toggle the pinned flag. |
| `agentd_rename_session` | Set a user-facing title. |
| `agentd_set_session_group` | Move a session into, out of, or within a group. |
| `agentd_move_session` | Reorder a session in the visible session list. |
| `agentd_loop_create` / `agentd_loop_list` / `agentd_loop_update` / `agentd_loop_remove` | Manage recurring prompts attached to sessions. |

## Browser tools

| Tool | Purpose |
|---|---|
| `browser_open` | Open a URL in Chrome through DevTools. |
| `browser_inspect` | Read page title, URL, visible text, and links. |
| `browser_screenshot` | Capture a tab screenshot and emit a TUI preview. |
| `browser_eval` | Evaluate JavaScript for automation or DOM extraction. |

Browser tools emit a `BrowserPreview` event back to the calling session, so the
TUI thumbnail updates for MCP-capable harnesses the same way it does for
smith-native browser calls.

## Memory and session context

`agentd_context` is the high-level way for an agent to load its current context:
global/project memory, memory file paths, session widget paths, widget policy,
supported widget Markdown extensions, and memory maintenance policy.

The environment variables below are the low-level view of that same context.
They are passed to child agents so tools know which daemon and session they
belong to.

| Variable | Purpose |
|---|---|
| `CONSTRUCT_SESSION_ID` | Identifies the calling session, so tools can avoid acting on themselves. |
| `CONSTRUCT_RUNTIME_DIR` / `CONSTRUCT_STATE_DIR` / `CONSTRUCT_DATA_DIR` / `CONSTRUCT_CONFIG_DIR` | Point tools at the same daemon and storage layout as the parent session. |
| `CONSTRUCT_GLOBAL_MEMORY_FILE` / `CONSTRUCT_PROJECT_MEMORY_FILE` / `CONSTRUCT_PROJECT_ID` | Point `agentd_context` at the Markdown memory files for the session. |
| `CONSTRUCT_SESSION_WIDGETS_DIR` | Points agents at the current session's file-backed widget directory. Prefer reading it from `agentd_context` so the agent also sees widget policy and supported Markdown extensions. |

## Generative widgets

Agents can create session-scoped UI widgets by writing Markdown files into the
`session_widgets.dir` returned by `agentd_context`. The same directory is also
available as `CONSTRUCT_SESSION_WIDGETS_DIR`, but `agentd_context` is preferred
because it includes widget policy and supported Markdown extensions. See
[Generative widgets](generative-widgets.md) for the file format, lifecycle,
rendering behavior, and action-link semantics.
