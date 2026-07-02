# 0057-program-shimmer-tooltip

Status: accepted
Date: 2026-07-02
Area: protocol
Scope: Program-block shimmer carries a required, concise agent status tooltip, with a daemon-derived fallback that renderers surface on hover.

## Decision

A program block's shimmer state carries an agent-authored **tooltip**: a concise, ≤10-word string describing why that block is still running (e.g. "Building PR", "Waiting on CI").

- **Setting shimmer requires a tooltip.** When an agent declares a block `shimmer: true`, it MUST also supply a `tooltip`. The requirement is enforced at the agent-facing call boundary (the program-edit and program-update MCP tools): a pending declaration with a missing or empty tooltip is rejected with a clear error. A tooltip is **not** needed for `shimmer: false` (settling) and is ignored there.
- **The tooltip travels with the shimmer state across the program surfaces** (building on `0053-program-shimmer-block-addressing`):
  - The per-block shimmer declaration unit gains a `tooltip` (the edit surface's `{id, shimmer}` entries; the update surface's complete declaration carries a parallel per-block tooltip array in document order).
  - The daemon stores each pending block's tooltip alongside its id in the active run, keyed by stable block ref when available and by legacy content id only for fallback. Settling a block, or dropping it from the pending set, drops its tooltip.
  - The per-block projection returned by program get/edit/update (the `blocks` array) reports each block's tooltip, so any renderer reads it without re-deriving it.
- **The ≤10-word guidance is enforced by graceful normalization, not rejection.** A stored tooltip is trimmed, internal whitespace collapsed, and truncated to at most ten words (with an ellipsis when truncated). An over-long tooltip is never a hard error.
- **A block that shimmers without a stored agent tooltip renders a daemon-derived system status when available, then a hardcoded fallback** (e.g. "Working…"). The active run projection carries an optional run-level system status derived from daemon facts: whether Run was delivered while the owning session was already mid-turn, whether the program turn has been seen running, and whether any first agent-visible output has arrived. Renderers use this fallback order per block: agent tooltip → system status (with elapsed time when the surface can compute it from the run start) → hardcoded fallback. The system labels are plain and functional, such as "Queued behind current turn", "Delivered, waiting for agent", and "Agent working, no status yet". This covers optimistic client-side shimmer (created before any daemon echo or agent tooltip exists), legacy/in-flight run state, and a block kept pending across a text-changing edit (`keep_pending`), whose new id carries no tooltip until the agent re-declares it.
- **Renderers show the tooltip on hover.** Wherever a surface renders shimmering program blocks and hover is feasible, hovering a shimmering block shows its tooltip (or the fallback). A surface that cannot support hover is exempt.

## Reason

Shimmer (`0042`, `0048`, `0053`) tells a viewer *that* a block is still being worked, but not *what* is happening. A run with several pending blocks is opaque: the animation is uniform, so a human watching the program cannot tell which block is building a PR versus waiting on CI versus blocked. A concise per-block status string closes that gap with the smallest possible affordance — a hover tooltip — without turning shimmer into a progress bar or a persistent per-task status field.

Requiring the agent tooltip at the moment shimmer is set (rather than making it optional) is what guarantees the block-specific affordance is present after the planning pass. The agent already performs a planning pass that declares the pending set (`0048`); attaching a one-line status to each pending declaration is a marginal cost there and keeps the status truthful and current, because it is re-declared whenever the block's pending state is.

Before that planning pass, the daemon still knows useful run-level facts that no agent-authored text can truthfully provide yet: the prompt may be queued behind the current turn, delivered but not yet picked up, or producing first output before any program declaration has supplied per-block status. Exposing those facts as a system fallback keeps the silence between Run and the agent's first program write informative without weakening the agent contract.

Enforcement lives at the agent-facing call boundary, while every downstream layer stays tolerant of a missing tooltip. This split is deliberate: new calls are held to the contract, but no block can ever fail to render because a tooltip is absent — optimistic shimmer, legacy runs, and the transient `keep_pending` re-id all legitimately lack one and must degrade to a fallback label, never to a broken or blank render. Graceful truncation rather than rejection follows the same robustness stance: the ≤10-word rule is guidance that shapes the stored value, not a validation gate that can fail a write.

## Consequences

- The shimmer declaration types and the per-block projection gain an optional tooltip field; the active-run model stores a per-block tooltip map kept in sync with the pending set. The active-run projection also gains an optional run-level system status. These are additive and default-empty, so older payloads and in-flight runs deserialize unchanged and simply present no agent tooltip or system status (the hardcoded fallback covers them).
- Agent callers that set `shimmer: true` without a tooltip now fail the call. The agent-facing tool descriptions and run instructions must state the requirement so the planning pass supplies tooltips from the start.
- The clearing path and non-shimmer behavior are unchanged: settling a block needs no tooltip, and a block with no shimmer has no tooltip.
- A renderer must treat "shimmering with no stored tooltip" as a normal state and substitute the system status or hardcoded fallback, never as an error or an empty label.
- Two blocks with identical normalized content share one id (`0053`) and therefore one tooltip, just as they share one shimmer state.

## Non-Goals

- This does not make the tooltip a persistent or per-task status, a lock, or a progress indicator; it remains a facet of the transient pending/settled shimmer defined in `0042`/`0048`/`0053`, cleared with it at turn end.
- It does not change how shimmer is addressed, declared, started, narrowed, or stopped (`0053`, `0042`); it only attaches a status string to a pending declaration.
- It does not require a tooltip to be unique, stable across edits, or carried forward when a block's content (and id) changes; a re-id falls back until re-declared.
- It does not mandate hover on a surface that cannot support it.

## Examples

- **Required on set.** An agent's planning pass declares two items pending with tooltips ("Building PR", "Waiting on CI") and the inert headings settled. Hovering either item shows its status; the headings show nothing. A planning pass that marks an item pending with no tooltip is rejected, so the agent always supplies one.
- **Optimistic/system fallback.** The instant a Run is invoked, the whole region shimmers optimistically before any daemon echo or agent tooltip exists; hovering a block may show "Working…". Once the daemon echoes the active run, hovering shows a system status such as "Delivered, waiting for agent — 8s" or "Queued behind current turn — 2m 10s". Once the planning pass lands, hovering shows the real per-block status.
- **Kept pending across an edit.** An agent moves a still-in-flight item into an In progress section with `keep_pending`. The moved block's id changes, so it carries no tooltip and hovering shows "Working…" until the agent re-declares the new id with a tooltip.
- **Truncation.** An agent supplies a fifteen-word tooltip; it is stored truncated to its first ten words with an ellipsis rather than rejected.
