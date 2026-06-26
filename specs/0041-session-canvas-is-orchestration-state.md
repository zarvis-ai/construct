# 0041-session-canvas-is-orchestration-state

Status: accepted
Date: 2026-06-26
Area: ux
Scope: Per-session canvas documents used to orchestrate task execution.

## Decision

Every user-facing session may have one durable canvas: a Markdown document owned by that session and editable by both humans and the underlying agent. Canvas execution routes to the owning session as an instruction to interpret the document or selected Markdown fragment as orchestration state.

Smart clips are persisted as Markdown-native typed references, using inline `@{type:id}` references for compact clips and fenced `:::clip type ... :::` blocks for larger embeds. Renderers may present these references as rich chips or blocks, but the stored document remains plain Markdown.

Canvas updates use optimistic concurrency. Writers may pass the version they read as `base_version`; if the current version differs, the daemon rejects the update and the writer must re-read, merge, and retry. Agent-originated canvas updates are trusted session actions and do not require user approval.

## Reason

Canvas is an alternative orchestration surface, not an output preview. Keeping Markdown as the source of truth makes the state inspectable, editable with ordinary tools, and resilient across clients. Routing execution through the owning session preserves the existing session/subagent model instead of creating a second workflow engine.

Optimistic concurrency is simpler than a CRDT and matches the expected conflict shape: human and agent edits can overlap, but conflicts can be resolved by the agent using the latest document plus the failed attempted change.

## Consequences

Clients must treat canvas rendering as a projection of Markdown, not as canonical state. A rich UI can show smart clips, live session status, and action affordances, but it must save back to Markdown.

The daemon owns canvas persistence, versioning, and execution routing. Agents should use canvas tools to update the document rather than writing session storage files directly.

Template selection copies Markdown into the session canvas. Templates are not live-linked after selection.

## Non-Goals

The canvas is not a general-purpose task scheduler. Execution creates a request for the owning session to act; subagents, moves between sections, and progress annotations are agent behavior layered on top of the document.

## Examples

```md
# Todo

- resolve issue #132 @{harness:codex}

# Progress

- summarize results with @{harness:claude}

# Done
```

```md
Current worker: @{session:s_123 label="issue #132"}

:::clip session-response
session="s_123"
mode="live"
:::
```
