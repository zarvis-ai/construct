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

## Reason

Multiple TUI and web clients can edit the same Program at the same time. Local-only buffering hides other users' work until save and creates avoidable conflicts. Full-document writes per keystroke are too easy to race. Anchored edits already merge independent changes against the latest Program, while cursor presence gives users immediate visibility into who is editing where.

## Consequences

Clients should prefer live anchored edits for normal typing, paste, delete, and small replacements. Whole-document update remains available for template application, explicit save fallback, and large rewrites. When a remote Program state or rebased own-cursor notification arrives, a clean or live-synced editor should adopt it immediately and remap the local caret. If a client still has unsynced local edits, it may temporarily preserve them and fall back to the existing merge-on-save path.

Future clients must filter their own cursor from remote-cursor rendering and must expire stale cursor presence defensively. A remote cursor that has published no update for over one minute must stop rendering, whether or not an explicit tombstone was received for it — a client that goes idle or drops without a clean disconnect must not leave a permanent ghost cursor. The daemon applies the same one-minute cutoff when answering a Program snapshot request, so a freshly-opened client never adopts an already-stale cursor.

## Non-Goals

This does not add durable user accounts, avatars, comments, or operational-transform history. It also does not require keystroke-level persistence for agent-authored bulk Program rewrites.
