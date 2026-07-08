# 0076-session-search

Status: accepted
Date: 2026-07-08
Area: protocol
Scope: full-text search across session name/metadata, program contents, and transcript history, exposed uniformly to daemon clients, MCP, CLI, and the TUI picker.

## Decision

`session.search` is a read-only daemon IPC method that scans, on demand, three
independent scopes for a case-insensitive substring query:

- **Name** — the same fields the TUI's instant session-switcher match already
  covers (title/label, full id, short id, harness). At most one hit per
  session for this scope.
- **Program** — the session's stored program document, matched line by line.
- **Transcript** — the session's persisted event log, matched against the
  human/model-relevant text of message, reasoning, and tool-call events.

The engine is a **scan, not an index**: every call walks the relevant files
fresh. There is no persistent search index, no background indexing job, and
no on-disk artifact besides what already exists for playback (`program.md`,
`transcript.jsonl`). Sessions are visited most-recent-activity first; within a
session, name hits precede program hits, which precede transcript hits, and
transcript/program hits are newest-first.

A transcript hit's cursor is the event's `seq` — the same sequence number
`session.transcript`'s `from` parameter already accepts. This is the sole
mechanism for a caller to jump from a hit to its surrounding context; the
search result carries no other positional information.

The scan is bounded by byte and hit budgets, not wall-clock time, so behavior
is deterministic and testable:

- A per-session cap on transcript bytes read from the tail.
- A global cap on total transcript bytes read across every session in one
  call.
- A global cap on the number of hits returned across every scope and
  session.
- A per-session, per-scope cap on hits contributed by program and transcript
  scopes.

Whenever any of these budgets or caps stops the scan before it would
otherwise have finished — a session's transcript scan hit its byte cap, a
scope hit its per-session limit, or the global hit limit was reached before
every session was visited — the result reports `truncated: true`. This is
advisory: it means "there may be more," never "there is definitely more" or
"here is exactly what's missing."

The same query semantics and result shape are exposed through every
surface built on top of the daemon: the IPC method itself, an MCP tool, a
CLI subcommand, and an async second tier layered onto the existing
in-memory session-switcher (`C-x b`) picker in the TUI. The picker's
existing instant, in-memory name/id/harness matching (tier 1) is unchanged;
the new tier only adds debounced program/transcript results underneath it,
guarded by a query-generation counter so a stale in-flight or completed
search response can never clobber the results of a query the user has since
edited away from.

## Reason

Session history and program documents already accumulate real, searchable
content the moment a session exists — there is no reason a user or an agent
should have to remember which of dozens of sessions contains a fact it once
saw. A scan-first engine gets full coverage (every session, whatever its
age) shipped immediately, without committing to an index format, a storage
migration, or incremental-update logic before the feature has proven its
shape. Transcripts are append-only and can be multi-gigabyte, so the engine
still has to behave like a production system on day one — hence the byte
budgets and the newest-first, early-exit scan order, not a naive full-file
read per session.

## Consequences

- Clients that need "more results" narrow the request (`session_ids`,
  `scopes`, a more specific query) rather than paginating a truncated scan;
  there is no cursor/offset for resuming a truncated search.
- The API surface (params/result shape, the `seq`-as-cursor contract, the
  scope enum) is intentionally storage-agnostic: a future FTS index (or any
  other engine swap) can replace the scan implementation behind
  `session.search` without changing any client — MCP tool, CLI, or TUI. Any
  such swap must preserve the ordering and truncation contracts above, since
  clients render and reason about results assuming them.
- Because the scan is bounded, a query against a very large or very old
  fleet of sessions can legitimately miss matches that exist further back
  than the budgets reach. This is an accepted tradeoff for bounded, testable
  latency over completeness; a future index-backed engine is expected to
  remove it.
- The TUI's async tier must never be allowed to block or visibly stall the
  picker: it is debounced, cancellable mid-flight (superseded by further
  typing), and rendered as a clearly separate, dismissible section rather
  than merged into the always-instant tier-1 rows.

## Non-Goals

- No search of raw PTY/shell scrollback (`pty.log`). That content is raw
  terminal bytes including ANSI escape sequences; stripping them for
  matching is deferred to a future iteration.
- No regex, whole-word, or other advanced query modes — substring only,
  case-insensitive.
- No persistent search index of any kind.
- No jump-to-match positioning inside the transcript or program view. A
  transcript hit's `seq` is a cursor a caller can feed back into
  `session.transcript`; the TUI picker's "select a content-match row"
  action switches to that hit's session and stops there — it does not
  scroll the view to the match.

## Examples

- A CLI invocation restricted to one session and one scope (e.g. only the
  transcript of a specific session) is expected to be effectively exhaustive
  for that session, modulo the per-session transcript byte budget.
- An MCP tool call returns a transcript hit's `seq`; the calling agent is
  expected to pass that value as the `from` parameter of the transcript-read
  tool to see the surrounding turns, since the search result's snippet is
  deliberately short.
- Typing quickly in the TUI's `C-x b` picker never fires one search per
  keystroke and never shows results for an earlier, already-abandoned
  query — both are guaranteed by the debounce-plus-generation-counter
  design, not by accident of timing.
