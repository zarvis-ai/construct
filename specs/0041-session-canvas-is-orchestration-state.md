# 0041-session-canvas-is-orchestration-state

Status: accepted
Date: 2026-06-26
Area: ux
Scope: Per-session canvas documents used to orchestrate task execution.

## Decision

Every user-facing session may have one durable canvas: a Markdown document owned by that session and editable by both humans and the underlying agent. Canvas execution routes to the owning session as a submitted instruction turn to interpret the document or selected Markdown fragment as free-form instructions and state. A canvas run is autonomous orchestration work, not a one-shot status check: the owning agent should infer the user's intended objective from the document structure and prose, keep taking useful next actions while there is actionable work it can do, update the canvas with meaningful state changes or results, and not ask the human to run the canvas again while such work remains. If a run is blocked, the blocker and required external action should be recorded on the canvas before the turn ends. Execution must both deliver the prompt to the owning session *and* submit it as one turn; delivering the text without submitting is a defect. For interactive agent TUIs that enable bracketed-paste mode, execution delivers the prompt framed as a single bracketed paste and then sends a submit Enter after a short settle: the explicit paste markers let the harness buffer the whole multi-line prompt as one input (its multiline-paste guard suppresses per-line submits) and the trailing Enter — sent once the paste end marker has closed the paste — submits it. Delivering a multi-line prompt as unframed keystrokes is unreliable here: the harness's burst heuristic still treats it as a paste, but with no end marker the trailing Enter is absorbed as another newline and the prompt is never submitted. Other PTY-backed sessions receive the prompt as input followed by a newline, and non-PTY sessions receive it through the adapter input path.

Smart clips are persisted as Markdown-native typed references, using inline `@{type:target clip_id=instance}` references for compact clips and fenced `:::clip type ... :::` blocks for larger embeds. The `target` identifies the referenced session, harness, or object; `clip_id` uniquely identifies that smart clip instance within the canvas document so repeated references to the same target remain distinguishable. Renderers may present these references as rich chips or blocks, but the stored document remains plain Markdown.

Rich canvas editors should treat a typed `@` as an inline smart clip trigger. The trigger opens a cursor-anchored picker, filters as the user types, and inserts the selected Markdown-native typed reference without changing the surrounding prose.

When an inline smart clip is rendered as a chip, editor cursor movement and single-character deletion should treat the whole typed reference as one visual unit.

Canvas writes are conflict-free by construction wherever edits touch different regions, so concurrent human and agent editing does not lose work. There are two write shapes:

- **Anchored edits** are the preferred write for agents and any client making a targeted change: each edit names the existing text to replace (and its replacement), or appends. Anchored edits apply to the *latest* document, not a snapshot, so an edit lands regardless of intervening versions and merges automatically with concurrent edits elsewhere. The only failure is an anchor that is missing or ambiguous — the precise signal that the targeted text itself changed underneath the writer — and the write is rejected atomically so nothing is partially applied; the writer re-reads and retries.
- **Whole-document writes** replace the entire Markdown and carry the `base_version` they were based on. If the document advanced, the writer reconciles by 3-way merge against the latest version using the based-on content as the common ancestor: disjoint changes merge silently, and only genuinely overlapping edits surface as conflict markers in the saved document for a human or the agent to resolve. A whole-document write never blocks an interactive action (such as hiding the canvas) and never discards either side's edits.

Optimistic `base_version` checks remain the trigger that detects a moved document; they gate whole-document writes into the merge path rather than hard-failing the user. Agent-originated canvas writes are trusted session actions and do not require user approval.

Terminal clients expose canvas through a terminal-deliverable command path rather than modifier-only key gestures. Browser or desktop clients may add richer gestures, but the TUI must keep an explicit keyboard command and palette command for canvas access. That command opens a focused canvas-specific surface that renders Markdown and smart clips, accepts typing/click cursor placement, and saves through the canvas update protocol; raw external-editor workflows are secondary affordances, not the default canvas UI. Opening and closing the surface should use the same reveal/erase visual language as transient browser previews.

The session title bar should expose canvas/chat mode with a compact clickable status glyph to the left of the session title instead of a literal `<canvas>` label. The glyph toggles the surface between terminal chat and canvas mode, and its tooltip should describe the current mode, click action, and `C-x Space` shortcut.

Client-local canvas visibility is UI state and should survive normal client relaunch. A restarted TUI should reopen the same session canvases by re-reading their documents from the daemon, not by treating its prior render buffer as canonical content.

## Reason

Canvas is an alternative orchestration surface, not an output preview. Keeping Markdown as the source of truth makes the state inspectable, editable with ordinary tools, and resilient across clients. Routing execution through the owning session preserves the existing session/subagent model instead of creating a second workflow engine.

A CRDT is the wrong tool here. The agent does not emit fine-grained operations on a live shared state the way a human typist in a collaborative editor does — it reads a snapshot, thinks, and writes back. Feeding a whole-document replace into a CRDT would require diffing it back into operations, and a mechanical text merge would silently paper over edits the agent made from a now-stale understanding. Anchored edits give the agent genuine operations cheaply (the same find/replace shape it already uses for code), which makes the common case — agent and human working different regions — conflict-free without any merge at all. The 3-way merge is only a fallback for whole-document writes over a moved document, and it keeps the inspectable Markdown as the source of truth. Together they make overlapping edits rare and, when they do happen, visible rather than lost.

## Consequences

Clients must treat canvas rendering as a projection of Markdown, not as canonical state. A rich UI can show smart clips, live session status, and action affordances, but it must save back to Markdown.

The daemon owns canvas persistence, versioning, and execution routing. Agents should use canvas tools to update the document rather than writing session storage files directly.

Clients may persist which session canvases were open, but document contents stay daemon-owned. Clean client shutdown should flush dirty human edits before recording the open state.

A client that lets a human edit the whole document must reconcile a moved document by merging rather than hard-failing the save, and should adopt daemon-broadcast canvas updates live when the human has no unsaved edits so its tracked version stays fresh. Hard-failing on a `base_version` conflict regresses the no-lost-edits guarantee. Clients that only make targeted changes should prefer anchored edits and inherit conflict-freedom for free.

Template selection copies Markdown into the session canvas. Templates are not live-linked after selection.

Terminal shortcuts must avoid bindings that are easy to confuse with quit or interrupt chords.

Closing the canvas is reserved to the same affordance that opens it: the canvas command / C-x Space and the title-glyph toggle. No incidental gesture closes it. In particular, Esc is not a canvas-hide affordance (it only cancels transient in-canvas pickers such as the smart-clip picker), and clicking outside the canvas — including in the session list — never closes it. Because the canvas is per-session, selecting another session (by list click, pane focus, or keyboard navigation) makes the visible canvas follow the new selection: the outgoing session's canvas is stashed and preserved, the incoming session's canvas is revealed if it has one open. A stashed canvas is restored on return and only ever discarded by the explicit close affordance, so a reflexive click or keystroke can never destroy canvas content or lose unsaved edits.

## Non-Goals

The canvas is not a general-purpose task scheduler. Execution creates a request for the owning session to act; subagents, moves between sections, and progress annotations are agent behavior layered on top of the document.

## Examples

```md
# Todo

- resolve issue #132 @{harness:codex clip_id=clip_1}

# Progress

- summarize results with @{harness:claude clip_id=clip_2}

# Done
```

```md
Current worker: @{session:s_123 clip_id=clip_3 label="issue #132"}

:::clip session-response
session="s_123"
mode="live"
:::
```
