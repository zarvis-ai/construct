# 0099-program-attachments

Status: accepted
Date: 2026-07-16
Area: ux
Scope: Image and file attachments referenced from a session's program document, and how clients render them.

## Decision

- The program document stores attachments as **standard Markdown links**:
  `![name](path)` for images, `[name](path)` for other files — never a
  bespoke token format. The path points at a file the daemon host can
  read (typically under the session's attachments directory, written by
  the existing clipboard-attachment mechanism).
- Clients render **image links only** as compact chips; plain file links
  stay literal text (they have no preview affordance, and collapsing them
  would hide the path for no benefit). Hovering a chip shows the file's
  info (name, path, size, type) and, where the client can, a preview.
  Chips do not require the link to stand alone on its line; an expanded
  preview renders below the line the link sits in.
- A chip is **atomic to the cursor**: caret positions inside the link's
  source map to the chip's boundary, and positions after it measure from
  the chip's painted width, so the caret and selection always land where
  the user sees them. The source text itself is untouched — atomicity is
  a presentation/cursor rule, not an editing restriction.
- A chip is toggleable between its compact form and an expanded preview.
  Expansion follows CommonMark/GitHub rendering semantics: the image
  **replaces** the chip at the link's position in the flow — the chip text
  is not shown alongside it — and is **left-aligned**, never centered.
  Text before the link keeps its place; text after the link flows **below
  the image**, preserving reading order (a terminal's equivalent of the
  tall HTML line box). Clicking the image collapses back to the chip.
  Expansion state and preview size are **client-local view state**: they
  are never written into the Markdown and never synchronized, so agents
  and other clients see only the canonical link. A client may persist its
  own view state (per session) so expansion survives restarts.
- Expansion state is **per link instance**, not per target path: the same
  file referenced twice expands independently. Instance identity derives
  from the link's containing line content (plus duplicate-line ordinal and
  index within the line) — so ordinary edits elsewhere leave it untouched,
  breaking a link mid-edit and re-completing it restores its expansion,
  and editing the line itself resets its instances to chips.
- Clients expand previews as their architecture allows: a client that
  owns its text rendering may reflow the document around a true inline
  block; a client built on a native text-input surface may anchor the
  preview to the chip without reflowing. Both must keep the document
  text itself untouched.
- The chip machinery keys on Markdown link syntax generically, not on an
  attachment-specific marker — any `![name](path)` in the document
  renders the same way, whether a client inserted it or a human/agent
  typed it. Previews only ever read **local filesystem paths**; clients
  must not fetch remote URLs to render a preview.

## Reason

The program is a shared surface edited by humans and agents. Standard
Markdown keeps it legible to every consumer (agents act on the path
directly), portable across renderers, and free of invented syntax that
would go stale (a stored `#N` breaks the moment content is inserted above
it). Render-time chips give humans the compact, chat-style affordance
without contaminating the stored document, and keeping expansion state
local prevents one user's view preferences from becoming everyone's
edits.

## Consequences

- Future editor changes must preserve the source→display transform
  contract: wrap math, cursor mapping, and painting all measure the same
  rendered text, chips included.
- Attachments live under the session's attachment storage; deleting a
  session can dangle program references. Accepted for now — the chip
  renders its info card with a missing-file note.
- No remote-URL previews means a document can contain image links that
  render as chips but never preview; that is intentional (no network
  fetches from render paths).

## Non-Goals

- WYSIWYG Markdown editing. Only attachment/image links get chip
  treatment; the rest of the document stays plain text.
- Synchronized view state (shared "expanded" flags) across clients.

## Examples

- Pasting an image into the program surface uploads it as a session
  attachment and inserts `![screenshot](…/attachments/screenshot-….png)`
  on its own line; the editor shows `[Image #1]`.
- An agent writes `![diagram](/tmp/diagram.png)` into the program; every
  client renders `[Image #2]` (or whatever its render-time ordinal is)
  with the same hover and expansion affordances.
- Clicking `[Image #1]` in a client that reflows text shows the image as
  a block at that point in the document, drag-resizable; clicking again
  collapses it back to the chip. The stored Markdown never changes.
