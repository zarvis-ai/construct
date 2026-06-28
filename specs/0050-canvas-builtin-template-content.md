# 0050-canvas-builtin-template-content

Status: accepted
Date: 2026-06-28
Area: ux
Scope: What the built-in canvas templates contain and the constraints their Markdown must respect.

## Decision

Built-in canvas templates are scaffolds that both structure a workflow and teach the canvas's capabilities. Each built-in template includes a short, human-facing orientation that explains how running the canvas dispatches work to the owning session and its subagents, and demonstrates smart clips. Sectioned templates give each section a one-line description of what belongs in it.

Template Markdown may only contain smart clips that resolve, and must not contain illustrative or placeholder clips that would render as dangling chips. In practice that means harness clips (which always resolve to a harness) are fine to embed as live examples, while a concrete session reference cannot be baked into a static template because the session does not exist yet. Session embeds and fenced `:::clip` blocks are therefore described in prose ("type @ to embed a live session") rather than shown as literal syntax.

The built-in set is Blank (empty), Tasks (a Todo / Progress / Done board), and Investigation (Question / Context / Plan / Findings / Done).

## Reason

The empty-state placeholder surfaces these templates as one-click starting points, so they are many users' first contact with the canvas. A bare set of headings does not convey that the canvas is an execution surface or that smart clips exist. A small amount of in-document guidance turns each template into onboarding without a separate tutorial. Because canvas execution feeds the document prose to the owning agent, the guidance also orients the agent, while the canonical smart-clip syntax is still injected by the run-context tool rather than relied upon from the template.

The clip constraint exists because canvas rendering scans for clip syntax everywhere, including inside code fences and inline code — there is no "raw" region. A literal example clip with a non-existent target would render as a broken chip in a brand-new canvas. Restricting templates to resolvable clips keeps a freshly applied template clean.

## Consequences

- Editing a built-in template, or authoring a user template, must keep every embedded clip resolvable. To show non-resolvable syntax (a specific session, a `:::clip` block), describe it in prose instead of embedding it.
- Template guidance should stay short and clearly read as orientation, so it does not read as a task when the canvas is run.
- Renaming or adding a built-in template changes its stable `id`; the empty-state placeholder and any id-based references must be updated together. Template selection copies Markdown into the canvas and is not live-linked, so changing a template does not alter canvases already created from it.
- Built-in templates should only use Markdown constructs the canvas renderer styles (headings, list items, smart clips, `:::clip` blocks); emphasis, inline code, and fenced code render as literal characters and should be avoided in template bodies.

## Non-Goals

This spec does not define the template-selection UI, user-template discovery, or the smart-clip syntax itself (see the canvas orchestration spec).
