# 0053-program-shimmer-block-addressing

Status: accepted
Date: 2026-06-30
Area: protocol
Scope: How a program block's shimmer state is addressed and declared across program read, edit, update, and execute.

## Decision

Program-block shimmer is a declared per-block state addressed by a daemon-owned stable block reference, not by position and not by inferred text changes.

Definitions:

- A **block** is a run of non-blank Markdown lines split at heading and list-item boundaries: each heading line, each list item (with its wrapped continuation lines), and each plain paragraph is its own block.
- A **block id** identifies one block instance across moves and concurrent edits.
- A **content epoch** increments when that block instance's semantic content changes.
- A **block ref** is `block_id:content_epoch`. It is the authoritative shimmer address returned as each block's `id`/`block_ref` in program projections.
- A **content id** is the legacy normalized-content hash. It ignores smart-clip `clip_id` metadata and remains available as `content_id` for compatibility, but it is ambiguous for duplicate text and must not be preferred when block refs are available.

The daemon stores block identity in program metadata and reconciles it whenever the Markdown changes. Unchanged content keeps its ref across moves. Metadata-only smart-clip instance-id changes keep the same ref. Semantic edits keep the block id but advance the epoch, producing a new ref so stale shimmer declarations do not attach to changed meaning. New blocks receive new block ids.

Shimmer is carried across the program surfaces as follows:

- **Read** returns an ordered projection of the document: each block with its stable ref (`id`/`block_ref`), legacy `content_id`, text or source line range, and current `shimmer` boolean.
- **Edit** accepts an optional and partial list of `{id, shimmer}` declarations. The id may be a stable ref or a legacy content id. Declarations resolve against the document the call produces; a declaration whose id matches no block is dropped.
- **Update** accepts a complete shimmer declaration over the blocks of the new Markdown. The daemon maps that ordered declaration to the new stable refs.
- **Execute** lights the executed region optimistically immediately. The daemon then publishes stable refs for the active run so clients can replace the optimistic projection as soon as possible. For a selection Run, the client should tell the daemon which real document blocks its selection overlaps — computed by checking containment of the selection's range against each block's line range, not by re-parsing the raw selected text into its own standalone document and hash-matching the result. The daemon trusts that client-supplied identity when given. Hash-matching a re-parsed selection is a legacy fallback for callers that omit it; it only identifies the right block when the selection spans one or more whole blocks exactly, because a strict substring of a single block's text hashes differently from that block's real (full) content.

Because shimmer is keyed by block ref, a block's prior shimmer does not carry across a semantic edit by default. Agents that edit a still-in-flight block must set `keep_pending: true` on that edit, or explicitly declare the resulting block's new ref pending in the same call. `keep_pending` is preferred because it re-adds the produced ref atomically, before any intermediate empty pending set can make the UI go dark.

Every write returns the fresh per-block projection. Agents should read once, then use the echoed blocks from each write.

## Reason

Users can edit Program at any time and run again while an older run is still active. A robust shimmer model must therefore provide both immediate affordance and precise visibility without trusting positions or stale text.

Content-derived ids were better than indexes because they failed closed under many races, but they had two serious gaps: identical blocks shared one shimmer state, and semantic edits had no stable object to attach an explicit "same task, new text" transition to. A daemon-owned block id plus epoch solves both. Duplicate blocks get distinct refs. Moves preserve refs. Semantic edits intentionally advance the ref, so old shimmer cannot accidentally stick to new meaning. Smart-clip `clip_id` repair is treated as metadata and does not advance the epoch, so UI normalization does not settle real work.

The model remains plain-Markdown friendly: identity lives in program metadata, not hidden Markdown markers, and content ids remain as a compatibility fallback for older clients and transient dirty-buffer projections.

## Consequences

- Program get, edit, update, execute, and state payloads publish the per-block projection so clients do not independently invent block identity.
- Clients should use stable refs whenever the local buffer matches the saved daemon document. They may fall back to content ids only for legacy payloads or dirty optimistic buffers.
- A stale stable-ref declaration fails closed: it matches no block if the target's semantic content changed and the caller did not use `keep_pending`.
- Identical-content blocks are distinct when addressed by stable ref. Legacy content-id declarations may still affect all matching duplicates and should be treated as compatibility behavior.
- No write changes the shimmer of a stable-ref-addressed block it did not name, except `keep_pending`, which names blocks by the edit output instead of by a ref the caller cannot know yet.
- A run is not destroyed by a transient empty pending set mid-turn. Text-changing edits can drop old refs before new refs are added; the run survives that gap and is reaped by the lifecycle defined in `0042`.
- Smart-clip `clip_id` normalization is shimmer-neutral. Changing only instance metadata leaves block refs unchanged.
- A selection Run's block identity is resolved from the client's explicit overlap-computed block ids when the client supplies them, not solely by re-parsing and hash-matching the raw selected text. A client that omits the ids falls back to hash-matching, a legacy path that cannot distinguish "the selection is this whole block" from "the selection is part of this block's text" — a strict partial-line/partial-block selection can therefore produce an identity that matches no real block in the document, and the client-supplied ids exist specifically to avoid that.

## Non-Goals

- This does not introduce task status, locks, or permanent task tracking. Shimmer remains the transient pending/settled signal defined in `0042`.
- This does not change the optimistic start or stop lifecycle of `0042`; it only defines the addressing model for the pending set.
- This does not require identity markers in Markdown.
- This does not make legacy content ids precise for duplicates. They are compatibility fallback only.

## Examples

- **Duplicate blocks.** Two identical list items have the same content id but different stable refs. Settling one by ref leaves the other shimmering.
- **Smart-clip metadata.** `@{session:s1}` becoming `@{session:s1 clip_id=clip_4}` keeps the same block ref and shimmer.
- **Semantic edit.** `* build` becoming `* build and test` advances the epoch and drops old shimmer unless the edit uses `keep_pending`.
- **Move.** Moving `* build` from Todo to In progress without changing its text keeps the same ref and its shimmer follows the block.
