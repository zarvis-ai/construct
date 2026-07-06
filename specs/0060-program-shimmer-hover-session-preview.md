# 0060-program-shimmer-hover-session-preview

Status: accepted
Date: 2026-06-30
Area: tui
Scope: Hovering a shimmering program block shows a cropped live preview of the dispatching (orchestrator) session's own terminal, captioned with the block's status tooltip; hovering a session clip chip continues to preview the session the clip points to.

## Decision

When a viewer hovers a still-running (shimmering) program block, the hover affordance is a small cropped live view of the **dispatching session's own terminal** — the orchestrator session running the program itself — captioned with the block's shimmer status tooltip (`0057`). This holds regardless of whether the block names a worker session in an `@{session:…}` clip: the shimmer-text hover always previews the dispatcher, never a session a block happens to delegate to.

Hovering a `@{session:…}` smart-clip chip is a distinct affordance and keeps previewing the session the clip actually points to (e.g. a subagent/worker session) — the same session-preview card, just anchored to the chip and targeted at the referenced session instead of the dispatcher.

- The preview shows the target session's recent terminal tail in a compact floating card, anchored near the pointer and kept inside the program surface.
- The card's caption is the block's status tooltip, or its fallback label when none is stored, so the `0057` status text is never lost by upgrading to the richer preview.
- It degrades to the bare text tooltip (`0057`) whenever a preview cannot be shown: the target session is unknown, or has produced no captured output yet.
- It persists for as long as the pointer remains over the shimmering block, exactly like the clip-chip hover — it does not self-dismiss just because the pointer has been briefly still. It disappears only when the pointer leaves the block (or the block stops shimmering).
- It opens only on pointer-enter (`0057`): a block that starts shimmering while the pointer already rests on it (e.g. a selection-Run context menu adjacent to the selection, leaving the pointer on the block the moment it starts shimmering) must not immediately reveal the card. Only a pointer that actually moves onto the block after its shimmer began opens it.
- When it opens, it anchors beside the hovered block's on-screen rows — never over them (`0057`) — the same adjacent-row placement rule the bare text tooltip follows when no preview is available.

## Reason

Shimmer tells a viewer *that* a block is running, and the tooltip tells them *what* it is doing (`0057`), but neither shows the actual work. The shimmer belongs to the program, and the program is the dispatching session's own document, so hovering the shimmer text is a window onto the dispatcher's live terminal — the session actually doing the dispatching — not a worker's. A block naming `@{session:worker}` is a link to that worker; hovering the link itself is what shows the worker's progress. Hovering the surrounding shimmering prose should not silently jump to a different session's terminal, since a viewer scanning the program's own narration expects to see the program's own session at work.

Captioning the preview with the tooltip preserves the concise agent-authored status while adding the live view, so the enhancement never removes information the prior affordance carried.

## Consequences

- The shimmer hover and the session-clip hover share one preview card (look, sizing, placement), but resolve to different target sessions: shimmer hover → the program's own dispatching session; clip hover → the clip's referenced session.
- A shimmering block whose dispatching session has no captured output yet must still show the text tooltip — the preview is an upgrade, never a replacement that can blank the affordance.
- A surface that cannot render a live terminal preview (or cannot support hover) remains free to show only the text tooltip and still satisfies `0057`.
- Because the shimmer-text preview always targets the program's own session — already warm, since it's the currently rendered program's owner — it does not need referenced worker sessions hydrated. Only the clip-chip hover, which can target any worker named anywhere in the document, needs those referenced sessions' PTY history kept warm.
- A surface needs a way to tell a genuine pointer-enter apart from content merely changing under a stationary pointer, and a way to place the card relative to the block's on-screen row span rather than only the pointer's own cell.

## Non-Goals

- It does not change how shimmer or its tooltip is declared, addressed, or cleared (`0042`, `0048`, `0053`, `0057`); it only enriches how the tooltip is surfaced on hover in a surface that can render a session preview.
- It does not make the preview persistent, pinned, or interactive; it is a transient hover affordance.
- It does not require the web program view to render the preview; cross-client parity (`0059`) may adopt or omit it.

## Examples

- **Delegated block.** A program item "Building the PR @{session:worker}" shimmers while the worker runs. Hovering its prose text (not the clip chip) shows a small card with the orchestrator's own live terminal tail, captioned "Building PR". Hovering the `@{session:worker}` chip itself instead shows the worker session's live terminal tail — the clip's own target.
- **Optimistic / no output yet.** The instant a Run starts, hovering a shimmering block shows the bare "Working…" tooltip until the dispatching session produces output, then upgrades to the live preview.
