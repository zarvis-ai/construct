# 0041-operational-markdown-canvas

Status: proposed
Date: 2026-06-25
Area: ux
Scope: Defines a document-first command surface for projects, sessions, tasks, context, and operator collaboration.

## Decision

Construct should explore an operational markdown canvas as the primary user
experience, with the operator session as a secondary collaborator rather than
the main chat surface.

The canvas is a live, editable document. Users can write prose, headings,
lists, tables, links, freeform notes, diagrams, boxes, arrows, and task lists.
Construct decorates recognized entities with smart-chip style affordances:
sessions, harnesses, files, diffs, branches, PRs, commands, context sources,
tasks, approvals, artifacts, and status.

Document structure is intentionally flexible. A heading does not always mean a
project or lane, and section labels such as `Tasks`, `Context`, `Todo`, or
`Done` are not reserved keywords. The operator reads the current document and
infers intent from visible content, layout, repeated patterns, selection, user
actions, and nearby text. If the document is ambiguous, the operator asks for
clarification or edits the document to make the intended operation explicit.

Construct should not add a hidden workflow rule engine for canvas semantics.
The visible document is the semantic contract. If behavior matters, it must be
visible in the document as text, chips, comments, diagrams, or other editable
content.

The canvas should use a document-capable editor surface, not a native textarea
as the durable UX. Markdown can remain the interchange and persistence format,
but the rendered surface must support inline smart chips, status affordances,
generated regions, comments, selection-aware actions, and eventually richer
layout primitives such as boxes or arrows.

Execution is explicit. Editing text never launches agents, mutates files,
cancels sessions, or submits prompts by itself. Actions happen through a
contextual action affordance, direct user command, accepted operator proposal,
or ordinary approval flow.

Canvas updates after execution are also explicit. When the operator creates or
delegates work to sessions, subagents, or tools, it must check the resulting
state before proposing document changes. The product may surface the proposed
replacement or patch as a pending canvas update, but the editable document is
not changed until the user accepts it or directly edits it.

## Reason

Chat is a good input channel for intent, but it is a poor primary surface for
many long-running workstreams. Work state becomes scattered across transcripts,
session lists, widgets, memory, and the user's head. A document-first surface
makes the state visible, editable, shareable, and durable.

The Google Docs smart-chip model is a better fit than a rigid markdown DSL.
Users should be able to shape the canvas around their own workflow: kanban-like
sections, release boards, ad hoc checklists, diagrams, matrices, notes, or
hybrid structures. Construct can augment that document without taking ownership
away from the user.

A textarea can prove the submission plumbing, but it cannot make chips,
operator-written status, or generated regions feel native. A document surface
lets Construct keep the content editable while giving recognized entities their
own behavior and presentation.

Keeping semantics visible avoids brittle hidden state. If Construct stored
document-local selectors such as "items under heading X form a workflow", a
simple rename or layout change could leave behavior stale or surprising. Letting
the operator reinterpret the current visible document keeps the model flexible
and keeps the document as the source of truth.

Explicit action boundaries preserve safety. A collaborative editable document
must be safe to draft, rearrange, paste into, and discuss without accidentally
starting work.

## Consequences

The canvas has two layers of state:

- The document layer contains human-visible content, smart chips, generated
  summaries, diagrams, checklists, comments, and context references.
- The runtime graph records factual execution state: sessions, tasks, running
  processes, approvals, tool calls, outputs, artifacts, status, provenance, and
  cancellation handles.

The runtime graph is not a hidden workflow semantics engine. It is operational
bookkeeping so Construct can reconnect after restart, show live status, prevent
duplicate submissions, stop active work, and preserve history even when the
source document block is deleted.

The operator interprets document semantics from the current canvas. Product code
provides primitives for rendering, selection, smart chips, action sheets,
realtime collaboration, execution, status overlays, and provenance links.

The primary action affordance should be contextual rather than sprinkled across
every recognized item. Selection wins over cursor context, cursor context wins
over nearest block, nearest block wins over section, and section wins over page.
The default action is to ask the operator about the current selection or
location. The operator may then propose concrete actions such as running a task,
creating sessions, attaching context, summarizing a section, moving completed
items, or updating status.

The operator-to-canvas return path should be visible and reviewable. In v1 this
can be a full-document replacement proposal; later versions may use structured
patches, comments, or tracked changes. The important invariant is that completed
work does not silently disappear into another transcript: the operator either
updates the canvas through an accepted proposal or explains why no update is
needed.

When Construct writes into the document, it should prefer generated regions,
comments, status chips, summaries, or clearly attributed edits. User prose
remains user-owned. The operator may suggest rewrites, but should not rewrite
arbitrary user text without a direct request or an accepted proposal.

If a user deletes content Construct wrote, the deletion is respected:

- Deleted generated summaries or status blocks disappear and are not recreated
  automatically in the same place.
- Deleting a visible task or section detaches the view from active runtime work;
  it does not implicitly cancel a running session or process.
- Detached active work remains available through activity surfaces with actions
  such as open, stop, restore, or archive.
- Deleting context chips means future submissions from that visible scope should
  not include that context unless the user adds it back.

Realtime collaboration should treat the operator as a collaborator in the same
document. Multiple humans may eventually edit the canvas while the operator
adds suggestions, status, and generated content. Conflicts should be resolved at
the document layer, while runtime events preserve what Construct actually did.

An append-only event history can include both document events and runtime
events. The edit log alone is not sufficient to represent runtime state because
process starts, stream output, approvals, errors, and completions are not merely
document edits.

## Non-Goals

This decision does not require replacing the operator session. Chat remains
useful as a secondary input, especially for broad instructions such as "organize
this page", "run the unchecked review items", or "compare these two sessions".

This decision does not define a rigid markdown grammar. Syntax can remain useful
for power users, but plain document content must remain valid and safe.

This decision does not make every recognized item executable. Recognition can
decorate, preview, explain, or suggest without producing an action.

This decision does not make deletion a cancellation command. Stopping active
work remains an explicit action.

## Examples

A user can create a loose workflow with headings:

```md
# Operator Canvas

Move items left to right:
Ideas -> Doing -> Review -> Done

## Ideas

- Smart-chip canvas for sessions and context.

## Doing

- Ask @codex to inspect the operator architecture.

## Review

## Done
```

Construct may infer this as a workflow because the visible text says how items
move. If the Codex task finishes, the operator can propose moving the item from
`Doing` to `Review` or `Done`, but the move is still a visible document edit.

A user can work through selection:

```md
- Ask @claude to design the web canvas UX.
```

With the cursor on that line, the contextual action can offer:

- Ask operator about this line.
- Run with `@claude`.
- Run fresh.
- Add selected output as context.

A user can delete generated status without losing the run:

```md
- [~] Ask @codex to inspect the operator architecture.
  Status: running for 2m.
```

If the user deletes the status line, Construct stops updating that location.
The Codex session continues unless the user explicitly stops it. The running
work remains visible in activity surfaces and can be restored into the canvas.

A diagram can be meaningful without a fixed syntax:

```text
[Backlog] -> [Doing] -> [Review] -> [Done]
```

If the user draws or writes an equivalent graph, the operator may interpret it
as a workflow because the relationship is visible. Product code does not need a
stored selector rule for the graph; the operator uses the current document.
