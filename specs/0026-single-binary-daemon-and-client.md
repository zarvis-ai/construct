# 0026-single-binary-daemon-and-client

Status: accepted
Date: 2026-06-07
Area: architecture
Scope: The daemon and the TUI/CLI client ship as one executable; the daemon's runtime is a library with thin, interchangeable entry points.

## Decision

The daemon and the client are **one shipped binary**, not two. The `construct`
binary runs the TUI by default and runs the daemon under a `daemon` subcommand
(`construct daemon run`). The daemon's entire runtime lives in a library crate
so it can be driven from more than one entry point.

A standalone `constructd` binary remains as a **back-compat alias** that is a
thin shim over the same library. `constructd run` and `construct daemon run`
are equivalent and share 100% of their code path.

This consolidation is about the *executable*, not the *process*. The daemon is
still a single long-lived process that many clients attach to over the IPC
socket; running `construct` (the TUI) does not start an embedded daemon, and
running it multiple times yields multiple clients against one daemon — never
multiple daemons.

## Reason

- One installed binary can do everything (client + daemon), simplifying
  install, upgrade, and the mental model — you no longer need two files on
  PATH to run the system.
- The daemon's logic was bin-only and unreachable as a library; promoting it to
  a library lets the unified binary call it directly with no duplication and no
  subprocess hop.
- Keeping `constructd` as an alias avoids breaking existing installs, scripts,
  muscle memory, and the atomic-rename upgrade layout, which replaces binaries
  in place at fixed paths.

## Consequences

- **The daemon's runtime must stay library-shaped.** Its public entry points
  (run, tracing init, paths printing, default-config) are what both binaries
  call. Don't push entry-point-only logic back into a `main`.
- **Self-restart re-execs by replaying argv verbatim.** The daemon picks up an
  upgraded binary by `exec()`ing its startup-captured executable path with its
  original arguments. This is why a symlink/`argv[0]`-multiplex approach was
  rejected: resolving a `constructd`→`construct` symlink would drop the name
  while the replayed args still assume daemon dispatch, breaking restart. Each
  real entry point must replay an argv that re-enters the *same* mode
  (`construct daemon run …` re-execs `construct daemon run …`; `constructd run`
  re-execs `constructd run`).
- **The unified binary links both dependency sets** (TUI rendering + daemon
  server/tunnel). Accept the larger single binary in exchange for shipping one
  file instead of two; most heavy dependencies are already shared.
- **Daemon mode owns the socket; client mode connects to it.** Mode selection
  must happen before socket discovery and before tracing init, so the daemon's
  verbose log filter applies in daemon mode and the client's quiet filter
  applies otherwise.
- **The client auto-starts a daemon when none is live.** Since one binary does
  both, running the TUI with no daemon is a setup mistake, not a user intent —
  so the TUI spawns a detached `construct daemon run` in the background and
  waits for the socket, instead of erroring out. Opt out with
  `CONSTRUCT_NO_AUTOSTART=1`. Auto-start is best-effort: on failure the normal
  connect error still surfaces.
- **A single-instance lock makes concurrent starts safe.** Auto-start means two
  `construct` launches (or a stray second `daemon run`) can race to start a
  daemon. The daemon takes an exclusive advisory file lock keyed to its socket
  path (`<socket>.lock`) before binding; the loser exits cleanly rather than
  unlinking and stealing the live socket. The lock is held for the process
  lifetime and released across the restart `exec()` (CLOEXEC fd), so the
  re-execed image re-acquires it. Daemons on *different* sockets don't contend.

## Non-Goals

- Not merging the adapter binaries or the MCP bridge into the unified binary.
  Adapters are an independent process/plugin boundary and stay separate.
- Not auto-starting a daemon for one-shot control subcommands (`list`, `ping`,
  …). Only the interactive TUI auto-starts one; a scripted one-shot that finds
  no daemon should fail fast rather than leave a lingering background daemon.

## Examples

- First run: `construct` with no daemon running auto-starts one in the
  background and attaches — no separate `construct daemon run` step needed.
- Start the system explicitly: run the daemon in one place
  (`construct daemon run`), attach one or more clients elsewhere (`construct`).
- Upgrade in place, then restart the running daemon: the daemon re-execs itself
  at the same path and rebinds the socket without losing sessions, regardless of
  whether it was launched as `construct daemon` or `constructd`.
- A legacy script or habit that runs `constructd run` keeps working unchanged.
