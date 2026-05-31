# Harnesses

A **harness** is the engine that runs a session. agentd gives each harness the
same outer shape — sessions, transcripts, approvals, widgets, restart behavior,
and remote control — while the harness decides how the actual assistant or shell
runs.

You can mix harnesses in one fleet. For example, keep a shell open, run Codex in
one pane, ask Claude in another, and use Zarvis as the built-in agent that can
coordinate them.

## Available harnesses

| Harness | What it is | Best for |
| --- | --- | --- |
| `zarvis` | agentd's built-in agent | Native tool use, session orchestration, widgets, and model-provider routing without a separate CLI. |
| `shell` | Your local shell | Long-running commands, logs, REPLs, and manual debugging. |
| `claude` | The Claude CLI | Using Claude Code exactly as installed on your machine, inside the agentd fleet. |
| `codex` | The Codex CLI | Using Codex exactly as installed on your machine, inside the agentd fleet. |
| `antigravity` | The Antigravity CLI | Using Antigravity sessions inside the same UI and daemon. |

Create a session with:

```sh
agent new zarvis "review this repo"
agent new shell ""
agent new codex "implement the failing test"
```

## Interactive and headless sessions

Most harnesses can run in two modes:

- **Interactive**: the harness owns a PTY, so its normal terminal UI appears in
  the agentd pane. This is the default when you create sessions from the TUI.
- **Headless**: the harness emits structured events instead of a terminal UI.
  This is useful for automation and non-PTY clients.

Choose explicitly when needed:

```sh
agent new claude --mode interactive ""
agent new zarvis --mode headless "summarize the last run"
```

`zarvis`, `claude`, `codex`, and `antigravity` support both modes. `shell` is
always interactive because it is a terminal program.

## What agentd provides to every harness

agentd is not just a process launcher. It provides a shared **session contract**
for every harness:

- **One session model**: every harness appears as a session with an id, title,
  state, transcript, scrollback, cwd, and lifecycle.
- **One UI surface**: sessions show in the same TUI, Web UI, and remote-control
  APIs, even when their internal CLIs are different.
- **One approval flow**: risky actions surface through agentd's approval UI when
  the harness exposes enough control for agentd to mediate them.
- **One widget system**: harnesses can publish compact Markdown status panels and
  action links through the session widgets directory.
- **One persistence story**: transcripts and PTY logs stay readable after a
  restart; adapters resume their upstream harness when that harness supports it.
- **One configuration layer**: agentd passes common environment and policy into
  harnesses so user-facing features can be defined once and reused.

A useful term for this is **capability injection**: define a capability once at
the agentd layer, and agentd injects it into the harness in the form that harness
understands.

Examples:

- Define skills once, and agentd can include the relevant skill instructions in
  Zarvis prompts. Wrapper harnesses still run their own upstream CLIs, but they
  receive the same session context and environment where applicable.
- Define a session widget once, and every client renders it the same way.
- Define an auto-approval policy once, and adapters translate it into their
  harness's native allow-list when the upstream CLI supports that.
- Define session metadata once — cwd, title, mode, worktree, environment — and
  every harness starts with the same agentd-managed context.

The adapter is the translation layer between this shared contract and a specific
harness.

## Fleet-wide capabilities

agentd's fleet-wide capabilities are features you define once at the
agentd/session level and then reuse across harnesses where possible.

Use them when you want a fleet-wide behavior instead of configuring each harness
by hand. For example: "make these skills available," "allow writes under this
session widget directory," or "show this status widget in every client."

| Abstraction | How to use it | Support |
| --- | --- | --- |
| Session identity and lifecycle | Create sessions with `agent new <harness> ...`. agentd assigns ids, tracks state, persists start params, and exposes the same list/status APIs. | All harnesses. |
| Transcript and scrollback | Use the TUI/Web UI or `agent` APIs to view session history. PTY output and structured events are stored by agentd. | All harnesses. Fidelity depends on whether the harness emits PTY bytes, structured events, or both. |
| Cwd, environment, and worktree context | Start a session in the desired cwd, optionally with a worktree. agentd passes session env such as `AGENTD_SESSION_ID`, data dirs, widget dirs, and resume flags to the adapter. | All harnesses receive the context; each upstream CLI decides what to do with it. |
| Skills | Install or define skills in the normal agentd/Zarvis skill locations. Zarvis loads matching skills and injects their instructions into the model context when relevant. | Native in `zarvis`. Wrapper harnesses (`claude`, `codex`, `antigravity`) keep using their own skill/plugin systems; agentd does not rewrite their prompts today. |
| Widgets | Write Markdown files to `AGENTD_SESSION_WIDGETS_DIR`. agentd renders them in the TUI/Web UI and supports action links. | All harnesses can write widgets if they know the directory. `zarvis` has first-class tool support; wrappers can write files through their own tools or shell commands. |
| Approval policy | Configure path-scoped auto-approval through agentd's approval policy. agentd injects `AGENTD_AUTO_APPROVE_PATHS`; adapters translate it to the harness's native allow-list when available. | Native in `zarvis`; translated for `claude`; limited by upstream support for `codex` and `antigravity`. |
| Tool/event display | Harnesses emit PTY output and/or structured events. agentd renders common session chrome, tool status, grouped tool calls, approvals, and errors where it has structured data. | Best for `zarvis`; wrapper detail depends on what the upstream CLI exposes. |
| Resume | agentd stores session start params and provides per-session data dirs. Adapters persist upstream ids when the upstream CLI supports resume. | `zarvis` resumes from agentd state; `shell` restarts in the same cwd; wrappers resume when their CLI exposes a reliable resume mechanism. |

### Example: make skills available

For the built-in agent, define skills once and let Zarvis load them as part of
its context selection:

```sh
agent new zarvis "use the repo's release skill to cut the next version"
```

That skill setup is shared at the agentd/Zarvis layer: you do not need to copy the
same instructions into every Zarvis session manually.

For wrapper harnesses, use the upstream system for that CLI. agentd still manages
the session, cwd, transcript, widgets, and lifecycle, but it does not currently
inject agentd/Zarvis skills into Claude, Codex, or Antigravity prompts.

### Example: share an approval policy

agentd can define one path-scoped policy, then adapters apply it where possible.
The most common built-in use is the session widget directory: harnesses should be
able to update their own widget Markdown without asking every time.

The support is intentionally adapter-specific:

- `zarvis` enforces the policy directly because its tools run through agentd.
- `claude` receives equivalent allowed-tool patterns when it starts.
- `codex` and `antigravity` currently expose less path-scoped approval control,
  so agentd can pass the policy but cannot always force the upstream CLI to honor
  it.

### Example: publish one widget to every client

A harness can write:

```sh
cat > "$AGENTD_SESSION_WIDGETS_DIR/status.md" <<'MD'
# Status

- [~] Working
- [ ] Run tests
MD
```

agentd then renders that widget in every client that supports widgets. The
harness does not need separate TUI and Web UI code.

## Built-in vs wrapper harnesses

There are two kinds of harnesses:

### Built-in harness

`zarvis` is native to agentd. Because it runs inside the agentd adapter, it can
use agentd features directly: tools, approvals, skills, widgets, background
tasks, and structured status updates.

See [Zarvis built-in agent](zarvis.md) for details.

### Wrapper harnesses

`claude`, `codex`, and `antigravity` wrap existing CLIs. agentd starts the CLI,
connects it to a session, records its output, and injects the common session
context it can provide.

Wrapper harnesses keep their native behavior. If an upstream CLI does not expose
a setting — for example, path-scoped tool auto-approval — agentd cannot always
force that behavior from outside the process. In those cases the session still
gets the shared UI, transcript, lifecycle, and environment, but the upstream CLI
keeps control of its own internals.

## Resume after restart

When agentd restarts, it restores sessions from saved start parameters:

- PTY scrollback and transcripts remain readable.
- `shell` starts a fresh shell in the original cwd.
- `zarvis` reloads its persisted conversation state.
- Wrapper harnesses resume when their upstream CLI provides a reliable session id
  or resume command.

If a harness cannot be restarted — for example, its binary is missing — agentd
marks the session errored while keeping the transcript available.

## Common knobs

You normally do not need these, but they are useful for scripting and debugging:

| Setting | Purpose |
| --- | --- |
| `--mode interactive\|headless` | Choose the session mode at creation time. |
| `AGENTD_ZARVIS_MODE`, `AGENTD_CLAUDE_MODE`, `AGENTD_CODEX_MODE`, `AGENTD_ANTIGRAVITY_MODE` | Default mode per harness. |
| `AGENTD_*_CMD` | Override the full command used for a built-in wrapper harness. |
| `AGENTD_*_BIN` | Override just the binary path when no full command override is set. |
| `AGENTD_AUTO_APPROVE_PATHS` | Path allow-list injected into adapters that can translate it. |
| `AGENTD_SESSION_WIDGETS_DIR` | Directory where a session writes Markdown widgets. |

Prefer the normal `agent new ...` flow unless you are integrating agentd into a
larger script or testing a custom harness setup.
