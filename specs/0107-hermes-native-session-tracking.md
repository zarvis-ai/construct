# 0107-hermes-native-session-tracking

Status: accepted
Date: 2026-07-24
Area: harness
Scope: Hermes sessions use the upstream durable store as the authority for identity, resume, transcript, model, and usage.

## Decision

Each construct-owned Hermes process receives a unique native session source tag
derived from its construct session id. The adapter follows only rows with that
tag in Hermes' durable SQLite session store and persists the active Hermes id in
the construct session's data directory.

Interactive sessions run Hermes in a PTY. Headless turns use Hermes' one-shot
command and resume the captured native id on later turns. In both modes, the
SQLite rows—not terminal scraping—are authoritative for structured messages,
reasoning, tool calls and results, model and effort changes, and cumulative
token and dollar-cost counters.

On daemon restart, the adapter resumes the persisted Hermes id and seeds its
message cursor and usage baseline from the existing row so history is not
emitted twice. When Hermes rotates to a newer row with the same source tag
because of a native reset or compression boundary, the adapter emits a native
id change, starts reading the new row from the beginning, and resets its usage
baseline.

Hermes does not expose a non-mutating CLI launch flag that forks a resumed
conversation. A same-harness construct fork therefore starts a new Hermes
conversation with construct's portable transcript seed instead of claiming a
native fork.

## Reason

Hermes already records exact, structured session state in a WAL-mode database
and supports custom source tags. Using that surface avoids ambiguous process
matching, ANSI scraping, guessed usage, and duplicate accounting while keeping
Hermes' own resume semantics intact.

## Consequences

- The Hermes home used by the child and watcher must be the same.
- A missing or unreadable Hermes database degrades PTY operation but disables
  native resume and semantic events until the store becomes readable.
- Resume skips existing message rows and cumulative usage; a newly detected
  native id backfills from its first row.
- Token input includes fresh input, cache reads, and cache writes; cached input
  remains the cache-read subset. Reasoning tokens count as output.
- Context-window fill is omitted until Hermes records the most recent call's
  prompt-side usage and stated window size.
- MCP injection and path-scoped approval translation remain explicit gaps until
  Hermes offers per-invocation configuration surfaces that do not mutate the
  user's persistent configuration.

## Non-Goals

- Mutating the user's Hermes configuration or session database.
- Inferring a context window from model names.
- Treating resume as a native fork.
