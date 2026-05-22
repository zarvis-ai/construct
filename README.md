# agentd

A terminal **agent fleet** — run and supervise multiple coding-agent sessions across heterogeneous harnesses (Claude Code, Codex, generic shell, ...) from one TUI.

Status: **early — M2 (PTY mode) just landed. Wire protocols may still break.**

```
┌─ sessions ────────────────┬─ session: s4f3...  shell  running ─────┐
│ ● s4f3a...  shell   echo… │  [12:04:11] status running              │
│ ◐ sa3944... shell   while │  [12:04:11]  agent: hello-from-shell    │
│ ✓ sc4d20... shell   echo… │  [12:04:11]  agent: and-another-line    │
│ ✗ s78b1... claude   migr… │  [12:04:11] ▢ done (exit 0)             │
│                           │                                          │
├───────────────────────────┴──────────────────────────────────────────┤
│ M-x send-input ▸ confirm yes_                                        │
├──────────────────────────────────────────────────────────────────────┤
│ agentd  [emacs]  sc4d20bd24  done  -    ? for help                   │
└──────────────────────────────────────────────────────────────────────┘
```

## Architecture

Five layers, each replaceable:

```
┌──────────────────────────────────────────────┐
│ TUI shell (rendering, layout, keymap)        │  emacs default; vim profile
├──────────────────────────────────────────────┤
│ Command + keybinding kernel                  │  every action is a command
├──────────────────────────────────────────────┤
│ Session manager (state, events, broadcast)   │  daemon-side
├──────────────────────────────────────────────┤
│ Agent Harness Protocol (AHP) — JSON-RPC      │  stable wire contract
├──────────────────────────────────────────────┤
│ Harness adapters (separate processes)        │  plugin boundary
│ shell   claude   codex   <your-harness>      │
└──────────────────────────────────────────────┘
```

- **Daemon** (`agentd`) owns sessions, spawns adapters, persists transcripts. Speaks JSON-RPC over a Unix socket to clients.
- **Client** (`agent`) is the TUI plus a set of one-shot subcommands. Multiple clients can attach concurrently.
- **Adapter** binaries are independent processes. They implement the AHP over stdio. Anyone can ship one in any language.

## Crates

| Crate | Binary | Purpose |
|---|---|---|
| `crates/protocol` | — (lib) | AHP + IPC types, transport, adapter SDK |
| `crates/daemon` | `agentd` | Session supervisor, IPC server |
| `crates/cli` | `agent` | TUI client + control subcommands |
| `crates/adapter-shell` | `agentd-adapter-shell` | Generic shell command runner |
| `crates/adapter-claude` | `agentd-adapter-claude` | Wraps the `claude` CLI |
| `crates/adapter-codex` | `agentd-adapter-codex` | Wraps the `codex` CLI |
| `crates/adapter-zarvis` | `agentd-adapter-zarvis` | Built-in multi-provider agent (OpenAI / Anthropic / Ollama) |

## Quick start

```sh
cargo build --workspace --release

# Terminal 1: daemon (foreground)
./target/release/agentd run

# Terminal 2: control
./target/release/agent harnesses
./target/release/agent new shell "echo hello"
./target/release/agent list
./target/release/agent          # launches TUI
```

Smoke test:

```sh
cargo build --workspace
scripts/smoke.sh
```

## Contributor workflow

Contributor workflow notes, including PR review artifact guidance and TUI
recording instructions, live in [`AGENTS.md`](./AGENTS.md).

## Paths

`agentd` reads/writes under XDG-style directories, with `AGENTD_*_DIR` overrides:

| Use | Default | Override |
|---|---|---|
| Config | `~/.config/agentd` | `AGENTD_CONFIG_DIR` |
| State (pid/log) | `~/.local/state/agentd` | `AGENTD_STATE_DIR` |
| Data (sessions) | `~/.local/share/agentd` | `AGENTD_DATA_DIR` |
| Socket | `$XDG_RUNTIME_DIR/agentd/agentd.sock` (falls back to state) | `AGENTD_RUNTIME_DIR` |

`agentd paths` prints the resolved layout.

## TUI Theme

The TUI uses a built-in Matrix theme by default. Override any color slot in
`$AGENTD_CONFIG_DIR/theme.toml` (default `~/.config/agentd/theme.toml`):

```toml
[colors]
text = "#b8ffcc"
accent = "#39ff88"
border_focused = "#4bff82"
harness = "#96ffaa"
danger = "red"
matrix_dim = "indexed:34"
```

Colors accept `#rrggbb`, `indexed:N`, or ANSI names such as `green`, `cyan`,
`dark_gray`, and `light_yellow`. Omitted slots keep the Matrix default.

## Adapter protocol (AHP)

The daemon spawns one adapter process per session and speaks JSON-RPC 2.0 over the adapter's stdin/stdout, one message per line.

Methods the adapter implements:

| Method | Payload |
|---|---|
| `initialize` | `{protocol_version, client_info}` → `InitializeResult` |
| `session.start` | `{session_id, cwd, prompt?, model?, mode?, pty_size?, env, args}` |
| `session.input` | `{session_id, text}` — line-oriented input |
| `session.pty_input` | `{session_id, data}` — base64 raw bytes for the PTY master |
| `session.pty_resize` | `{session_id, cols, rows}` — SIGWINCH equivalent |
| `session.interrupt` | `{session_id}` |
| `session.stop` | `{session_id}` |
| `shutdown` | `{}` |

Notifications the adapter emits:

- `session/event` — one `SessionEvent`. `Pty {data}` (base64 bytes) is the
  hot path for PTY-backed sessions; structured variants (`Message`,
  `ToolUse`, `ToolResult`, `Cost`, `Diff`, `Status`, `Done`, ...) are emitted
  alongside when the adapter has them.
- `log` — free-form line for the daemon's log.

Adapters that own a PTY can opt into a shared runtime helper:

```rust
use agentd_protocol::adapter::pty::{run_session, PtySpec};

// in your run(metadata, |params, ctx| async move { ... }) closure:
let spec = PtySpec {
    bin: "bash".into(),
    args: vec!["-il".into()],
    cwd: params.cwd.into(),
    env: params.env.into_iter().collect(),
    size: params.pty_size.unwrap_or(PtySize { cols: 100, rows: 30 }),
    status_detail: Some("bash -il".into()),
};
let _ = run_session(spec, ctx).await;
```

(Enable the `pty` feature on `agentd-protocol` to pull in `portable-pty`.)

Writing an adapter in Rust is roughly:

```rust
use agentd_protocol::adapter::run;
use agentd_protocol::{Capabilities, InitializeResult, MessageRole, SessionEvent, SessionState};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let metadata = InitializeResult {
        name: "demo".into(),
        version: env!("CARGO_PKG_VERSION").into(),
        capabilities: Capabilities { supports_input: true, ..Default::default() },
    };
    run(metadata, |params, mut ctx| async move {
        ctx.emit.emit(SessionEvent::Status { state: SessionState::Running, detail: None });
        ctx.emit.emit(SessionEvent::Message {
            role: MessageRole::Assistant,
            text: format!("got prompt: {:?}", params.prompt),
        });
        ctx.emit.emit(SessionEvent::Done { exit_code: 0 });
    }).await
}
```

Adapters in other languages just need to speak the same JSON shapes.

## Milestone 1 scope

Implemented:

- [x] Session lifecycle (create, list, get, send input, interrupt, stop, kill)
- [x] Multi-harness adapters: `shell`, `claude`, `codex`
- [x] **Multi-turn** for `claude` (via `--resume <session_id>`) and `codex`
      (per-turn re-spawn; opt-in resume via `AGENTD_CODEX_RESUME_FLAG`)
- [x] Live transcript view (streaming, structural rendering)
- [x] Session list with status glyphs
- [x] Send input to selected session; mid-turn inputs queue for the next turn
- [x] Diff panel (uses `git diff` against the session cwd / worktree)
- [x] Git worktree isolation (`--worktree`)
- [x] Command palette (`M-x`)
- [x] emacs + vim keymap profiles
- [x] Config file (`~/.config/agentd/config.toml`)
- [x] Daemon + client process split (Unix socket)

### Per-adapter modes

Each session has a **mode**: `interactive` (PTY-attached, default when the
TUI is creating sessions) or `headless` (structured stream, default for
non-PTY-aware clients). Pick explicitly with `agent new ... --mode <m>` or
the per-adapter env var (`AGENTD_CLAUDE_MODE`, `AGENTD_CODEX_MODE`).

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
- **`zarvis`** — agentd's built-in agent. Talks to model APIs directly,
  no vendor CLI needed. See the [zarvis section](#zarvis-built-in-agent) below.
  - *interactive (default in the TUI)*: chat-style REPL synthesized
    into the session's PTY pane — colored prompt, streaming assistant
    text, inline tool blocks, inline approval prompts (`y`/`n`/`a`).
  - *headless (default for non-PTY clients)*: structured event stream
    (`Message` / `ToolUse` / `ToolResult` / `Cost`). Approvals come
    from the TUI minibuffer / `agent` IPC.
  - Override with `--mode interactive|headless` or
    `AGENTD_ZARVIS_MODE`.

## zarvis (built-in agent)

`zarvis` is a self-contained headless agent that ships with agentd. It
talks to OpenAI, Anthropic, or a local Ollama directly and runs its own
agent loop with shell + filesystem + agentd-control tools. No external
CLI install required.

### Quick start

```sh
# Pick a provider — only one of these needs to be set:
export ANTHROPIC_API_KEY=sk-ant-...
# or  export OPENAI_API_KEY=sk-...
# or  run a local ollama (default http://localhost:11434)

agent new zarvis "list the rust files in this repo and summarize what each crate does"
```

### Model selection

Pass `--model <spec>` on `agent new` (or set `AGENTD_ZARVIS_MODEL`).
The spec is one of:

- `openai:<name>` — e.g. `openai:gpt-5-mini`
- `anthropic:<name>` — e.g. `anthropic:claude-haiku-4-5`
- `ollama:<name>` — e.g. `ollama:llama3.1`

Bare names auto-detect: `gpt-*` / `o[1-5]*` → OpenAI, `claude-*` →
Anthropic, anything else → Ollama. When in doubt, use the explicit
prefix.

If you don't pass a model and `AGENTD_ZARVIS_MODEL` isn't set, zarvis
picks: `ANTHROPIC_API_KEY` → `claude-haiku-4-5`, else `OPENAI_API_KEY`
→ `gpt-5-mini`, else `ollama:llama3.1`. The initial Status event
records the chosen `provider:model` so you can verify.

### Tools

Local: `shell`, `read_file`, `write_file`, `edit_file` (search/replace
with required uniqueness), `list_dir`, `find_files`.

Agentd-control (16 tools, same surface as `agentd-mcp`):
`agentd_list_sessions`, `agentd_create_session`, `agentd_send_input`,
`agentd_get_output`, `agentd_get_diff`, `agentd_pin_session`,
`agentd_rename_session`, … — full read + write access to other
sessions on the same daemon. `agentd_whoami` returns the session id
this zarvis is running inside (auto-injected via env).

### Approval / automode

Tool calls run with your permissions, so zarvis classifies each tool
as **Safe** (read-only — `read_file`, `list_dir`, `find_files`, all
`agentd_get_*`/`agentd_list_*`) or **Risky** (mutates fs/sessions —
everything else, including `shell`).

- **automode off (default)**: Safe runs silently; Risky pauses with a
  minibuffer prompt showing the tool + arg summary + risk badge.
- **automode on**: all tools run silently. Modeline shows
  `[automode]`.

Approval prompt keys: `y`/Enter approve, `n`/Esc deny, `a` approve **and
flip automode on for this session**. Toggle automode anytime with
`C-x A` (emacs) / `A` (vim). Denied calls return a synthetic "user
denied" result to the model so it can pivot rather than crash.

Override the initial state with `AGENTD_ZARVIS_AUTOMODE=1` (useful for
scripted/batch runs).

### Long output handling

The full tool output goes to the transcript (you see everything). The
agent's context only gets a truncated head + `[N bytes elided]` + tail
(8 KiB budget per call), so a `find /` doesn't blow the context
window.

Context budget is also pruned automatically: estimated tokens past 70%
of the model's window drops the oldest turn pair, always keeping the
two most-recent.

### Opt-out / customization

- `AGENTD_ZARVIS_AUTOMODE=1` — start with automode on.
- `AGENTD_ZARVIS_MODEL=<spec>` — default model when `--model` is
  omitted.
- `OPENAI_BASE_URL` / `ANTHROPIC_BASE_URL` / `OLLAMA_HOST` — point at
  alternate endpoints (OpenAI-compatible vendors, self-hosted, etc.).

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

Sessions whose adapter binary is missing, whose start params can't
be loaded, or whose harness rejects the respawn are marked `Errored`
(transcript + scrollback remain readable).

Deferred to later milestones:

- Custom user keymap file (today: choose `AGENTD_KEYMAP=emacs|vim`)
- Cost/token dashboards across sessions
- Notifications (desktop / Slack)
- Web UI on the same IPC

## Agent-controlled agentd (MCP)

An agent (claude / codex) running inside an agentd session can drive the
daemon itself — list other sessions, read their PTY output, send input,
spawn helper sessions, etc. — via an MCP stdio server, `agentd-mcp`.

When the claude / codex adapter spawns the child CLI in interactive mode,
it automatically:
- Writes a per-session MCP config under `$STATE_DIR/mcp/<session_id>.json`
- Passes `--mcp-config <path>` to the child
- Sets `AGENTD_SESSION_ID=<session_id>` in the child's environment

The MCP server exposes a read + write tool surface that mirrors the IPC:
`agentd_whoami`, `agentd_list_sessions`, `agentd_get_session`,
`agentd_get_transcript`, `agentd_get_output`, `agentd_get_diff`,
`agentd_list_harnesses`, `agentd_create_session`, `agentd_send_input`,
`agentd_send_keys` (raw PTY bytes), `agentd_interrupt_session`,
`agentd_stop_session`, `agentd_kill_session`, `agentd_delete_session`,
`agentd_pin_session`, `agentd_rename_session`.

Opt out with `AGENTD_INJECT_MCP=0` in the daemon's environment.

## License

MIT — see [LICENSE](LICENSE).
