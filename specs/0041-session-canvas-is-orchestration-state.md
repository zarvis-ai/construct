# 0041-session-canvas-is-orchestration-state

Status: accepted
Date: 2026-06-26
Area: ux
Scope: Per-session canvas documents used to orchestrate task execution.

## Decision

Every user-facing session may have one durable canvas: a Markdown document owned by that session and editable by both humans and the underlying agent. Canvas execution routes to the owning session as a submitted instruction turn to interpret the document or selected Markdown fragment as orchestration state. Execution must both deliver the prompt to the owning session *and* submit it as one turn; delivering the text without submitting is a defect. For interactive agent TUIs that enable bracketed-paste mode, execution delivers the prompt framed as a single bracketed paste and then sends a submit Enter after a short settle: the explicit paste markers let the harness buffer the whole multi-line prompt as one input (its multiline-paste guard suppresses per-line submits) and the trailing Enter — sent once the paste end marker has closed the paste — submits it. Delivering a multi-line prompt as unframed keystrokes is unreliable here: the harness's burst heuristic still treats it as a paste, but with no end marker the trailing Enter is absorbed as another newline and the prompt is never submitted. Other PTY-backed sessions receive the prompt as input followed by a newline, and non-PTY sessions receive it through the adapter input path.

Smart clips are persisted as Markdown-native typed references, using inline `@{type:target clip_id=instance}` references for compact clips and fenced `:::clip type ... :::` blocks for larger embeds. The `target` identifies the referenced session, harness, or object; `clip_id` uniquely identifies that smart clip instance within the canvas document so repeated references to the same target remain distinguishable. Renderers may present these references as rich chips or blocks, but the stored document remains plain Markdown.

Rich canvas editors should treat a typed `@` as an inline smart clip trigger. The trigger opens a cursor-anchored picker, filters as the user types, and inserts the selected Markdown-native typed reference without changing the surrounding prose.

When an inline smart clip is rendered as a chip, editor cursor movement and single-character deletion should treat the whole typed reference as one visual unit.

Canvas updates use optimistic concurrency. Writers may pass the version they read as `base_version`; if the current version differs, the daemon rejects the update and the writer must re-read, merge, and retry. Agent-originated canvas updates are trusted session actions and do not require user approval.

Terminal clients expose canvas through a terminal-deliverable command path rather than modifier-only key gestures. Browser or desktop clients may add richer gestures, but the TUI must keep an explicit keyboard command and palette command for canvas access. That command opens a focused canvas-specific surface that renders Markdown and smart clips, accepts typing/click cursor placement, and saves through the canvas update protocol; raw external-editor workflows are secondary affordances, not the default canvas UI. Opening and closing the surface should use the same reveal/erase visual language as transient browser previews.

The session title bar should expose canvas/chat mode with a compact clickable status glyph to the left of the session title instead of a literal `<canvas>` label. The glyph toggles the surface between terminal chat and canvas mode, and its tooltip should describe the current mode, click action, and `C-x Space` shortcut.

Client-local canvas visibility is UI state and should survive normal client relaunch. A restarted TUI should reopen the same session canvases by re-reading their documents from the daemon, not by treating its prior render buffer as canonical content.

## Reason

Canvas is an alternative orchestration surface, not an output preview. Keeping Markdown as the source of truth makes the state inspectable, editable with ordinary tools, and resilient across clients. Routing execution through the owning session preserves the existing session/subagent model instead of creating a second workflow engine.

Optimistic concurrency is simpler than a CRDT and matches the expected conflict shape: human and agent edits can overlap, but conflicts can be resolved by the agent using the latest document plus the failed attempted change.

## Consequences

Clients must treat canvas rendering as a projection of Markdown, not as canonical state. A rich UI can show smart clips, live session status, and action affordances, but it must save back to Markdown.

The daemon owns canvas persistence, versioning, and execution routing. Agents should use canvas tools to update the document rather than writing session storage files directly.

Clients may persist which session canvases were open, but document contents stay daemon-owned. Clean client shutdown should flush dirty human edits before recording the open state.

Template selection copies Markdown into the session canvas. Templates are not live-linked after selection.

Terminal shortcuts must avoid bindings that are easy to confuse with quit or interrupt chords.

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
