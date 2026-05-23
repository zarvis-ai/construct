# agentd

**Command a fleet of agents, designed for hackers.**

Create Codex, Claude Code, Antigravity, and Zarvis sessions all in one place. Or
let your agent coordinate them in a terminal crafted for hardcore hackers like
you. Remote control from your phone when you're in motion.

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

## Why agentd?

Modern development often means asking several agents to investigate, edit, test,
review, or monitor the same project. `agentd` turns that from a pile of orphaned
CLI windows into a managed fleet — fast to read, tactile to drive, and built for
dense, high-signal engineering work:

- **One cockpit for every agent** — attach to Claude Code, Codex, Antigravity,
  Zarvis, or a shell process from one focused workspace that rewards attention.
- **Persistent sessions** — transcripts, PTY scrollback, status, cwd, and resume
  metadata live in the daemon instead of disappearing when a client exits.
- **Parallel work without losing control** — spawn helper sessions, pin important
  work, interrupt stuck runs, inspect diffs, and send follow-up input mid-turn.
- **Native PTY mode** — interactive CLIs keep their real shape inside the right
  pane, including slash commands and upstream TUIs.
- **Agent-to-agent orchestration** — MCP tools let an agent list sessions, read
  output, spawn helpers, send input, inspect diffs, and drive Chrome.
- **Remote control when you step away** — `/remote-control` opens a
  browser-accessible web client with a QR code. Connect from your phone, no
  service signup, no setup required.
- **Extensible harness protocol** — adapters are separate processes speaking
  JSON-RPC over stdio, so new tools can plug in without changing the daemon.

## Getting started

### 1. Build

```sh
git clone https://github.com/zarvis-ai/agentd.git
cd agentd
cargo build --workspace
```

Debug binaries land in `target/debug/`:

- `target/debug/agentd` — daemon / session supervisor
- `target/debug/agent` — TUI and control CLI
- `target/debug/agentd-mcp` — MCP bridge for agents
- `target/debug/agentd-adapter-*` — harness adapters

For an optimized build, use `cargo build --workspace --release` and replace
`target/debug` with `target/release` below.

### 2. Start the daemon

```sh
./target/debug/agentd run
```

Leave this running. It owns sessions, persists state, and exposes the local IPC
socket used by clients.

### 3. Open the fleet TUI

In a second shell:

```sh
./target/debug/agent
```

Use `?` for help and `M-x` for the command palette. From the TUI you can create
sessions, switch between agents, send input, inspect diffs, and interrupt or stop
work without leaving the flow.

### 4. Try the built-in Zarvis agent

Zarvis ships with agentd and talks directly to OpenAI, Anthropic, or local
Ollama — no vendor CLI required.

```sh
# Pick one provider, or run local Ollama at http://localhost:11434
export ANTHROPIC_API_KEY=sk-ant-...
# export OPENAI_API_KEY=sk-...

./target/debug/agent new zarvis "list the Rust crates and explain what each one does"
```

## Documentation

- [Architecture](docs/architecture.md) — daemon/client split, crates, and the
  Agent Harness Protocol (AHP).
- [Harnesses and session modes](docs/harnesses.md) — supported adapters,
  interactive vs. headless modes, worktree isolation, and resume behavior.
- [Zarvis built-in agent](docs/zarvis.md) — providers, model selection, tools,
  approvals, automode, and hooks.
- [Unified tool layer](docs/unified-tool-layer.md) — MCP servers and shared tools for
  fleet control, browser automation, and agent coordination.
- [Configuration](docs/configuration.md) — XDG paths, `AGENTD_*` overrides,
  remote-control setup, and TUI theme customization.
- [Contributor workflow](AGENTS.md) — PR workflow, build expectations, and TUI
  recording guidance.

## License

MIT — see [LICENSE](LICENSE).
