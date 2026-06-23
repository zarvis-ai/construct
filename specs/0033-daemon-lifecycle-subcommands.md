# 0033-daemon-lifecycle-subcommands

Status: accepted
Date: 2026-06-23
Area: cli
Scope: How `construct daemon start | stop [--sessions] | restart [--sessions]` map onto daemon and session lifecycle.

## Decision

The `daemon` command exposes explicit lifecycle subcommands alongside the
foreground `run`:

- **`run`** stays the foreground entry point (for process supervisors). It is
  unchanged: it owns the socket and blocks.
- **`start`** spawns a detached background daemon if none is listening, then
  waits for the socket to bind. It is idempotent — a no-op success when a daemon
  already owns the socket.
- **`stop`** asks the running daemon to stop every session's adapter and exit.
  Sessions are left **resumable**: they are neither archived nor deleted, so the
  next `start` (or `run`) resumes them where they left off. Idempotent — a no-op
  success when no daemon is running.
- **`stop --sessions`** is accepted as an explicit spelling of the default
  `stop` behavior. It exists for operator clarity and symmetry with
  `restart --sessions`; it does not archive or delete sessions.
- **`restart`** restarts the running daemon **in place** (re-exec, PID
  preserved), or starts one if none is running. Sessions, their harness/adapter
  processes, and each session's `construct-mcp` child **survive and reattach** —
  they are not restarted.
- **`restart --sessions`** does everything `restart` does and additionally
  bounces every session's adapter process. Each adapter (and the `construct-mcp`
  child it owns) is respawned fresh. Sessions are **preserved/resumed** — neither
  archived nor deleted.

Teardown for these commands means "stop the processes," never "discard the
session." No lifecycle subcommand archives or deletes sessions.

## Reason

Operators and supervisors need conventional start/stop/restart verbs without
having to know the internal IPC. The building blocks already existed (detached
spawn for auto-start, in-place re-exec for upgrades, graceful adapter shutdown
for SIGTERM); these subcommands surface them as first-class operations.

The `--sessions` split exists because there is **no daemon-level MCP process to
restart**. Each `construct-mcp` server is a per-session stdio child spawned and
owned by that session's harness. The only way to restart a session's MCP child
is to restart its harness/adapter. So "restart everything, including MCP" is
necessarily the same operation as "bounce the adapters," and it lives under one
flag rather than pretending MCP is independently restartable.

Leaving sessions resumable on `stop`/`restart` matches the daemon's existing
restart-survives-sessions behavior: graceful adapter shutdown deliberately keeps
persisted session state non-terminal so the next boot resumes it.

## Consequences

- Future changes must keep `stop` and `restart` non-destructive to session
  state. Anything that wants to remove sessions is a separate, explicit action
  (archive/delete), not a side effect of a lifecycle command.
- The default `restart` must remain fast and minimally disruptive: harnesses and
  MCP children reattach rather than restart. Only `--sessions` pays the cost of
  bouncing them.
- Because the daemon re-execs in place, lifecycle commands confirm completion by
  polling socket reachability, not by observing the daemon exit — precise
  "new image is up" detection is not available from outside an in-place exec.
- If a daemon-level (non-per-session) MCP surface is ever introduced, this
  decision must be revisited: "restart MCP" would no longer be subsumed by
  `--sessions`.

## Non-Goals

- This does not add a "restart and wipe sessions" mode. Clearing sessions is the
  job of archive/delete, kept orthogonal to lifecycle.
- This does not change how SIGTERM/SIGINT/SIGHUP are handled by the daemon.
