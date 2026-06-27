# 0043-canvas-incremental-search

Status: accepted
Date: 2026-06-27
Area: ux
Scope: Incremental search behavior in markdown canvas editing.

## Decision

Canvas markdown editing uses Emacs-style incremental search with `C-s` to enter search mode, incremental query input, and explicit navigation with `C-s` (next) / `C-r` (previous). Exiting search is split by intent:

- `Enter` accepts the current match and closes search mode.
- `C-g` cancels search mode and restores the cursor to the pre-search anchor.

While search is active, every typed character extends the query and updates match ranges. Search highlights are visible in the canvas body and the active match is visually distinguished from non-active matches.

## Reason

Canvas editing is now a primary markdown editing surface; it needs the same discoverable in-place incremental search behavior users expect from editors for fast local navigation. Explicit mode transitions prevent accidental full-canvas text replacement and make search a reversible, low-risk command.

## Consequences

- Search state is tracked on the canvas popup and does not interfere with smart-clip suggestions or selection gestures.
- Search mode can be re-entered and edited from the current cursor position without closing the canvas.
- The modeline should prefer search status text while search mode is active so users can tell whether a query is empty, failing, or positioned.
- Search highlights must preserve existing canvas visuals (selection, smart-clip spans, and running-shimmer overlay) and remain compatible with wrapped rows.
- Cancelling search restores the original cursor anchor; accepting search keeps the current cursor position.

## Non-Goals

This spec does not define full regex search, case-sensitivity toggles, search/replace, or cross-session canvas search.

## Examples

- `C-s` `a` `l` `p` `h` `a` `C-s` cycles from the first match to the next; `C-r` cycles backward.
- `C-s` then `C-g` returns to the query start position and closes the I-search bar state.
- `C-s` with an empty query opens I-search with no active match, then typing begins collecting matches immediately.
