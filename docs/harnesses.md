# Harnesses

A **harness** is an agent or shell runner inside construct. Harnesses let you run
smith, Claude, Codex, Antigravity, and local shells side by side while construct
gives them one UI, history, widgets, control plane, and shared approval surface
where supported.

A **fleet** is the set of sessions managed by one construct daemon. For example,
you can keep a shell running tests, ask Codex to implement a fix, ask Claude to
review it, and use smith as the built-in coordinator.

## Which harness should I use?

| Harness | What it is | Use it when |
| --- | --- | --- |
| `smith` | construct's built-in agent | You want the deepest construct integration: native tools, approvals, skills, widgets, orchestration, and model-provider routing. |
| `shell` | Your local shell | You need long-running commands, logs, REPLs, servers, or manual debugging. |
| `claude` | The Claude CLI | You already use Claude Code and want it inside the same construct UI and session fleet. |
| `codex` | The Codex CLI | You already use Codex and want it inside the same construct UI and session fleet. |
| `antigravity` | The Antigravity CLI | You want Antigravity sessions inside the same UI and daemon. |

Create a session with:

```sh
construct new smith "review this repo"
construct new shell
construct new codex "implement the failing test"
```

By default, `construct new ...` creates an interactive session and opens the TUI
focused on it. Pass `--mode headless` when you want a script-friendly command
that creates a headless session, prints its id, and exits. Pass `--no-tui` when
you want to create an interactive session, print its id, and stay in the current
terminal.

CLI-backed harnesses require the matching CLI to be installed and discoverable on
`PATH`. Use the `*_BIN` or `*_CMD` environment variables below when you need to
point construct at a specific binary or command.

## What construct gives every harness

construct gives every harness the same shared session model, then lets each adapter
translate that model into the underlying agent or shell.

| Capability | Why it matters | Support and details |
| --- | --- | --- |
| Session identity and lifecycle | Every harness has the same id, title, state, cwd, mode, transcript, and lifecycle. | All harnesses. |
| Transcript and scrollback | You can inspect session history from the TUI, Web UI, and remote APIs, even after restart. | All harnesses; fidelity depends on what the harness emits. |
| Shared UI | Different CLIs appear in one session list instead of separate terminals. | All harnesses. |
| Approval flow | Risky actions can use construct's approval UI instead of each session inventing its own workflow. | Native in `smith`; translated where CLI-backed harnesses expose enough control. |
| Widgets | Agents can publish Markdown status/action panels once and every client can render them. | All harnesses can write widgets via `CONSTRUCT_SESSION_WIDGETS_DIR`; see [Generative widgets](generative-widgets.md). |
| Session context | Sessions receive shared cwd, environment, data dirs, widget dirs, memory pointers, and resume flags. | All harnesses receive the context; each upstream CLI decides what to do with it. |
| Skills | Reusable instructions can be defined once for the built-in agent. | Native in `smith`; CLI-backed harnesses use their own upstream skill/plugin systems today. |
| Unified tools | Agents can inspect and coordinate the fleet without shelling out to `construct` commands. | Native in `smith`; injected through MCP where supported; see [Unified tool layer](unified-tool-layer.md). |
| Resume | Restarts do not wipe out what you were looking at, and some upstream CLIs can continue the same conversation. | `smith` resumes from construct state; `shell` restarts in the same cwd; CLI-backed harnesses resume when their CLI exposes a reliable mechanism. |

The adapter is the translation layer between these fleet-wide capabilities and a
specific harness. Some capabilities are native in smith, some are injected into
CLI-backed harnesses, and some depend on what the upstream CLI exposes.

## Built-in vs CLI-backed harnesses

There are two kinds of harnesses:

### Built-in harness

`smith` is native to construct. Use it when you want access to the most construct
features: tools, approvals, skills, widgets, background tasks, and structured
status updates.

See [smith built-in agent](smith.md) for details.

### CLI-backed harnesses

`claude`, `codex`, and `antigravity` wrap existing CLIs. Use them when you want
those tools exactly as installed on your machine, but inside the same construct
fleet.

CLI-backed harnesses keep their native behavior. If an upstream CLI does not
expose a setting — for example, path-scoped tool auto-approval — construct cannot
always force that behavior from outside the process. In those cases the session
still gets the shared UI, transcript, lifecycle, and environment, but the
upstream CLI keeps control of its own internals.

Claude Code, Codex, Antigravity, and Grok subagents created through their native
delegation tools appear beneath the owning session as `(native)` child rows.
Their live state and structured transcript are inspectable like any other
session, including nested children. These rows are read-only mirrors: use the
parent CLI's native subagent commands to message, interrupt, resume, or remove
them. Removing a Claude child archives its mirror while retaining the
transcript. A native child from any harness is archived automatically when it
reaches a terminal state, preserving both its transcript and terminal outcome.

## Interactive and headless sessions

Most harnesses can run in two modes:

- **Interactive**: the harness owns a PTY, so its normal terminal UI appears in
  the construct pane. This is the default when you create sessions from the TUI.
- **Headless**: the harness emits structured events instead of a terminal UI.
  This is useful for automation and non-PTY clients.

**How the mode is chosen.** An explicit `--mode` always wins. Otherwise the mode
is *interactive* when the creating client supplied a PTY size or used the
`construct new` CLI, and *headless* when it did not. The TUI always supplies one,
so TUI sessions default to interactive.

**The initial prompt does not pick the mode.** `construct new <harness> "<prompt>"`
and `construct new <harness>` both create interactive sessions unless you pass
`--mode headless`; the prompt only decides what the session does once it starts:

- A non-empty prompt is recorded as the first user turn and run immediately. For
  headless clients this is the structured-output path (for example, `shell`
  runs `$SHELL -lc "<prompt>"` once and exits).
- An empty prompt launches the harness's interactive program (for example,
  `shell` runs `$SHELL -il`), which you can attach to and type into.

Pass `--mode` to choose explicitly (optionally alongside a seed prompt):

```sh
construct new claude
construct new --no-tui claude
construct new smith --mode headless "summarize the last run"
```

`smith`, `claude`, `codex`, and `antigravity` support both modes. `shell` always
owns a PTY (there is no structured "headless" shell), so it presents a terminal
regardless of the mode label.

## Resume after restart

When construct restarts, it restores sessions from saved start parameters:

- PTY scrollback and transcripts remain readable.
- `shell` starts a fresh shell in the original cwd.
- `smith` reloads its persisted conversation state.
- CLI-backed harnesses resume when their upstream CLI provides a reliable session
  id or resume command.

If a harness cannot be restarted — for example, its binary is missing — construct
marks the session errored while keeping the transcript available.

## Common knobs

You normally do not need these, but they are useful for scripting and debugging:

| Setting | Purpose |
| --- | --- |
| `--mode interactive\|headless` | Choose the session mode at creation time. |
| `CONSTRUCT_SMITH_MODE`, `CONSTRUCT_CLAUDE_MODE`, `CONSTRUCT_CODEX_MODE`, `CONSTRUCT_ANTIGRAVITY_MODE` | Default mode per harness. |
| `CONSTRUCT_CLAUDE_CMD`, `CONSTRUCT_CODEX_CMD`, `CONSTRUCT_ANTIGRAVITY_CMD`, `CONSTRUCT_SHELL_CMD` | Override the full command used for a CLI-backed harness or shell. |
| `CONSTRUCT_CLAUDE_BIN`, `CONSTRUCT_CODEX_BIN`, `CONSTRUCT_ANTIGRAVITY_BIN`, `CONSTRUCT_SHELL_BIN` | Override just the binary path when no full command override is set. |
| `CONSTRUCT_SMITH_MODEL` | Default model for the built-in smith harness. |
| `CONSTRUCT_AUTO_APPROVE_PATHS` | Path allow-list injected into adapters that can translate it. |
| `CONSTRUCT_SESSION_WIDGETS_DIR` | Directory where a session writes Markdown widgets. |
| `CONSTRUCT_INJECT_MCP=0` | Disable automatic MCP tool injection for MCP-capable harnesses. |

Set these in the daemon environment, or in whatever process starts `construct`. See
[Configuration](configuration.md) for general configuration patterns.

Prefer the normal `construct new ...` flow unless you are integrating construct into a
larger script or testing a custom harness setup.
