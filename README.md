# construct

**Command a fleet of agents, designed for hackers cracking the matrix.**

Create Codex, Claude Code, Antigravity, and smith sessions all in one place. Or
let your agent coordinate them in a terminal crafted for hardcore hackers like
you. Remote control from your phone when you're in motion.

![construct TUI demo](https://raw.githubusercontent.com/zarvis-ai/agentd/73525a653d1969474f02f0ac699867a68565ac99/demos/browser-thumbnail.gif)

## Why construct?

- **One cockpit for every agent** — attach to Claude Code, Codex, Antigravity,
  smith, or a shell process from one focused workspace that rewards attention.

  <img src="https://raw.githubusercontent.com/zarvis-ai/agentd/f8fae6e5227ccd0b2140c35ce6e2ad16349da848/demos/new-session.gif" alt="construct new session demo" width="50%">
- **A delightful way to manage multiple Claude Code and Codex sessions** —
  switch sessions instantly, pin multiple sessions to monitor, or let an agent
  observe all your sessions across different harnesses.
- **Agent-to-agent orchestration** — MCP tools let an agent list sessions, read
  output, spawn helpers, send input, inspect diffs, and drive Chrome.
- **Generative widgets** — construct generates and updates widgets for your task,
  so you can track progress, review outputs, and take action without leaving
  the TUI or web client.

  <img src="https://raw.githubusercontent.com/zarvis-ai/agentd/0b9df04fb1fb40b2cea5f7e42b2e249a649b0ec2/demos/generative-widgets.gif" alt="construct generative widgets demo" width="50%">
- **[Remote control](docs/remote-control.md) when you step away** — `/remote-control`
  opens a browser-accessible web client with a QR code. Connect from your phone,
  no service signup, no setup required.

  <img src="https://raw.githubusercontent.com/zarvis-ai/agentd/31239874073db9fee79d78eb98ea1e7f434d051b/demos/remote-control.gif" alt="construct remote control demo" width="50%" align="middle"> &nbsp;&nbsp;&nbsp;&nbsp;→&nbsp;&nbsp;&nbsp;&nbsp; <img src="https://raw.githubusercontent.com/zarvis-ai/agentd/91d88b514b602fb313aa82b9783d68b8ca1ab5a9/demos/webui-phone.jpg" alt="construct web client on a phone" width="180" align="middle">
- **Extensible harness protocol** — adapters are separate processes speaking
  JSON-RPC over stdio, so new tools can plug in without changing the daemon.

## Getting started

### 1. Requirements

Bring the agents you want to run. `construct` wraps the CLIs already on your
machine, so install whichever harnesses you use, keep them on `PATH`, and log in
first:

- **Codex** — install the `codex` CLI and complete its OAuth login.
- **Claude Code** — install the `claude` CLI and complete its OAuth login.
- **Antigravity** — install the `agy` CLI and complete its OAuth login.
- **smith** — built in to construct; no separate CLI to install. Talks to OpenAI,
  Anthropic, or Google Gemini via API key, a local Ollama, or a ChatGPT
  subscription via Codex OAuth.

Once those CLIs are available and authenticated, `construct` can create and resume
their sessions from the fleet TUI.

### 2. Install

The installer downloads the right prebuilt binary for your platform, verifies its
SHA-256 checksum, and drops every binary into one directory on your PATH:

```sh
curl -fsSL https://raw.githubusercontent.com/zarvis-ai/agentd/main/install.sh | sh
```

Pin a version or change the directory with `CONSTRUCT_VERSION=v0.2.0` /
`CONSTRUCT_BIN_DIR=/usr/local/bin`.

### 3. Start the daemon

```sh
construct daemon run
```

Leave this running. It owns sessions, persists state, and exposes the local IPC
socket used by clients. (`constructd` is a back-compat alias for the same
daemon — `constructd run` and `construct daemon run` are equivalent.)

### 4. Open the fleet TUI

In a second shell:

```sh
construct
```

Use `?` for help and `M-x` for the command palette. From the TUI you can create
sessions, switch between agents, send input, inspect diffs, and interrupt or stop
work without leaving the flow.

### 5. Start crack the matrix

Happy hacking. Chase the dream idea from your terminal: ask Codex, Claude Code,
Antigravity, and [smith](docs/smith.md) to dive into the hard parts, then keep
steering from your phone when you're in motion.

## Upgrading

```sh
construct upgrade            # install the latest release (atomic in-place replace)
construct upgrade --check    # just compare your version against the latest
construct upgrade --restart  # upgrade, then restart a running daemon to apply
```

`construct upgrade` re-runs the installer for you (pin a release with
`--version vX.Y.Z`); re-running the install one-liner does the same thing. A
running daemon keeps the old code until it restarts — pass `--restart`, or run
`/construct restart` in the TUI, to pick up the upgrade without losing sessions.
The TUI also surfaces a one-line notice when a newer release is available
(disable with `CONSTRUCT_NO_UPDATE_CHECK=1`).

## Building from source

```sh
git clone https://github.com/zarvis-ai/agentd.git
cd agentd
cargo build --workspace
```

Debug binaries land in `target/debug/`:

- `target/debug/construct` — TUI, control CLI, **and the daemon** (`construct daemon run`)
- `target/debug/constructd` — standalone daemon binary; a thin back-compat alias for `construct daemon run`
- `target/debug/construct-mcp` — MCP bridge for agents
- `target/debug/construct-adapter-*` — harness adapters

For an optimized build, use `cargo build --workspace --release` and replace
`target/debug` with `target/release`.

## Documentation

- [Architecture](docs/architecture.md) — daemon/client split, crates, and the
  Agent Harness Protocol (AHP).
- [Harnesses and session modes](docs/harnesses.md) — supported adapters,
  interactive vs. headless modes, worktree isolation, and resume behavior.
- [smith built-in agent](docs/smith.md) — providers, model selection, tools,
  approvals, automode, and hooks.
- [Unified tool layer](docs/unified-tool-layer.md) — MCP servers and shared tools for
  fleet control, browser automation, and agent coordination.
- [Generative widgets](docs/generative-widgets.md) — agent-generated Markdown UI
  for compact session-scoped task state, timelines, and action links.
- [Memory](docs/memory.md) — durable Markdown context for project workflows,
  decisions, preferences, and pitfalls.
- [Configuration](docs/configuration.md) — XDG paths, `CONSTRUCT_*` overrides, and
  TUI theme customization.
- [Remote control](docs/remote-control.md) — phone/browser access, QR setup,
  credentials, and local debug mode.

## License

MIT — see [LICENSE](LICENSE).
