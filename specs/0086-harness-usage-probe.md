# 0086-harness-usage-probe

Status: accepted
Date: 2026-07-11
Area: harness
Scope: how the daemon captures and caches each harness's own usage/status output, and its native-transcript cleanup obligation.

## Decision

Each wrapper harness (claude, codex, agy, grok) has its own interactive
slash command for account/token usage (`/usage`, `/status`, ...). The
daemon can capture what that command renders on demand: spin up a
short-lived, daemon-internal session, send the harness's usage command,
capture the raw PTY bytes it produces, cache the capture in daemon memory
for 10 minutes, and tear the ephemeral session back down — including the
native transcript file the harness CLI wrote for it.

This capture is:

- **Lazy.** It only runs when a caller explicitly asks and the cache is
  stale or missing. There is no background timer that probes every
  harness on a fixed cadence.
- **Cache-gated.** A cached capture is reused for its full TTL regardless
  of how many callers ask for it; only one probe runs at a time per
  harness (concurrent triggers dedupe into the in-flight one).
- **Verbatim.** The captured bytes are stored and replayed exactly as
  rendered — colors, layout, wrapping — and never parsed into token
  counts or other structured fields.
- **In-memory only.** The cache lives on the daemon process and is lost on
  restart. A restart just means the next query re-probes.

### Session identity

The ephemeral session used to run the probe is a distinct session kind,
not an ordinary user session. It is excluded from every session list and
from any UI that enumerates a user's fleet. It never persists across a
daemon restart (it does not exist by the time a restart could matter,
since a probe's whole lifetime is seconds) and never appears as
resumable.

### Native-transcript cleanup

Every harness CLI wrapped by the daemon persists its own conversation
history somewhere outside the daemon's own storage (a resume picker, a
sessions directory, a per-conversation log, etc.). Spinning up a probe
session inevitably causes the underlying harness CLI to create one of
these native entries. Because probing is lazy and cache-gated (not a
fixed background cadence), the volume of probe-caused entries stays low
enough that the daemon can — and does — delete the specific native file
it caused to exist, immediately after capturing the probe's output. This
is the daemon's *only* native-transcript deletion path: it never deletes
a native transcript for a real user session, and it never deletes a
native transcript it did not itself just cause to be created. Failure to
delete a native file (missing, permission error, harness didn't finish
writing it yet) is logged and swallowed — it does not fail the probe or
withhold the captured output, which is the primary goal.

### Configuration semantics

The command a probe sends for each harness is configurable as a string,
not a boolean, with a three-way meaning chosen specifically because TOML
has no null literal:

- **Absent** (the field is not set at all): the harness's built-in
  default usage command runs.
- **Explicit empty string**: the probe is disabled for that harness —
  no session is ever spun up, and a query reports "not enabled."
- **Any other string**: sent verbatim instead of the built-in default.

"Absent" and "explicitly null" are the same representable state in TOML,
so this is the only three-way split available without inventing a sentinel
value; it matches the same absent-vs-empty-string convention already used
elsewhere in this config for harness-level opt-out.

### Delivery must be a real interactive submission

The probe command must be delivered to the harness the same way a human's
keystrokes would be — a paste of the command text followed by a distinct
submit action — not as a single bulk write of "text + newline" through a
harness-agnostic structured-input channel. Rich interactive harness TUIs
distinguish a real (bracketed) paste from raw bytes and only treat the
former as one atomic submission; without that framing, a bulk write can
visibly land the text in the input box and never submit it, silently
producing zero captured output. Any future probe-like feature that drives
one of these interactive harness TUIs programmatically must deliver input
the same way, not assume "type it and press enter" is equivalent across
delivery channels.

### Probe working directory avoids per-directory trust gates

Some harnesses gate a working directory they have not seen before behind
a first-run interactive trust prompt, before any other input is
processed. If the probe's working directory is untrusted, that prompt
consumes the probe's only turn and the probe captures the trust prompt
instead of usage output. The probe's working directory is therefore
chosen to be one the harness is likely to already trust — the daemon's
own process working directory, matching the same choice already made
elsewhere for other daemon-internal sessions — rather than a fixed
path (such as the user's home directory) that a harness has often never
seen and therefore does not trust. This is a best-effort mitigation, not
a guarantee: a harness can still show its trust prompt for a directory it
truly has never seen. When that happens the prompt itself is exactly what
gets captured (consistent with "redisplay verbatim, don't parse"), and a
later probe succeeds once the user has trusted that directory through
normal use.

### Native session storage shape varies per harness, and cleanup must match it

Not every harness persists one native session as a single flat file.
Some give each session its own exclusive directory containing several
sibling artifacts (session metadata, prompt/context caches, full version-
control history, uploaded files, ...) alongside the transcript-like file
the daemon otherwise reads to mirror native chat history. Deleting only
that one transcript-like file in such a case is insufficient — the
harness's own session picker can still show the entry, because the
directory (and whatever file it treats as the session's existence marker)
still exists. Native-transcript cleanup must therefore know, per harness,
whether the cleanup unit is a single file living in a directory *shared*
with other sessions (delete only that file) or a directory *exclusive* to
one session (delete the whole directory) — never assume the former
generically.

Separately, native-side persistence can lag process death: some harnesses
capture their own session id via a background watcher that polls
periodically rather than synchronously at spawn, and a harness's transcript
write can still land on disk a moment after its process receives a hard
kill signal (writes already in flight complete even though the process
cannot run more code). A single immediate check-and-delete attempt is not
reliable enough to catch this — cleanup must retry over a short bounded
window before concluding there is nothing to clean up.

### Query contract

Fetching a probe's cached result is a read-mostly operation: it returns
whatever is cached immediately and never blocks the caller on the probe
itself, even when a probe is warranted and gets triggered as a side
effect of the call. A caller that wants a fresh capture opts in
explicitly; the response tells it whether a refresh is now in flight so
it can poll again on its own cadence rather than blocking.

## Reason

Users have no way to see a harness's own account/usage status without
manually running that harness's command themselves inside a real working
session — disruptive if they just want a quick check. Surfacing it
passively (e.g. as a hover tooltip) requires the daemon to have *already*
captured it before the moment someone looks, which requires probing
proactively rather than only in response to an explicit user action in
that exact moment.

The "spin up, probe, tear down" model — rather than a single persistent
probe session per harness — was chosen because a long-lived idle session
per harness has its own costs (a resident adapter process, a slot in any
process/resource accounting) for a feature that's used in short bursts.
The tradeoff is that each probe leaves a footprint in the harness's own
native history unless the daemon cleans it up — hence the native-
transcript-cleanup obligation is treated as a first-class part of this
design, not an afterthought. This is safe specifically because probing
is on-demand and cache-gated: nothing about this design causes a
harness's native history to accumulate entries on a schedule the user
didn't ask for.

Capturing raw bytes instead of parsing structured fields avoids the
daemon needing to understand or maintain a per-harness output schema
that changes whenever a harness CLI's usage panel is redesigned. The
tradeoff is that the daemon cannot answer questions like "how many
tokens are left" programmatically — only "here is what the harness says,
verbatim."

## Consequences

- Any future harness added to the wrapper set that has its own usage/
  status command should get a `usage_probe` default and participate in
  this same lazy-probe-and-cache flow rather than inventing a parallel
  mechanism.
- A client rendering the cached capture must treat it as opaque terminal
  output (feed it through a terminal emulator/parser) rather than
  extracting fields from it. Any future desire for structured usage data
  (e.g. "warn at 80% of quota") requires a separate, explicitly-designed
  data path — this cache is not it and should not be repurposed for it.
- Adding cleanup support for a new harness means adding a path-resolution
  formula for that harness's native transcript location, determining
  whether that harness's native storage is a shared-directory single file
  or an exclusive per-session directory, and wiring the correct removal
  unit into the same cleanup step; it must not touch the deletion path for
  ordinary user sessions.
- A harness's built-in default probe command is a best guess at what that
  harness's interactive command set actually supports; it is not
  guaranteed correct for every harness version. The config override exists
  specifically so an operator can correct a wrong or missing default
  without waiting on a code change.
- Because the cache is in-memory and per-daemon-process, callers must not
  assume a captured snapshot survives a daemon restart, nor that it is
  shared across independently-running daemons.
- Because triggering a refresh never blocks the caller, a caller that
  needs a definite answer synchronously (rather than "check again
  shortly") is not well served by this design — it is built for a
  passive/ambient display, not a blocking status check.

## Non-Goals

- This does not attempt to normalize or unify what "usage" means across
  harnesses — each harness's own command decides what it shows, and the
  daemon does not reconcile differences between them.
- This does not add any new persistent storage; the cache is explicitly
  ephemeral daemon-process state.
- This does not change how real user sessions are created, resumed, or
  deleted, and does not touch native-transcript files belonging to real
  user sessions under any circumstance.

## Examples

- An operator sets a harness's usage-probe field to an empty string in
  config to disable probing for that harness entirely (e.g. because its
  usage command is slow or the harness doesn't support one usefully) —
  the daemon never spins up a session for it, and a query for it reports
  "not enabled" without attempting anything.
- An operator overrides a harness's usage-probe command to pass extra
  flags the harness's CLI supports — the probe sends that exact string
  instead of the built-in default.
- Two callers ask for the same harness's usage snapshot within a few
  seconds of each other, both while the cache is stale: only one probe
  session is spun up; the second caller's request is told a refresh is
  already in flight rather than causing a second concurrent probe.
