<div align="center">
  <h1>construct</h1>
  <p><strong>A terminal-native agentic development environment.</strong></p>
  <img src="https://raw.githubusercontent.com/construct-worlds/construct/52849f56c902397d6729ec286293064c5b15bcfe/demos/lineage-program-run.gif" alt="construct lineage and program run">
</div>

### Quick start

```sh
curl -fsSL https://raw.githubusercontent.com/construct-worlds/construct/main/install.sh | sh
```

More screenshots and demos: [gallery](gallery.md).

## Why construct?

- **tmux for agent fleets** — manage Codex, Claude Code, OpenCode, Antigravity,
  Grok, and smith sessions from your terminal — or let an agent coordinate them.
  CLI-only; no desktop app to install.
- **Fork / merge** — fork a session when you need a parallel attempt
  (new idea, side quest, or a long shot). Supports cross-harness forks and
  merging results back.

  <img src="https://raw.githubusercontent.com/construct-worlds/construct/9de1982e7ec4ae9ad71c32f3f3e3f2f58fbe93ca/demos/fork-merge.gif" alt="construct fork and merge demo" width="70%">
- **Program** — collaborative, executable Markdown
  ([docs/program.md](docs/program.md)): co-develop workflows, tasks, and ideas
  with the agent, then run them from the same document.

  <img src="https://raw.githubusercontent.com/construct-worlds/construct/90d02bd2c1e6108eaa5c763bde9e2d78f8786691/demos/program.gif" alt="construct program demo" width="70%">
- **Agent-to-agent orchestration** — MCP tools let an agent list sessions, read
  output, spawn helpers, send input, inspect diffs, and drive Chrome.
- **ACP (Agent Client Protocol) server** — point Agent Client Protocol clients at
  `construct acp` to create, load, resume, prompt, cancel, and close construct
  daemon sessions through the same installed binary.
- **Generative widgets** — construct generates and updates widgets for your task,
  so you can track progress, review outputs, and take action without leaving
  the TUI or web client.

  <img src="https://raw.githubusercontent.com/construct-worlds/construct/0b9df04fb1fb40b2cea5f7e42b2e249a649b0ec2/demos/generative-widgets.gif" alt="construct generative widgets demo" width="70%">
- **[Remote control](docs/remote-control.md) when you step away** — `/remote-control`
  opens a browser-accessible web client with a QR code. Connect from your phone,
  no service signup, no setup required.

  <img src="https://raw.githubusercontent.com/construct-worlds/construct/31239874073db9fee79d78eb98ea1e7f434d051b/demos/remote-control.gif" alt="construct remote control demo" width="50%" align="middle"> &nbsp;&nbsp;&nbsp;&nbsp;→&nbsp;&nbsp;&nbsp;&nbsp; <img src="https://raw.githubusercontent.com/construct-worlds/construct/91d88b514b602fb313aa82b9783d68b8ca1ab5a9/demos/webui-phone.jpg" alt="construct web client on a phone" width="180" align="middle">
- **Extensible harness protocol** — adapters are separate processes speaking
  JSON-RPC over stdio, so new tools can plug in without changing the daemon.

## Getting started

### 1. Requirements

Bring the agents you want to run. `construct` wraps the CLIs already on your
machine, so install whichever harnesses you use, keep them on `PATH`, and log in
first:

- **Codex** — install the `codex` CLI and complete its OAuth login.
- **Claude Code** — install the `claude` CLI and complete its OAuth login.
- **OpenCode** — install the `opencode` CLI and authenticate the providers you
  plan to use.
- **Antigravity** — install the `agy` CLI and complete its OAuth login.
- **Grok** — install the `grok` CLI and complete its OAuth login.
- **smith** — built in to construct. Talks to OpenAI, Anthropic, Google Gemini,
  or xAI Grok via API key, a local Ollama, a ChatGPT subscription via Codex
  OAuth, a Claude subscription via the authenticated Claude Code CLI, or a Grok
  subscription via the authenticated Grok CLI.

Once those CLIs are available and authenticated, `construct` can create and resume
their sessions from the fleet TUI.

### 2. Install

The installer downloads the right prebuilt binary for your platform, verifies its
SHA-256 checksum, and drops every binary into one directory on your PATH:

```sh
curl -fsSL https://raw.githubusercontent.com/construct-worlds/construct/main/install.sh | sh
```

Pin a version or change the directory with `CONSTRUCT_VERSION=v0.2.0` /
`CONSTRUCT_BIN_DIR=/usr/local/bin`.

### 3. Open the construct Terminal UI

```sh
construct
```

If no daemon is running yet, `construct` auto-starts one in the background and
attaches — there's no separate daemon step. (Opt out with
`CONSTRUCT_NO_AUTOSTART=1`, e.g. in scripts that manage the daemon themselves.)

Use `?` for help and `M-x` for the command palette. From the TUI you can create
sessions, switch between agents, send input, inspect diffs, and interrupt or stop
work without leaving the flow.

MIDI controllers can drive those same native TUI actions without keyboard
emulation. See [MIDI control surfaces](docs/midi.md) for device discovery and
the `construct midi learn` workflow, including OP–XY setup.

To run the daemon explicitly instead (e.g. on a server, or under a process
supervisor):

```sh
construct daemon run
```

It owns sessions, persists state, and exposes the local IPC socket used by
clients. Lifecycle helpers are also available for background daemons:

```sh
construct daemon start
construct daemon stop             # stops adapters; sessions resume on next start
construct daemon stop --sessions  # explicit spelling of the same session-safe stop
construct daemon restart
construct daemon restart --sessions
```

### 4. Start building

Happy hacking. Chase the idea from your terminal: ask Codex, Claude Code,
OpenCode, Antigravity, Grok, and [smith](docs/smith.md) to dive into the hard
parts, then keep steering from your phone when you're in motion.

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
Interactive client commands also ask whether to upgrade when a newer release is
available; saying yes upgrades in place, restarts a running daemon, and resumes
the original command under the new binary. The TUI still surfaces a one-line
notice from the cached check. Disable both with `CONSTRUCT_NO_UPDATE_CHECK=1`.

## ACP (Agent Client Protocol) server

`construct acp` runs an Agent Client Protocol stdio server. Configure ACP
clients to launch this command:

```sh
construct acp
```

It auto-starts the daemon if needed, then maps ACP session lifecycle calls onto
construct daemon sessions. Use `--harness`, `--model`, or `--cwd` to set
defaults for `session/new` requests that omit those fields.

## Building from source

```sh
git clone https://github.com/construct-worlds/construct.git
cd construct
cargo build --workspace
```

Debug binaries land in `target/debug/`:

- `target/debug/construct` — TUI, control CLI, **the daemon**
  (`construct daemon run`), ACP stdio server (`construct acp`),
  MCP bridge (`construct __mcp`, internal), and all harness adapters
  (`construct __adapter <name>`, internal)

For an optimized build, use `cargo build --workspace --release` and replace
`target/debug` with `target/release`.

## Documentation

- [Gallery](gallery.md) — screenshots and demo clips of the TUI and web client.
- [Architecture](docs/architecture.md) — daemon/client split, crates, and the
  Agent Harness Protocol (AHP).
- [Harnesses and session modes](docs/harnesses.md) — supported adapters,
  interactive vs. headless modes, worktree isolation, and resume behavior.
- [Program](docs/program.md) — the per-session Markdown document you and the
  agent co-edit and run: smart clips, run shimmer, templates, and live
  collaboration.
- [Program selection verbs](docs/program-verbs.md) — typed refinement actions
  on a Program selection (challenge assumptions, simplify, crystallize,
  interview), the pinned inline terminal for interactive verbs, and authoring
  your own.
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
