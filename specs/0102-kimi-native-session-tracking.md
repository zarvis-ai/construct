# 0102-kimi-native-session-tracking

Status: accepted
Date: 2026-07-18
Area: harness
Scope: Native Kimi Code conversation identity inside Construct sessions.

## Decision

Each interactive Kimi Code harness session runs the `kimi` TUI under
Construct's PTY and records the native Kimi session id by observing Kimi's
own append-only session index in its home directory. Only index entries that
appear after the adapter starts, match the session's working directory, and
are not already claimed by a sibling Construct session are eligible to bind.
A Construct respawn resumes the recorded native session with Kimi's session
resume flag and does not resubmit the original prompt.

When the bound id changes mid-session (Kimi creating a fresh conversation),
the adapter reports the change through Construct's native-reset event so the
prior conversation becomes ordinary reset lineage. The recorded session's own
wire log is the source for model and thinking-effort reporting.

Kimi's prompt flag is headless-only, so the initial creation prompt is
delivered exactly once by typing it into the PTY after the native session is
observed to exist — never by switching the CLI into its non-TUI mode.

## Reason

Kimi Code assigns session ids itself and rejects resume requests for unknown
ids, so Construct can neither name a session up front nor derive the id from
the process. The session index is Kimi's own durable record of session
creation, ordered by creation time, which makes "first unclaimed entry for
this working directory after our spawn" the most precise ownership signal
available without modifying the user's Kimi configuration. Sibling exclusion
exists because multiple Construct sessions may run Kimi in the same working
directory against one shared index.

## Consequences

- Kimi command discovery honors explicit Construct overrides first, then the
  daemon's `PATH`, then the standard `~/.kimi-code/bin/kimi` installer
  location. Availability reporting and process launch use the same
  resolution.
- The watched Kimi home follows Construct's override, then Kimi's own home
  env var, then the default home; the adapter re-exports the resolved home
  to the child so the CLI and the watcher can never disagree.
- Failure to locate the index (or the persisted id inside it) must not
  prevent Kimi from launching; it degrades to a fresh conversation and lost
  model/effort reporting, and says so in the session log.
- The Kimi TUI continues to own approvals, tools, rendering, and session
  switching. Construct supplies the PTY, lifecycle, and persisted native id.
- Two Construct Kimi sessions created simultaneously in the same working
  directory can, in a sub-second window, bind each other's conversations —
  the same accepted tradeoff as other index/directory-observed harnesses.

## Non-Goals

This does not translate Kimi's internal tool calls or approvals into
Construct transcript events, does not inject Construct's MCP server (Kimi
currently exposes no per-process MCP configuration surface), and does not
implement native forks or usage probes.
