# 0085-reset-lineage-edge

Status: accepted
Date: 2026-07-11
Area: harness, protocol, tui
Scope: A harness-native context reset (`/clear`, `/branch`, `/new`, and equivalents) becomes a third lineage edge, and the pre-reset conversation becomes a selectable, readable, forkable node in the same graph as fork/subagent edges.

## Decision

A session's `SessionSummary` gains a `resets: Vec<ContextReset>` list. Each entry is recorded the moment an adapter observes a mid-session native id change — the change spec 0079 already detects and rebinds its transcript watcher to, but previously only logged:

```
ContextReset {
    at_ms: i64,
    transcript_seq: u64,      // this session's event_count at the moment of reset
    busy_ms: u64,             // this session's busy_ms_at() at the moment of reset
    message_count: u64,       // this session's message_count at the moment of reset
    prior_native_id: String,  // the native conversation id that was retired
    new_native_id: String,    // the native conversation id that replaced it
}
```

This mirrors `ForkedFrom`/`ForkMerge`'s existing snapshot-counter pattern: taking a transcript-position snapshot at a lineage boundary and using it to carve a timeline into segments after the fact, with plain arithmetic and no extra fetch.

`crate::lineage` (the CLI's pure fork/subagent tree module) gains `LineageEdge::Reset`. Unlike `Fork`/`Subagent`, a reset edge does not connect two distinct `SessionSummary` rows — it connects a session to a frozen segment of *its own* transcript. Each entry in `resets` materializes one synthetic node (session id `"{owner}#reset{n}"`, distinct per segment so hover/selection/hit-testing never conflate two segments or a segment with the live session), chained in front of wherever the live session already sits in the tree: `root → reset-segment-1 → reset-segment-2 → ... → live node`. This chain is spliced in as a separate pass *after* the ordinary fork/subagent tree is built — the ordinary tree walk only knows how to follow `forked_from`/`parent_session_id`, and a reset isn't backed by either. The segment nodes are always closed — never "open" the way an unresolved fork is — since a reset is instantaneous and irreversible; they render with a distinct glyph (`↺`) and dimmed styling, distinguished from a discarded fork's strikethrough treatment so a reset boundary is never mistaken for an abandoned fork.

**Read**: opening a reset-segment node fetches a windowed slice of the *same construct session's own transcript* — `[previous boundary, this reset's transcript_seq)` — via the existing offset+count transcript RPC. No harness-native transcript parsing is needed: construct's own event log is untouched by a clear (only the adapter's native-file tail cursor moves), so the pre-reset conversation is already sitting in the existing transcript store. The TUI renders it in a read-only popup reusing the same chat-line formatter the live chat pane uses, so an archived segment looks the same as it did live.

**Fork**: forking a reset-segment node spawns a new session using the segment's `prior_native_id` as the resume target, through the same same-harness-fork-resume path spec 0079 already built for forking the *current* native id — parameterized on an archived id instead of the live one (`ForkedFrom.reset_native_id`).

## Reason

The daemon already detects every native id change (spec 0079) and already has a proven pattern (`ForkedFrom`/`ForkMerge`) for snapshotting transcript position at a lineage boundary and slicing a timeline after the fact. A reset is structurally closer to a fork's terminal state (irreversible, timestamped, transcript-seq-anchored) than to a wholly separate object — modeling it as a same-graph edge type, rather than a second graph, keeps one place to look for "how did this session get here" and reuses the tree-construction, rendering, and cap rules already built for fork/subagent edges instead of standing up a parallel view.

## Consequences

- All four adapters spec 0079 covers (Claude, Codex, Antigravity, Grok) turn their native-id-change detection from a log line into a persisted `SessionEvent::NativeIdChanged` emission at the same code point.
- The daemon's `handle_event` gains a `NativeIdChanged` arm (alongside `ModelChanged`/`ApprovalModeChanged`) that snapshots the session's current `transcript_seq`/`busy_ms`/`message_count` and appends a `ContextReset` — durable per-session metadata, not a transcript row.
- `crate::lineage`'s `LineageNode` carries an optional `reset: Option<ResetSegment>`, populated only for synthetic reset nodes; ordinary nodes are unaffected, and a session that was never cleared produces a byte-for-byte identical tree to before this feature existed.
- Reset chains are capped at `MAX_RESET_SEGMENTS`, separately from the fork/subagent tree's `MAX_DEPTH`/`MAX_SIBLINGS` (the splice runs after that cap already applied), collapsing older resets into a visible "+N earlier" note on the oldest segment shown rather than a silent truncation.
- `ForkedFrom` gains `reset_native_id: Option<String>` — `None` for an ordinary fork (unchanged behavior: the daemon reads the parent's current native-id file at spawn time), `Some(id)` when forking from an archived reset segment (the daemon uses it directly, skipping the file read).
- Transcript read for a reset segment is a windowed read of the existing per-session event log — no new storage format, no dependency on harness-native transcript files staying on disk.

## Non-Goals

- Does not change what a fork or subagent edge is (specs 0078, 0014 unchanged).
- Does not persist or expose harness-native transcript files directly; all reads go through construct's own transcript store.
- Does not support merging a reset segment back into the live session — a reset segment is read/fork-only, same as a discarded fork has no merge action.
- Does not retroactively backfill `resets` for clears that happened before this lands; only clears observed after adapters start emitting `NativeIdChanged` are recorded.
- Does not distinguish *which* slash command triggered a reset (`/clear` vs `/branch` vs `/new`) — adapters other than Claude can't reliably tell apart the cause of a native-id change, so `ContextReset` is cause-agnostic by design.
- Does not fix the pre-existing race between the client's fork-request-time snapshot and the daemon's spawn-time native-id-file read for *ordinary* (non-archived) forks — a separate, unrelated issue, out of scope here.
- Does not expose `reset_native_id` through the MCP `construct_fork_session` tool — TUI-only for now; the field is general enough that exposing it there later is a small, independent follow-up.

## Examples

A user runs Claude, works for a while, types `/clear`, keeps working, then `/clear`s again. The session's lineage node now shows two dimmed `↺` segments chained before the live node. Selecting the first opens a popup replaying the transcript from session start up to the first clear; selecting the second replays from the first clear to the second. Pressing `f` inside either popup forks a new Claude session resuming from that segment's `prior_native_id`, landing as an ordinary fork sibling with its own `forked_from` pointing at the live session (not at the archived segment) — a fork born from history still fits the existing fork model once it exists as a real session.
