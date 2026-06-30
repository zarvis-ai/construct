# 0060-program-shimmer-hover-session-preview

Status: accepted
Date: 2026-06-29
Area: tui
Scope: Hovering a shimmering program block that names a session shows a cropped live preview of that session's terminal, captioned with the block's status tooltip.

## Decision

When a viewer hovers a still-running (shimmering) program block whose content references a session (its first session smart-clip), the hover affordance is a small cropped live view of that session's terminal output — the same session-preview card already shown when hovering a session clip chip — captioned with the block's shimmer status tooltip (`0057`).

- The preview shows the referenced session's recent terminal tail in a compact floating card, anchored near the pointer and kept inside the program surface.
- The card's caption is the block's status tooltip, or its fallback label when none is stored, so the `0057` status text is never lost by upgrading to the richer preview.
- It degrades to the bare text tooltip (`0057`) whenever a preview cannot be shown: the block names no session, the named session is unknown, or that session has produced no captured output yet.
- Like the other transient program hover affordances, it appears only while the pointer is actively moving and self-dismisses after the pointer has been still briefly.

## Reason

Shimmer tells a viewer *that* a block is running, and the tooltip tells them *what* it is doing (`0057`), but neither shows the actual work. When a block delegates to a session, that session's live terminal is the ground truth of its progress. Surfacing it on hover — reusing the session-preview card already shown for session clip chips — turns the program from a static status list into a window onto the running fleet, at the cost of a single hover, without making shimmer a persistent pane or a progress bar.

Captioning the preview with the tooltip preserves the concise agent-authored status while adding the live view, so the enhancement never removes information the prior affordance carried.

## Consequences

- The shimmer hover and the session-clip hover share one preview card, so their look, sizing, and placement must stay unified; a change to one is a change to both.
- A shimmering block that names no session, or whose session has no captured output, must still show the text tooltip — the preview is an upgrade, never a replacement that can blank the affordance.
- A surface that cannot render a live terminal preview (or cannot support hover) remains free to show only the text tooltip and still satisfies `0057`.
- A referenced worker session is usually not the one being viewed — it is neither selected, pinned, nor the orchestrator. For the preview to be ready the instant the pointer lands, a surface that renders a visible program must keep the terminal history of the sessions that program references warm, not only the history of the sessions it is currently displaying. A surface that hydrates session history lazily must extend that hydration to program-referenced sessions, or the preview silently degrades to the bare text tooltip.

## Non-Goals

- It does not change how shimmer or its tooltip is declared, addressed, or cleared (`0042`, `0048`, `0053`, `0057`); it only enriches how the tooltip is surfaced on hover in a surface that can render a session preview.
- It does not make the preview persistent, pinned, or interactive; it is a transient hover affordance.
- It does not require the web program view to render the preview; cross-client parity (`0059`) may adopt or omit it.

## Examples

- **Delegated block.** A program item "Building the PR @{session:worker}" shimmers while the worker runs. Hovering its text shows a small card with the worker session's live terminal tail, captioned "Building PR". Hovering elsewhere on a non-delegating shimmering block shows the plain "Building PR" tooltip.
- **Optimistic / no output yet.** The instant a Run starts, a delegating block shimmers but the worker has emitted nothing; hovering shows the bare "Working…" tooltip until the session produces output, then upgrades to the live preview.
