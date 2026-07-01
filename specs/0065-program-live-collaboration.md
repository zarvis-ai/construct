# 0065-program-live-collaboration

Status: accepted
Date: 2026-07-01
Area: protocol
Scope: Program view collaboration across TUI and web clients.

## Decision

Program collaboration is daemon-authoritative. Clients apply local edits immediately for responsiveness, then send small anchored edits to the daemon as soon as practical. The daemon persists each accepted edit, broadcasts the updated Program document, and separately broadcasts ephemeral cursor presence for each connected client.

Cursor presence is not persisted and must clear when a client disconnects or leaves a Program. Remote cursors are advisory UI: they must never block edits, saves, runs, shimmer updates, or conflict recovery.

The daemon assigns each active Program cursor a distinct short label. Clients may suggest a system or surface name, but the daemon must suffix duplicates and generic surface names so simultaneous TUI/web clients are distinguishable.

When the daemon accepts a Program edit, it must rebase active cursor offsets through that accepted edit before broadcasting cursor updates. The edit's source connection is excluded because its local caret is already in post-edit coordinates; agent-authored edits and other source-less edits rebase every active cursor.

## Reason

Multiple TUI and web clients can edit the same Program at the same time. Local-only buffering hides other users' work until save and creates avoidable conflicts. Full-document writes per keystroke are too easy to race. Anchored edits already merge independent changes against the latest Program, while cursor presence gives users immediate visibility into who is editing where.

## Consequences

Clients should prefer live anchored edits for normal typing, paste, delete, and small replacements. Whole-document update remains available for template application, explicit save fallback, and large rewrites. When a remote Program state or rebased own-cursor notification arrives, a clean or live-synced editor should adopt it immediately and remap the local caret. If a client still has unsynced local edits, it may temporarily preserve them and fall back to the existing merge-on-save path.

Future clients must filter their own cursor from remote-cursor rendering and must expire stale cursor presence defensively.

## Non-Goals

This does not add durable user accounts, avatars, comments, or operational-transform history. It also does not require keystroke-level persistence for agent-authored bulk Program rewrites.
