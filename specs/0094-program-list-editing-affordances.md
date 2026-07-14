# 0094-program-list-editing-affordances

Status: accepted
Date: 2026-07-14
Area: ux
Scope: how the program editor formats and continues markdown list items while the user is typing them.

## Decision

The program editor treats markdown list editing the way mainstream editors
(Notion, Obsidian, VS Code) do:

- A line whose content after the indent is just a list marker (`- ` or `* `,
  with nothing typed after it yet) already renders as a bullet. The bullet
  glyph appears the moment the marker is typed, not only once the first
  content character lands.
- Enter on a list item that has content continues the list: the new line
  starts with the same indent and the same marker style, and any text after
  the caret becomes the new item's content (splitting mid-item produces two
  items). A checklist item continues with a fresh unchecked box regardless of
  the current item's mark.
- Enter on a list item with no content dissolves the item — the marker (and
  any checklist box) is removed and the line becomes plain and empty — rather
  than inserting another empty bullet. This is how a list is ended from the
  keyboard.
- Enter with the caret still inside the indent or marker, with an active
  selection, or on a non-list line inserts a plain newline.

## Reason

`- ` followed by content is how people type bullets; requiring the first
content character before formatting made the editor feel broken for the
common case, and manually retyping the marker on every new line made lists
tedious. These affordances are the near-universal convention, so muscle
memory from other editors transfers directly.

## Consequences

- List-item detection must key on the start-trimmed line so that trailing
  whitespace (including the marker-only state) never defeats the match, and
  every consumer of the detection — rendering, cursor math, click
  normalization, indent commands, Enter handling — must share one rule or
  they desync.
- The Enter behavior is buffer-local editing sugar: it must compose with the
  editor's existing undo, selection-replacement, and live-edit sync exactly
  like any other insertion or deletion.
- Accepted tradeoff: a literal `- ` or `* ` line can no longer be kept as
  plain text in the program editor.

## Non-Goals

- Auto-renumbering ordered lists, or continuing other prefixes (quotes,
  headings) on Enter.
- Backspace-at-content-start dissolving the marker (a possible future
  affordance, not covered here).
