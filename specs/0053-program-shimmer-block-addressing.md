# 0053-program-shimmer-block-addressing

Status: accepted
Date: 2026-06-28
Area: protocol
Scope: How a program block's shimmer state is addressed and declared across program read, edit, update, and execute.

## Decision

Program-block shimmer is a **declared per-block state addressed by a stable, content-derived block id** — not a state inferred from whether a block's text changed between reads.

Definitions:

- A **block** is a run of non-blank Markdown lines split at heading and list-item boundaries: each heading line, each list item (with its wrapped continuation lines), and each plain paragraph is its own block. A section of consecutive task items is therefore many blocks, not one — so an individual item can be declared pending or settled without disturbing its siblings, even when the items are written with no blank lines between them. (The same unit as `0042-program-run-progress-affordance`.)
- A block's **id** is derived deterministically from its normalized content (its trimmed lines joined by newline). The id is **stable against position** — reordering, insertion or removal of other blocks, structural shifts, and concurrent edits elsewhere in the document all leave it unchanged. It is **not stable against editing the block's own content** (changing the text changes the id), and **two blocks with identical normalized content share one id** and therefore one shimmer state.

Shimmer is carried across the four program surfaces as follows:

- **Read** returns an ordered, derived projection of the document: each block with its id, its text (or source line range), and its current `shimmer` boolean. The projection is computed at read time from the live Markdown overlaid with the active run's shimmer set. It is never written into the Markdown, never versioned, and recomputed on every read.
- **Edit** accepts, alongside its content edits, an **optional and partial** list of `{id, shimmer}` declarations that may address **any** block — including blocks the edit does not change. Declarations resolve against the document the call produces; a declaration whose id matches no block is dropped (the block changed underneath the caller).
- **Update** (whole-document replace) requires a **complete** shimmer declaration over the blocks of the new Markdown — every block's pending/settled state. Because the caller supplies the entire text, the block set and its order are unambiguous.
- **Execute** lights the whole executed region (the full document, or the selection) shimmering optimistically the instant it is invoked, and accepts an optional initial pending set.

Because shimmer is keyed by id, **a block's prior shimmer does not carry across a change to its own content**: once a block's text changes its id changes, and the new content is settled unless the same call re-declares its new id pending. This one rule produces both required behaviors — a human (or agent) editing a block's text settles it by default, while an agent that edits a block but means to keep working on it re-declares its new id pending in the same call.

Every write returns the fresh per-block projection in its response. A caller therefore reads once, then rides the echo from each write, and rarely acts on a stale read.

## Reason

The earlier mechanism inferred shimmer from block-content changes: a block shimmered from Run start until its text changed. That cannot express "this block is settled but its text is unchanged," so blocks that need no work — which by definition never change — stayed shimmering for the whole turn, while the blocks with real work settled as the agent rewrote them. The observed result was the exact inverse of intent: inert sections (headings, untouched rules) shimmered while the actively-worked items went calm. Making shimmer a declared per-block state removes the inference entirely: a block shimmers because it is declared pending, full stop.

Addressing by a content-derived id rather than a positional index is a deliberate safety choice. A positional index silently retargets the **wrong** block whenever blocks are inserted, removed, reordered, or edited concurrently — it fails open, corrupting a block the caller never named. A content-derived id instead matches **nothing** when its block changed underneath the caller — it fails closed: the intended update is dropped, but no other block is touched. This is the same concurrency behavior the anchored content edit already has (its find anchor is content-matched and simply fails when the targeted text changed), so shimmer addressing and content addressing behave identically under a race, and the caller's existing "re-read and retry" path covers both.

A truly stable id — one that survives editing a block's own content — is intentionally out of scope. It would require either hidden identity markers embedded in the Markdown or a reconciliation side-table mapping blocks to persistent ids across every save. Both contradict the plain-Markdown, co-editable, no-fragile-bookkeeping stance of `0042-program-run-progress-affordance`, and both are far too heavy for a transient animation that is already best-effort and cleared at turn end.

Partial declarations on edit and a complete declaration on update reflect how each surface is used. Edit is targeted and frequent; forcing every edit to restate the shimmer state of every block would invite the opposite failure — quietly settling a still-pending block the caller forgot to re-list. A partial list lets a call touch only what it means to. Update already carries the whole document, so a complete declaration is both natural and unambiguous, and being required there means a wholesale rewrite can never silently inherit a stale shimmer set.

## Consequences

- Program get and state payloads publish the per-block projection (block ids plus shimmer state) rather than an opaque list of pending block signatures. Clients animate the time-based shimmer wave locally over the blocks the daemon marks pending; they do not re-derive the block-to-shimmer join themselves.
- The optimistic client-side start in `0042` is unchanged: a Run lights its whole region immediately, before the first projection arrives; the projection then narrows it.
- Stale-id drops and identical-content collisions are accepted imprecision, bounded by the best-effort narrowing and the authoritative turn-end stop signal in `0042`. A dropped declaration self-heals on the next read or write echo, or clears at turn end.
- No write ever changes the shimmer of a block it did not name by id. A concurrent human edit to a targeted block drops that one declaration (and, for a content edit on the same block, fails the find-anchor match); the caller re-reads via the echoed projection and retries.
- Shimmer is declared two ways, by purpose. The call-level per-block **id list** declares the pending state of blocks that already exist — settling no-work blocks, marking others pending — and is the planning-pass mechanism. A per-edit **`keep_pending`** flag instead keeps the block an edit *produces* pending, addressing it by the edit rather than by a not-yet-known id; it exists because a text-changing edit (move, annotate, append a clip) re-ids its block, and requiring the post-edit id would force a two-step declare-after-edit whose intermediate state empties the pending set. `keep_pending` adds the resulting block's id in the same narrowing call that drops the old one.
- A run is not destroyed by a *transient* empty pending set mid-turn. A text-changing edit drops the old id before any new id is added, so without `keep_pending` the set momentarily empties; reaping the run there would make a follow-up re-declaration a silent no-op. The run is kept alive across the gap (nothing shimmers in it) and reaped only on a terminal/idle owning-session state or the inactivity backstop — see the stop lifecycle in `0042`.

## Non-Goals

- This does not introduce persistent or sticky block identity, a per-block lock, or a per-task status; shimmer remains the transient pending/settled signal defined in `0042` and `0048`.
- It does not change the start / stop lifecycle, the optimistic start, or the stop signals of `0042-program-run-progress-affordance`; it only defines how the pending set is addressed and declared.
- It does not attempt to keep a block's id stable across edits to its own content, nor to disambiguate two blocks with identical content.

## Examples

- **The inverse, fixed.** A program has a Rule heading, an empty TODO heading, two in-progress items, and a Done heading. The agent reads the projection, then makes one edit that declares the two item ids `shimmer: true` and the Rule, TODO, and Done ids `shimmer: false` — changing no block's text. The two items keep shimmering; the inert headings go calm at once, instead of shimmering for the rest of the turn.
- **Editing a block but keeping it pending.** The agent rewrites an item to record a hand-off. The rewrite changes the block's text, so its id changes; the same edit declares the new id `shimmer: true`, and the item keeps shimmering. When the result lands, a later edit rewrites it again and declares it settled, and it settles.
- **Concurrent human edit.** Between the agent's read and its edit, a human rewrites one block. The agent's shimmer declaration for that block's old id matches nothing and is dropped; its declarations for the other blocks still apply. The agent re-reads the echoed projection and re-declares the human-changed block if it still intends to work on it.
- **Identical blocks.** Two list items have identical text. They share one id, so they shimmer and settle together; distinguishing them would require giving them distinct text.
