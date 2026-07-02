# 0065-program-live-collaboration

Status: accepted
Date: 2026-07-01
Area: protocol
Scope: Program view collaboration across TUI and web clients.

## Decision

Program collaboration is daemon-authoritative. Clients apply local edits immediately for responsiveness, then send small anchored edits to the daemon as soon as practical. The daemon persists each accepted edit, broadcasts the updated Program document, and separately broadcasts ephemeral cursor presence for each connected client.

Cursor presence is not persisted and must clear when a client disconnects or leaves a Program. Remote cursors are advisory UI: they must never block edits, saves, runs, shimmer updates, or conflict recovery.

The daemon assigns each active Program cursor a distinct short label. Clients may suggest a system or surface name, but the daemon must suffix duplicates and generic surface names so simultaneous TUI/web clients are distinguishable.

Remote cursor rendering must not hide Program text. Surfaces that can draw between characters may render a caret between glyphs; terminal surfaces must instead style the target character cell non-destructively and render cursor labels as visually distinct tags. Cursor label tags should fit the displayed label text, not a fixed surrounding box, and should ellipsize when capped.

When the daemon accepts a Program edit, it must rebase active cursor offsets through that accepted edit before broadcasting cursor updates. The edit's source connection is excluded because its local caret is already in post-edit coordinates; agent-authored edits and other source-less edits rebase every active cursor.

The owning agent is itself a presence, not just an edit source. When an agent-authored (source-less) edit is applied, the daemon publishes an ephemeral cursor for it, positioned at the end of the last applied edit and labeled with the owning session's harness name (or a generic fallback such as "agent" when no harness name is available). This agent cursor shares the same connected-cursor map, broadcast, and rebase machinery as human cursors, and the same one-minute expiry: an agent has no live connection to cleanly disconnect, so its cursor is retired only by going idle past the cutoff, never by an explicit tombstone. Renderers must be able to tell an agent cursor apart from a human TUI/web cursor (for example by a distinct cursor-kind marker) so they can style it differently without needing a new color or label scheme per harness.

Rebasing an agent cursor through an edit it did not itself author must correct its position (so it keeps pointing at the text it wrote) without renewing its reveal-highlight freshness — unlike a human cursor, whose "last updated" stamp legitimately advances on every rebase since a human's cursor is inherently "still there" regardless of what moved it. An agent has no such standing presence between edits: its freshness must reflect only its own most recent write, or an unrelated edit landing soon after would replay the reveal over text the agent never touched. This is an intentional asymmetry between the two cursor kinds sharing one expiry field, not an inconsistency to reconcile away.

Because an agent-authored edit typically lands as one multi-character (often multi-line) change rather than a human's incremental keystrokes, adopting it should reveal with a brief, subtle highlight over the changed span rather than an instant repaint, so the change reads as observed rather than silently swapped underneath the viewer. The highlight is a rendering affordance only: it lasts on the order of a few hundred milliseconds, must compose with (not replace or hide) any selection, search-match, or run-shimmer styling already on that text, and must never alter, delay, or gate the underlying document adoption, saves, or runs.

## Reason

Multiple TUI and web clients can edit the same Program at the same time. Local-only buffering hides other users' work until save and creates avoidable conflicts. Full-document writes per keystroke are too easy to race. Anchored edits already merge independent changes against the latest Program, while cursor presence gives users immediate visibility into who is editing where.

## Consequences

Clients should prefer live anchored edits for normal typing, paste, delete, and small replacements. Whole-document update remains available for template application, explicit save fallback, and large rewrites. When a remote Program state or rebased own-cursor notification arrives, a clean or live-synced editor should adopt it immediately and remap the local caret. If a client still has unsynced local edits, it may temporarily preserve them and fall back to the existing merge-on-save path.

Future clients must filter their own cursor from remote-cursor rendering and must expire stale cursor presence defensively. A remote cursor that has published no update for over one minute must stop rendering, whether or not an explicit tombstone was received for it — a client that goes idle or drops without a clean disconnect must not leave a permanent ghost cursor. The daemon applies the same one-minute cutoff when answering a Program snapshot request, so a freshly-opened client never adopts an already-stale cursor.

## Non-Goals

This does not add durable user accounts, avatars, comments, or operational-transform history. It also does not require keystroke-level persistence for agent-authored bulk Program rewrites.

Agent presence does not add a new persisted cursor kind, a per-block "last edited by" record, or any indication of which specific edit call produced a change. It is the same ephemeral, unpersisted, advisory-only presence signal defined above, scoped to the owning agent's own writes; it must never block edits, saves, runs, shimmer updates, or conflict recovery, exactly like a human cursor. The reveal highlight is advisory rendering only and carries no new state of its own — it is derived from the same agent-cursor publish, not persisted, and not a substitute for the shimmer lifecycle (`0042`/`0048`/`0053`), which remains the signal for "still executing."
