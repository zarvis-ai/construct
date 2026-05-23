# Harnesses and session modes

## Per-adapter modes

Each session has a **mode**: `interactive` (PTY-attached, default when the
TUI is creating sessions) or `headless` (structured stream, default for
non-PTY-aware clients). Pick explicitly with `agent new ... --mode <m>` or
the per-adapter env var (`AGENTD_CLAUDE_MODE`, `AGENTD_CODEX_MODE`, `AGENTD_ANTIGRAVITY_MODE`, `AGENTD_ZARVIS_MODE`).

- **`shell`** — always PTY. Empty prompt → `$SHELL -il` (interactive login
  shell). Non-empty prompt → `$SHELL -lc <prompt>` (one-shot).
- **`claude`** —
  - *interactive*: spawns `claude` (no `-p`) under a PTY → full Claude TUI in
    the right pane (`/resume`, slash commands, all of it).
  - *headless*: per-turn `claude -p --input-format stream-json --output-format
    stream-json --verbose` with `--resume <session_id>` for follow-ups. Emits
    structured `Message`/`ToolUse`/`Cost` events.
- **`codex`** —
  - *interactive*: spawns `codex` under a PTY.
  - *headless*: per-turn `codex exec`. Set `AGENTD_CODEX_RESUME_FLAG`
    (e.g. `--session-id`) if your codex build supports cross-turn resumption.
- **`antigravity`** —
  - *interactive*: spawns `agy` under a PTY.
  - *headless*: uses the Antigravity CLI transcript stream where available.
- **`zarvis`** — agentd's built-in agent. Talks to model APIs directly,
  no vendor CLI needed. See [Zarvis built-in agent](zarvis.md).
  - *interactive (default in the TUI)*: chat-style REPL synthesized
    into the session's PTY pane — colored prompt, streaming assistant
    text, inline tool blocks, inline approval prompts (`y`/`n`/`a`).
  - *headless (default for non-PTY clients)*: structured event stream
    (`Message` / `ToolUse` / `ToolResult` / `Cost`). Approvals come
    from the TUI minibuffer / `agent` IPC.
  - Override with `--mode interactive|headless` or
    `AGENTD_ZARVIS_MODE`.

## Session resume across daemon restarts

When the daemon restarts, sessions that were alive at the time of the
last shutdown are automatically re-spawned. The daemon persists the
original `SessionStartParams` to disk at create time and sets
`AGENTD_RESUME=1` + `AGENTD_SESSION_DATA_DIR=<session-dir>` in the
adapter's env on re-spawn; each adapter decides what "resume" means
for its harness:

- **shell** — respawns a fresh `$SHELL -il` in the original cwd. The
  PTY scrollback from the previous incarnation is still in
  `pty.log` (visible in the terminal pane), but any in-progress
  command is gone.
- **claude (interactive)** — mints a fresh UUID at first spawn,
  passes it as `--session-id <uuid>` to claude, and persists it under
  `<session-dir>/claude_session_id.txt`. On respawn we pass
  `--resume <uuid>` so the claude conversation continues.
- **codex (interactive)** — codex doesn't let the client assign a
  session id, so we tag the spawn with a unique `originator` via
  codex's internal env override
  (`CODEX_INTERNAL_ORIGINATOR_OVERRIDE=agentd:<session-id>`) and watch
  the codex sessions dir (`$CODEX_HOME/sessions` or
  `~/.codex/sessions`) for the rollout that bears that tag. When we
  find it, we read codex's UUID from `payload.id` and write it to
  `<session-dir>/codex_session_id.txt`. On respawn we run
  `codex resume <uuid>` to reattach. `AGENTD_CODEX_RESUME_ID`
  overrides the captured id. The watcher polls for the session's full
  lifetime — codex flushes its rollout lazily (sometimes only after
  the first turn completes), so a short timeout would just miss it.
  If no id has been captured yet when the daemon restarts, the
  respawn starts a *fresh* codex rather than `codex resume --last`
  — `--last` resolves globally across every codex session on the
  machine, so using it as a fallback conflates multiple agentd codex
  sessions into the same upstream conversation.

  Known limitation: using codex's own `/resume` slash command
  inside an agentd-managed codex session won't survive a daemon
  restart. Codex appends the resumed conversation to the original
  rollout file and leaves its `originator` unchanged, so our
  originator-tagged watcher can't detect the switch and we keep
  pointing at the *first* UUID we captured. On daemon restart you'll
  reattach to the original conversation, not the one you `/resume`d
  to. Create a separate agentd session instead if you want to work
  on a different codex conversation.
- **zarvis** — appends each `Message` to
  `<session-dir>/zarvis.jsonl` as the agent loop runs. On respawn
  the loop reads the file back into memory before waiting for new
  input, so the conversation history is intact across daemon
  restarts.

  Zarvis also has opt-in command hooks, configured explicitly with
  `AGENTD_ZARVIS_HOOKS_CONFIG=/path/to/hooks.json` or inline via
  `AGENTD_ZARVIS_HOOKS_JSON`. Hooks are not auto-loaded from a
  repository checkout because they execute local commands with the
  user's permissions. Config shape:

  ```json
  {
    "hooks": {
      "session_start": [{ "command": "jq . >> /tmp/zarvis-hooks.log" }],
      "user_prompt_mutate": [{ "command": "jq '.prompt += \"\\n\\nRemember to cite files.\"'" }],
      "user_prompt_submit": [{ "command": "jq .prompt >> /tmp/zarvis-prompts.log" }],
      "pre_tool_use_mutate": [{ "command": "jq .", "timeout_ms": 5000 }],
      "pre_tool_use": [{ "command": "jq . >> /tmp/zarvis-tools.log", "timeout_ms": 5000 }],
      "post_tool_use": [{ "command": "jq . >> /tmp/zarvis-tools.log" }],
      "tool_approval_request": [{ "command": "jq . >> /tmp/zarvis-approvals.log" }],
      "session_stop": [{ "command": "jq . >> /tmp/zarvis-hooks.log" }]
    }
  }
  ```

  Each hook command runs under `$AGENTD_SHELL_BIN -lc` (default
  `/bin/sh -lc`) in the session cwd, receives one JSON payload on
  stdin, and gets `AGENTD_ZARVIS_HOOK_EVENT` in its environment.
  Notification hooks ignore stdout. Mutating hooks are separate:
  `user_prompt_mutate` can return `{"prompt":"..."}` on stdout before
  the user message is sent to the model, and `pre_tool_use_mutate` can
  return `{"args":{...}}` before the tool is displayed, approved, or
  executed. Mutating hooks run in config order and each returned JSON
  object is merged into the next hook's payload. Hook failures,
  timeouts, invalid JSON, or non-object stdout are logged and the
  current payload continues unchanged.

Sessions whose adapter binary is missing, whose start params can't
be loaded, or whose harness rejects the respawn are marked `Errored`
(transcript + scrollback remain readable).
