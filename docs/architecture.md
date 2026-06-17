# Architecture


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

- **Daemon** owns sessions, spawns adapters, persists transcripts. Speaks JSON-RPC over a Unix socket to clients. Run it with `construct daemon run` (the TUI also auto-starts one when none is running).
- **Client** (`construct`) is the TUI plus a set of one-shot subcommands and protocol-facing entrypoints such as `construct acp`. Multiple clients can attach concurrently.
- **Adapter** binaries are independent processes. They implement the AHP over stdio. Anyone can ship one in any language.

The daemon and client ship as **one binary**: `construct` runs the TUI by default, the daemon under `construct daemon`, and the Agent Client Protocol stdio bridge under `construct acp`. The daemon's runtime lives in the `agentd` library crate; there is no standalone daemon binary. The daemon and client are not merged into one *process* — the daemon stays a separate long-lived process that many clients attach to — only into one shipped executable. See [`specs/0026-single-binary-daemon-and-client.md`](../specs/0026-single-binary-daemon-and-client.md).

## Crates

| Crate | Binary | Purpose |
|---|---|---|
| `crates/protocol` | — (lib) | AHP + IPC types, transport, adapter SDK |
| `crates/daemon` | `agentd` (lib only) | Session supervisor + IPC server runtime. No standalone binary — driven by `construct daemon` |
| `crates/cli` | `construct` | TUI client + control subcommands + `construct daemon` (runs the daemon via the `agentd` lib) + `construct acp` |
| `crates/adapter-shell` | `construct-adapter-shell` | Generic shell command runner |
| `crates/adapter-claude` | `construct-adapter-claude` | Wraps the `claude` CLI |
| `crates/adapter-codex` | `construct-adapter-codex` | Wraps the `codex` CLI |
| `crates/adapter-smith` | `construct-adapter-smith` | Built-in multi-provider agent (OpenAI / Anthropic / Gemini / Ollama) |

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
