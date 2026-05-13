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

## Paths

`agentd` reads/writes under XDG-style directories, with `AGENTD_*_DIR` overrides:

| Use | Default | Override |
|---|---|---|
| Config | `~/.config/agentd` | `AGENTD_CONFIG_DIR` |
| State (pid/log) | `~/.local/state/agentd` | `AGENTD_STATE_DIR` |
| Data (sessions) | `~/.local/share/agentd` | `AGENTD_DATA_DIR` |
| Socket | `$XDG_RUNTIME_DIR/agentd/agentd.sock` (falls back to state) | `AGENTD_RUNTIME_DIR` |

`agentd paths` prints the resolved layout.

## TUI keys (emacs default)

The right pane has two views — **transcript** (structured event log) and
**terminal** (live PTY emulator powered by `vt100`+`tui-term`). Sessions whose
adapter has `supports_pty=true` (shell always, claude/codex in interactive
mode) open in terminal view.

Two focusable panes (matching standard emacs window semantics): the **list**
on the left and the **view** on the right. When the view is focused *and* it's
in terminal mode for a PTY-backed session, keystrokes go to the child by
default — `C-x` is the escape prefix back to agentd commands.

| Key | Action |
|---|---|
| `C-x o` / `Tab` | switch focus (list ↔ view) — `other-window` |
| `C-x t` | toggle transcript ↔ terminal view |
| `C-n` / `↓` | next session |
| `C-p` / `↑` | prev session |
| `C-x C-f` | new session (wizard) |
| `C-x i` | send input to selected session |
| `C-x k` | delete selected session (confirms; kills if running, drops transcript + worktree) |
| `C-x d` | show diff |
| `C-x r` | rename selected session (sets the user-facing title; submit empty to clear back to the hash) |
| `C-c C-c` | interrupt |
| `M-x` / `C-x x` | command palette (the `C-x x` alias is Meta-free, useful on macOS Terminal.app without "Use Option as Meta") |
| `C-v` / `M-v` | scroll page down / up |
| `g g` / `G` | scroll top / bottom |
| `?` | toggle help |
| `C-x C-c` / `q` | quit |
| `Space` / `C-x p` | toggle pin on selected session (live tail tile in the pin strip below the main view) |
| `C-x C-p` / `C-x C-n` | reorder: move selected session up / down in the list (Meta-free; works in every terminal) |
| `Shift-↑` / `Shift-↓` | same, in terminals that forward Shift with arrows (iTerm2, WezTerm, Alacritty, Kitty — **not** macOS Terminal.app default) |

**In PTY-captured mode** (view focused on a PTY session), all keys pass
through to the child *except* `C-x`, which starts a chord. So everything in
the table above that begins with `C-x` works without changing focus —
`C-x o` to jump to the list, `C-x C-f` to start a new session, `C-x C-c` to
quit, etc. Other commands like `M-x` need focus on the list first.

Set `AGENTD_KEYMAP=vim` for the vim profile.

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

Deferred to later milestones:

- Custom user keymap file (today: choose `AGENTD_KEYMAP=emacs|vim`)
- Cost/token dashboards across sessions
- Notifications (desktop / Slack)
- Web UI on the same IPC

## License

MIT — see [LICENSE](LICENSE).
