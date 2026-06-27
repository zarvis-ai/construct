# 0031-session-archive

Status: accepted
Date: 2026-06-17
Area: ux
Scope: Archiving a session terminates its process and hides it from the list while preserving its history for later restart.

## Decision

A session can be **archived** as a third outcome of the kill prompt (`C-x k` / the
view `x` button), alongside delete and cancel:

- **Archive terminates the session's adapter but keeps everything on disk** —
  transcript, worktree, start params, widgets. It is the non-destructive
  counterpart to delete (which drops all of that).
- **Archived sessions are hidden from the session list by default**, revealed
  per section. The list has independent sections — the ungrouped top-level run
  and each project — and any section that contains archived sessions ends with an
  expandable "N archived" row. Activating that row reveals/hides only that
  section's archived sessions; while shown, their names render dimmed. The reveal
  state is per-section and not persisted (archived sessions default to hidden on
  each launch). The row is a first-class list entry: keyboard navigation lands on
  it (it is not skipped), the list expand/collapse keys reveal/hide it like a
  project header, and a slash command toggles the section the current selection
  lives in.
- **An archived session behaves like any other when selected** — its history
  renders normally and it can be restarted.
- **Restarting an archived session un-archives it.** Restart brings the session
  back to life and returns it to the active list (the archived flag clears).
- **The daemon does not auto-resume archived sessions on startup.** They stay
  terminated across daemon restarts until an explicit restart.
- **Archived state is persisted** on the session record so it survives daemon
  and client restarts.
- **Archiving is atomic against adapter teardown.** A single archive action
  reliably leaves the session archived — terminating the live adapter must never
  race ahead of (or behind) the archived flag and leave the session looking
  merely stopped (which would force the user to archive a second time). The
  daemon records the archive intent and persists+broadcasts the archived,
  terminal state *before* it tears the adapter down, and the adapter's
  asynchronous close handler keeps `archived` set when it persists/broadcasts
  the resulting terminal state. Every client reflects the archive on the first
  action.

The kill prompt requires an explicit choice: `d` deletes (destructive), `a`
archives, and anything else (including a reflexive `y`/Enter) cancels.

## Reason

Users accumulate finished or paused sessions they don't want cluttering the list
but aren't ready to destroy — a delete throws away transcript and worktree
irreversibly. Archive gives a reversible "put it away" that preserves the ability
to read history and pick the work back up, without a separate heavyweight
lifecycle state. Hiding by default keeps the working set focused; the toggle and
dimming keep archived work discoverable without competing visually with active
sessions. Un-archiving on restart matches the intuition that resuming a session
means you're using it again, so it belongs back in the active list. Requiring an
explicit `d` for delete (no `y` alias) keeps the destructive option from being a
single reflexive keystroke once a non-destructive option shares the prompt.

## Consequences

- The session record carries an `archived` flag that future changes must keep
  orthogonal to run state: archive is about list visibility and auto-resume
  suppression, not a new terminal `SessionState`. Startup auto-resume must keep
  skipping archived sessions regardless of their state.
- Restart is the canonical un-archive path; if a separate explicit "unarchive
  without restarting" action is ever added, it must also clear the flag and
  re-list the session.
- Clients are responsible for filtering archived sessions out of their list view
  (the daemon still includes them in the session list so a client can show them
  on demand and render their history). A client without archive support will show
  archived sessions as ordinary entries — acceptable and non-breaking.
- Archive keeps the worktree; only delete removes it. Disk usage of archived
  sessions persists until they are explicitly deleted.
- Any session-lifecycle action that flips a persisted flag while also
  terminating a live adapter must guard against the adapter's asynchronous close
  event clobbering that flag. The close handler runs on its own task and
  persists+broadcasts the session's terminal state; it must honor in-flight
  intent (such as archive) instead of overwriting it with stale state.

## Non-Goals

- No bulk archive/unarchive, auto-archive policies, or archive retention/expiry.
- No separate archived storage location or compaction; archived sessions live in
  the same on-disk layout as active ones.

## Examples

- A user finishes a task, presses the view's `x`, and types `a`: the adapter
  stops, the row disappears from the list, and the session's transcript remains
  available. The section it was in now shows a "1 archived" row.
- The user clicks that section's "N archived" row; the archived sessions appear
  with dimmed names, selecting one shows its full history, and restarting it
  removes the dim and returns it to the section's normal (active) run.
- The daemon restarts: active sessions auto-resume, the archived one stays down
  until the user restarts it.
