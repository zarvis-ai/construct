# 0026-single-binary-daemon-and-client

Status: accepted
Date: 2026-06-07
Area: architecture
Scope: The daemon, TUI/CLI client, and client-facing protocol entrypoints ship as one executable; the daemon's runtime is a library driven by the `construct daemon` subcommand, with no standalone daemon binary.

## Decision

The daemon and the client are **one shipped binary**, not two. The `construct`
binary runs the TUI by default and runs the daemon under a `daemon` subcommand
(`construct daemon run`). The daemon's entire runtime lives in a library crate
(`agentd`) that the `construct` binary drives — there is **no standalone daemon
binary**.

Client-facing protocol servers that attach to the daemon also belong in the
same `construct` binary when they are part of the client surface. For example,
ACP is exposed as `construct acp`, not as a separate `construct-acp` executable.

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
- A single binary is the whole point: a standalone `constructd` was kept only
  briefly as a transition alias, then dropped once `construct daemon` and TUI
  auto-start covered every way it was used. Fewer binaries to build, ship, and
  install.
- ACP clients need a stdio command, but they do not need a separate executable.
  A `construct acp` subcommand preserves the one-binary install model while
  still letting external ACP clients launch construct as their agent server.

## Consequences

- **The daemon's runtime must stay library-shaped.** Its public entry points
  (run, tracing init, paths printing, default-config) are what both binaries
  call. Don't push entry-point-only logic back into a `main`.
- **Self-restart re-execs by replaying argv verbatim.** The daemon picks up an
  upgraded binary by `exec()`ing its startup-captured executable path with its
  original arguments (`construct daemon run …` re-execs `construct daemon run
  …`). This is also why a standalone `constructd` was *not* kept as a symlink to
  `construct`: resolving the symlink would drop the `constructd` name while the
  replayed args still assume daemon dispatch, breaking restart. With a single
  real binary the replayed argv always re-enters daemon mode correctly.
- **The single binary links every layer** (TUI rendering + daemon
  server/tunnel). Accept the larger binary in exchange for shipping one file;
  most heavy dependencies are already shared.
- **Daemon mode owns the socket; client mode connects to it.** Mode selection
  must happen before socket discovery and before tracing init, so the daemon's
  verbose log filter applies in daemon mode and the client's quiet filter
  applies otherwise.
- **ACP mode is a client mode.** It connects to or auto-starts the daemon and
  translates ACP session lifecycle calls into daemon IPC; it does not own
  construct sessions directly and should not be modeled as an AHP harness
  adapter.
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
- **The release tarball keeps its historical `constructd-<target>` name** even
  though it no longer contains a `constructd` binary, because already-released
  `install.sh` / `construct upgrade` builds fetch that exact asset name.
  Renaming it would break the upgrade path. Dropping `constructd` from the
  packaged binary set is itself a one-time upgrade break: an `install.sh` baked
  into an older binary lists `constructd` in its `BINS` and aborts when it's
  absent from the new tarball, so that single hop must re-run the install
  one-liner instead of `construct upgrade`.

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
  at the same path (`construct daemon run …`) and rebinds the socket without
  losing sessions.
- Migration: anything that ran `constructd run` now runs `construct daemon run`.
