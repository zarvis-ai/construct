# 0093-program-editor-cursor-math-mirrors-paint

Status: accepted
Date: 2026-07-13
Area: tui
Scope: the program editor's cursor/selection/hit-test geometry must be derived from exactly the glyphs the renderer paints, at grapheme granularity.

## Decision

The program editor maps buffer offsets to screen cells (caret placement,
click hit-testing, vertical navigation, scroll follow, clip/link hitboxes)
through its own re-implementation of the paint-side word wrap. That math must
mirror the paint layer's behavior exactly, at the granularity the paint layer
actually works in:

- Text is measured in grapheme clusters, not chars. An emoji presentation
  sequence (VS16) or ZWJ sequence is one two-cell glyph regardless of how
  many chars compose it.
- Grapheme clusters containing control characters (tabs included) paint
  nothing and are not word-break opportunities.
- A no-break space is not a break opportunity; a zero-width space is.
- Every per-line source-to-rendered transformation the renderer applies
  (heading lines paint their source literally, bullets get a rendered marker
  prefix, clip fence lines paint fixed chip text, inline smart clips expand
  to labeled chips) must have an equivalent in the cursor math's model of the
  rendered line.

The contract is enforced by a differential test that paints representative
documents through the real render pipeline into an in-memory terminal and
asserts, for every checkable character, that the glyph under the computed
cursor cell is that character. Changes to the renderer's per-line
transformations, the wrap rules, or the text-measurement stack must keep that
test green — extending its corpus when they introduce a new transformation.

## Reason

Cursor offsets are buffer positions, but the user perceives the caret and
selection through painted cells. Any divergence between the editor's own wrap
math and what the paint layer actually does surfaces as the caret sitting
where the text is not — edits landing beside the visible cursor, clicks
selecting neighboring characters, and (when the divergence changes a line's
wrapped row count) every line below drifting by whole rows. These bugs
compound with document length and are nearly impossible to diagnose from
symptoms alone; a single emoji near the top of a long document can shift the
caret for the entire rest of the document.

## Consequences

- Upgrading or replacing the TUI rendering library requires re-verifying the
  wrap math against its actual output (the differential test does this
  mechanically).
- New markdown line transformations in the program renderer must land
  together with their cursor-math model and a corpus entry exercising them.
- Text measurement anywhere in the editor's geometry path must use
  grapheme-cluster segmentation with sequence-aware width, never per-char
  width sums.
- Accepted tradeoff: the paint layer can itself misrender one known edge (a
  two-cell glyph landing exactly on a wrap boundary makes it emit an
  over-wide row whose spill hides the next row's first cell). The cursor math
  intentionally reproduces that row geometry rather than "fixing" it locally,
  so offsets stay consistent with what is painted; the differential test
  documents and carves out that artifact.

## Non-Goals

- Grapheme-aware cursor *movement* (arrow keys step char-wise through
  multi-char clusters today) is a separate concern; this spec only governs
  where a given offset paints.
- Pixel-perfect caret placement inside inert chip renderings (clip fences)
  is not required; the caret must stay on the correct row and within the
  painted line.
