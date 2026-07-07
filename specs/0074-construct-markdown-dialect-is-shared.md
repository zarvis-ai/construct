# 0074-construct-markdown-dialect-is-shared

Status: accepted
Date: 2026-07-07
Area: ux
Scope: One construct-flavored Markdown dialect shared by all session Markdown surfaces, with one renderer per client.

## Decision

There is a single construct-flavored Markdown dialect: standard Markdown plus construct extensions. Extensions include typed inline references (smart clips such as `@{type:target clip_id=...}`), fenced `:::clip` embed blocks, `agentd:action/...` action links, and display extensions such as timeline blocks — plus any extension added later.

The dialect is shared across surfaces:

- Every session Markdown surface (program and widgets; memory where clients render it) accepts the full dialect. An extension is defined once and is available on every surface unless a surface explicitly restricts it for a stated, recorded reason.
- Each client implements one renderer for the dialect and reuses it across surfaces. Per-surface differences are layout and interaction mechanics (per 0004-widget-renderers-own-interaction), never dialect divergence: the same source text means the same thing everywhere.
- One extension registry is the single source of truth for both agent-facing guidance and client rendering. Smart-clip descriptors and widget Markdown extensions converge into this registry; adding or changing an extension updates what agents are taught and what clients render in one place, for all surfaces at once.

Extension semantics are surface-independent; execution semantics stay surface-bound:

- A session chip, a live clip, a timeline, or an action link renders and resolves the same way in a program and in a widget.
- Action links express user intent wherever they appear, subject to normal approval policy. Program execution interprets only the program document as instructions (per 0073-session-markdown-surfaces-have-distinct-roles); running a program does not activate its action links, and widget content is never runnable.

Surfaces compose by reference, not by merging:

- A widget may embed a live projection of a program region (a clip targeting the session's program, addressed by section), so status display has one source of truth instead of a maintained copy.
- A program may reference a widget or another session the same way it references sessions today.
- A projection is read-only at the point of display; edits happen at the source through that surface's own write path.

Stored documents remain plain Markdown, and the dialect must degrade readably: a document viewed in a generic Markdown renderer (a code host, an editor preview) stays legible, with extensions appearing as inert text rather than corrupting the prose around them.

An editable surface renders the dialect conservatively. The program surface is an editor over source Markdown, so it must preserve the mapping between what is shown and what is stored: display extensions style lines in place rather than collapsing or inserting visual structure, action links render as atomic interactive elements that serialize back to their exact source text, and bare action-link keyboard shortcuts stay inactive because an editor is a typing surface. A rendered (non-editing) surface, such as a widget or a projection, uses full display rendering. Both are renderings of the same dialect with the same meaning; only the fidelity of decoration differs, per the interaction-mechanics split this spec inherits.

## Reason

Extensions grew up siloed — smart clips on the program, action links and timelines on widgets. That silo has three costs: each client duplicates rendering work per surface; agents and users hit arbitrary capability gaps (a live session chip is expressible in a program but not in the status widget describing the same work); and the gaps create pressure to merge the surfaces themselves, which 0073 rejects for good reasons. Sharing the dialect removes the duplication and the gaps while leaving each surface's role, write model, and lifecycle untouched — consolidation at the layer where the surfaces genuinely are the same.

A single registry exists for the same reason the program run contract already generates its smart-clip reference from registered descriptors rather than a separately maintained list: two sources of truth drift, and drift here means agents writing syntax clients cannot render.

## Consequences

- New extension work targets the dialect and its registry, not one surface. "Only widgets need this" still goes through the shared registry, with the restriction recorded there.
- Clients must not fork per-surface dialect behavior. A renderer bug fix or extension upgrade lands everywhere at once.
- Clip resolution must work in widget context, not only in program context: the daemon resolves references relative to the owning session regardless of which surface contains them.
- Widget guidance to agents may include the full dialect, including references to sessions and program regions; program guidance may include action links and display extensions.
- Projections shift some duplication cost to liveness: a widget projecting a program region must update when the program changes, through the same update channel that already keeps that surface fresh.

## Non-Goals

- Does not merge program, widgets, or memory into one surface; roles and boundaries are governed by 0073-session-markdown-surfaces-have-distinct-roles.
- Does not require pixel parity across clients or surfaces; equivalent semantics matter, identical presentation does not.
- Does not define transport, persistence, or versioning of any surface.
- Does not require memory files to adopt extensions; memory stays useful as plain prose, and graceful degradation is the ceiling of what is asked of it.

## Examples

- A program's "Progress" section uses a timeline block — the same syntax an agent already uses in a status widget — instead of hand-drawn checkbox prose.
- A status widget contains `@{session:s_123 clip_id=clip_1}` and renders the same live chip the program would, showing the subagent's state without the agent rewriting the widget on every status change.
- A widget embeds the program's "Progress" section as a clip block; the human glances at the widget popover, and it is always exactly what the program says, because there is only one copy.
- A program contains `[Re-run checks](agentd:action/run-checks)`. In the program surface it renders as a clickable affordance; clicking it is user intent. Executing the program does not trigger it.
