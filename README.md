# agentd

**Command a fleet of agents, designed for hackers.**

Create Codex, Claude Code, Antigravity, and Zarvis sessions all in one place. Or
let your agent coordinate them in a terminal crafted for hardcore hackers like
you. Remote control from your phone when you're in motion.

![agentd TUI demo](https://raw.githubusercontent.com/zarvis-ai/agentd/73525a653d1969474f02f0ac699867a68565ac99/demos/browser-thumbnail.gif)

## Why agentd?

- **One cockpit for every agent** — attach to Claude Code, Codex, Antigravity,
  Zarvis, or a shell process from one focused workspace that rewards attention.
- **Parallel work without losing control** — spawn helper sessions, pin important
  work, interrupt stuck runs, inspect diffs, and send follow-up input mid-turn.
- **Agent-to-agent orchestration** — MCP tools let an agent list sessions, read
  output, spawn helpers, send input, inspect diffs, and drive Chrome.
- **[Remote control](docs/remote-control.md) when you step away** — `/remote-control`
  opens a browser-accessible web client with a QR code. Connect from your phone,
  no service signup, no setup required.
- **Extensible harness protocol** — adapters are separate processes speaking
  JSON-RPC over stdio, so new tools can plug in without changing the daemon.

## Getting started

### 1. Install

The installer downloads the right prebuilt binary for your platform, verifies its
SHA-256 checksum, and drops every binary into one directory on your PATH:

```sh
curl -fsSL https://raw.githubusercontent.com/zarvis-ai/agentd/main/install.sh | sh
```

Pin a version or change the directory with `AGENTD_VERSION=v0.2.0` /
`AGENTD_BIN_DIR=/usr/local/bin`.

### 2. Start the daemon

```sh
agentd
```

Leave this running. It owns sessions, persists state, and exposes the local IPC
socket used by clients.

### 3. Open the fleet TUI

In a second shell:

```sh
agent
```

Use `?` for help and `M-x` for the command palette. From the TUI you can create
sessions, switch between agents, send input, inspect diffs, and interrupt or stop
work without leaving the flow.

### 4. Start your agent

Happy hacking. Chase the dream idea from your terminal: ask Codex, Claude Code,
Antigravity, and [Zarvis](docs/zarvis.md) to dive into the hard parts, then keep
steering from your phone when you're in motion.

## Upgrading

```sh
agent upgrade            # install the latest release (atomic in-place replace)
agent upgrade --check    # just compare your version against the latest
agent upgrade --restart  # upgrade, then restart a running daemon to apply
```

`agent upgrade` re-runs the installer for you (pin a release with
`--version vX.Y.Z`); re-running the install one-liner does the same thing. A
running daemon keeps the old code until it restarts — pass `--restart`, or run
`/agentd restart` in the TUI, to pick up the upgrade without losing sessions.
The TUI also surfaces a one-line notice when a newer release is available
(disable with `AGENTD_NO_UPDATE_CHECK=1`).

## Building from source

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
`target/debug` with `target/release`.

## Documentation

- [Architecture](docs/architecture.md) — daemon/client split, crates, and the
  Agent Harness Protocol (AHP).
- [Harnesses and session modes](docs/harnesses.md) — supported adapters,
  interactive vs. headless modes, worktree isolation, and resume behavior.
- [Zarvis built-in agent](docs/zarvis.md) — providers, model selection, tools,
  approvals, automode, and hooks.
- [Unified tool layer](docs/unified-tool-layer.md) — MCP servers and shared tools for
  fleet control, browser automation, and agent coordination.
- [Configuration](docs/configuration.md) — XDG paths, `AGENTD_*` overrides, and
  TUI theme customization.
- [Remote control](docs/remote-control.md) — phone/browser access, QR setup,
  credentials, and local debug mode.
- [Releasing](docs/RELEASING.md) — how the prebuilt binaries are built and
  published, and how to cut a versioned release.
- [Contributor workflow](AGENTS.md) — PR workflow, build expectations, and TUI
  recording guidance.

## License

MIT — see [LICENSE](LICENSE).
