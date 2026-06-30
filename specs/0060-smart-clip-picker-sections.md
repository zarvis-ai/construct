# 0060-smart-clip-picker-sections

Status: accepted
Date: 2026-06-29
Area: ux
Scope: How the `@` smart-clip picker in the program view organizes its candidates into a relevance section and expandable category submenus, identically in the TUI and web UI.

## Decision

Typing `@` in the program view opens a two-level picker, not a flat list.

Root view, top to bottom:

- **Relevance section** — up to 5 clips drawn from *all* clip types together, ranked by the live type-ahead query. With an empty query this falls back to the most-recently-used clips (sessions first, most-recent first, then harnesses).
- **A separator** — present only when there is both a relevance section and at least one category below it.
- **Category rows** — one per non-empty clip type (currently `session`, `harness`), each showing its item count and a submenu affordance. A category is selectable; activating it (Enter, or Right) drills into that type's submenu.

Submenu view (one category):

- Lists every item of that type, never a filtered subset. The type-ahead query **dims** non-matching items rather than hiding them, so the full set stays visible while matches stand out. An empty query dims nothing.
- The **session submenu mirrors the session-list view's grouping and ordering**: ungrouped sessions first, then each project/group behind its own header, in the same order the list view uses.
- Left (or backing out) returns to the root view and re-highlights the category that was entered.

Selecting a clip (relevance section or submenu) inserts it; selecting a category only navigates. The picker still closes on Escape, on a separator character in the query, or when the caret leaves the `@…` token.

The relevance ranking and the matched/dimmed decision use one shared scoring notion: exact label, label prefix, label word-prefix, label substring, any-field substring, then a loose subsequence over the *label only*. A subsequence over the full searchable text is intentionally **not** a match — for short queries it matches almost everything and would dim nothing.

## Reason

A flat, truncated list forced a single ordering to serve two jobs at once — "show me the thing I'm typing toward" and "let me browse everything of a kind" — and did neither well once a fleet grows past a handful of sessions. Splitting the picker lets the top section stay short and query-driven while the categories give complete, structured browsing. Dimming instead of filtering inside a submenu keeps a stable, scannable list (especially the project-grouped session list) instead of a list that reflows on every keystroke.

## Consequences

- The TUI and web UI must keep the same structure, ordering, ranking, and dimming rules; they share no code, so the behavior is duplicated and must be changed in lockstep. Treat a change to one without the other as a regression.
- The session submenu's grouping/order is defined by reference to the session-list view. If that view's grouping or ordering changes, the submenu must follow.
- New clip types become new category rows (and submenus) and join the relevance pool; they should not be bolted on as a third flat section.
- The relevance section can legitimately repeat an item that also appears under its category — that redundancy is accepted in exchange for a fast top-of-menu path.
- Ranking is heuristic, not stable across query edits; selection is clamped, not preserved by identity, as the query changes.
- In the **TUI**, activating the `session` category opens the reusable session-picker dialog (see `0063-session-picker-dialog`) instead of the inline session submenu; that dialog additionally surfaces archived sessions and auto-expands matching groups. The inserted clip and the relevance section are unchanged, so the lockstep requirement above still binds the clip result, ordering, and dimming — it does not require the web UI to mirror the dialog's modal presentation. The web UI keeps the inline session submenu.

## Non-Goals

- Not a general command palette. The picker only inserts clips into the program buffer.
- No fuzzy ranking beyond the simple label-subsequence fallback; this is deliberately conservative to keep dimming meaningful.
- Archived-session rollups and other list-view affordances are not reproduced inside the submenu; only grouping and ordering are mirrored.

## Examples

- Query `co` with a `codex` harness and a `code review` session: the relevance section ranks `codex` (exact/prefix) and `code review` (word-prefix) at the top; both categories still appear below the separator.
- Open the session category with query `al`: every session is listed under its project header; `alpha` is bright, `beta` is dimmed but still visible in place.
- Empty query, three sessions and two harnesses: the relevance section shows the most-recent clips (up to 5), then `session` and `harness` category rows.
