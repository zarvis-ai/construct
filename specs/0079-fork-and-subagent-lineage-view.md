# 0079-fork-and-subagent-lineage-view

Status: superseded by spec 0080
Date: 2026-07-09
Area: tui
Scope: The `C-x q` / `q` popup renders a session's full fork lineage and
subagent tree as one live graph, instead of a flat fork count.

> **Superseded.** The global `C-x q` / `q` popup described in this document
> was deleted — there is no longer a second, screen-centered lineage
> dialog. Its tree-construction rules (fork vs. subagent edges, depth/
> breadth capping, merge/discard reuse) and its full keyboard vocabulary
> (`j`/`k`/arrows/`C-n`/`C-p` navigate, `Enter` jumps in, `m`/`d`
> merge/discard, `Esc` backs out) all still apply, unchanged — they simply
> moved onto the per-session lineage preview (spec 0080), which can now be
> given keyboard focus (`C-x Tab`, or a click inside its body) instead of a
> separate modal owning them. The rest of this document is kept as a
> historical record of the interaction design that was ported; read spec
> 0080 for the current architecture.

## Decision

The fork log popup is a live tree/graph view, not a flat list. It renders
**both** relationships a session can have with other sessions in one graph,
distinguished by edge style:

- **Fork edges** (`⑂`, dashed/lighter): mergeable siblings via
  `forked_from` (spec 0078). Forking is never a parent/child relationship —
  the fork is a normal top-level session that happens to carry lineage.
- **Subagent edges** (`▸`, solid): true parent/child helpers via
  `parent_session_id` (spec 0014). These are a different relationship and
  must never be conflated with fork lineage — a subagent is not mergeable
  and a fork is not archived-as-a-child.

The view opens rooted at the *topmost* ancestor reachable from the
currently selected session by walking both edge types upward, then renders
the full tree downward from there — so opening the view from any node in a
fork chain or subagent tree shows the same graph. Rendering is
`git log --graph`-style: a compact vertical/diagonal rail with a glyph at
each branch point, not a fixed two-column diagram or spelled-out labels —
arbitrary depth and multiple siblings per level are expected (a forked
session can itself fork; a session can have several forks and several
subagents at once).

A fork's terminal state is part of the graph, not a separate view:

- **Merged** (`ForkMergeMode::Result`): rendered closing back into the
  parent's column at the merge point. Jumping to a merged fork's node
  navigates to its *parent* instead of the archived fork itself — the merge
  point in the graph and the transcript message the merge injected into the
  parent are the same event (spec 0078), so the view links to where that
  event actually lives rather than duplicating it.
- **Discarded** (`ForkMergeMode::Discard`): rendered dimmed and
  struck-through, unambiguous against a fork that's merged or still
  running.
- **Open** (no merge outcome yet): rendered normally, and is the only state
  `m` (merge) / `d` (discard) act on.

The tree is capped at a bounded depth and a bounded number of siblings per
level; anything beyond the cap collapses into a single "+N more" row rather
than growing the popup unboundedly.

Each node shows a status icon, harness name, and compact live stats
(message/turn count, elapsed time, cost when the daemon has attributed
any) — sourced entirely from fields already present on the session
summary. While the popup is open, an interval refresh keeps those stats
current; nothing polls when the popup is closed.

Tree construction and `git log --graph`-style layout are a self-contained,
`App`-independent module (pure functions over session summaries in, rows
out) so the same logic can back a future pinnable/dockable panel without a
rewrite — the popup itself is a thin adapter wiring that module to live
session data, keyboard navigation, and the existing merge/discard action.
Merge and discard reuse the exact code path the `C-x m` minibuffer menu
already uses; the popup's `m`/`d` keys are a direct-key shortcut for it,
not a second implementation.

Keys the popup doesn't handle close it and re-dispatch the same keystroke
through ordinary routing, the same rule the `/configure` dialog follows —
an open popup never permanently deadens a chord the user reaches for out
of muscle memory.

## Reason

Fork lineage and subagent parenting are both "this session relates to that
one" facts the fleet already tracks, but they answer different questions
(which sibling can I merge back? vs. which helper did I delegate to?) and
the codebase treats them as intentionally distinct primitives (spec 0078
vs. spec 0014). A view that shows only one of them forces the user to
mentally overlay the other from memory. A single graph with distinguishable
edge styles answers both questions at a glance without conflating the two
relationships' different lifecycles (mergeable vs. archived-as-a-child).

The prior fork log was a status-line fork count with no navigation — it
could tell you *how many* forks existed but not their shape, state, or a
way to act on them. As soon as a fork forks again, or a session collects
both subagents and forks, a flat list stops being able to represent the
structure at all.

## Consequences

- Any future client rendering fork lineage or subagent trees can reuse the
  same tree-construction module rather than re-deriving parent/child
  relationships from `forked_from` and `parent_session_id` independently.
- A merged fork's row and the transcript message its merge injected into
  the parent are treated as one event with two views (graph node, transcript
  line), never as two independently-maintained records.
- The popup depends only on fields already present on the session summary
  (state, harness, `event_count`, `created_at`, `cost_usd`, `forked_from`,
  `parent_session_id`, `merge`) — it does not require new daemon-side
  aggregation (e.g. no per-session token totals exist yet, so the view
  omits that stat rather than inventing a new persisted field for it).
- Depth/breadth capping means an extreme fork/subagent tree stays a bounded
  render cost; discovering what's beyond the cap requires drilling in
  (selecting a node and reopening the view rooted there), not scrolling an
  unbounded list.

## Non-Goals

This does not change what a fork or a subagent *is* (specs 0078 and 0014
still govern those relationships), does not add a new merge/discard code
path, and does not yet promote the tree view to a pinnable/dockable panel —
the reusable module makes that possible later without being built now.

## Examples

Selecting a session that was itself forked from another session, which in
turn has two subagents and one still-open fork, and pressing `q` opens the
view rooted at the original ancestor: the ancestor's node shows both the
subagents (`▸`) and the fork chain (`⑂`) branching off it, with the
forked-from-a-fork session nested one level deeper. Pressing `m` on the
open fork merges it exactly as `C-x m` would, and its row immediately
renders as closed back into its parent's column.
