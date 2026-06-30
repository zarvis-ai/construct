# 0063-session-picker-dialog

Status: accepted
Date: 2026-06-29
Area: tui
Scope: A reusable modal session-picker dialog in the TUI, used both to switch the focused window's session and to insert a session clip into a program.

## Decision

The TUI has one reusable session-picker dialog. While open it captures all keyboard input (it sits above every other input surface, including the program view).

Structure and behavior:

- It lists sessions with the **same grouping and ordering as the session-list view**: ungrouped sessions first (by position), then each project/group behind its header (by position), and each section's archived sessions behind an expandable "N archived" row.
- A typeahead query filters by **dimming, not removing**: non-matching sessions stay in place but render dimmed; matching sessions render bright. An empty query dims nothing.
- Groups and archive sections **auto-expand and auto-collapse from the query**: a project group opens when one of its sessions matches (and collapses to just its header when none do); an archive section opens only when the query matches one of its archived sessions, so archived sessions stay hidden during ordinary browsing.
- Navigation (Up/Down and `C-n`/`C-p`) moves only through the **visible, non-dimmed** session rows, wrapping at the ends. Headers are never selection targets. Enter confirms the highlighted session; Escape (or `C-g`) cancels.

The dialog is opened from two places, which differ in **where it sits, where its query comes from, and what confirming does**:

- The `C-x b` keybinding opens it as a **centered session switcher** with its own typeahead search line; confirming focuses the chosen session in the active window. Its frame has a **fixed height** — derived from the full, unfiltered list — so the search line does not move as the query narrows the results; the body scrolls within the constant frame.
- The program view's `@`→session path opens it as a **clip picker anchored where the inline `@` context menu sat**, with **no in-dialog search line**: the live `@<typeahead>` token the user is already typing in the program buffer *is* the query (typing and backspace edit that token in place, and backspacing over the `@` itself dismisses the picker). Confirming inserts an `@{session:id}` clip into the program buffer in place of the `@…` token.

The match notion that decides "matched vs dimmed" is the same one the session switcher already used (exact/prefix/substring over title, id, and harness, then a loose fuzzy subsequence); an empty query matches everything.

## Reason

A flat text-input switcher and a separate inline `@`→session submenu each re-implemented "find me a session" with different affordances and neither showed archived sessions or the project structure at a glance. One dialog that mirrors the list view gives a single, familiar mental model for both jobs. Dimming instead of filtering keeps the list stable and scannable as the user types (the project-grouped layout does not reflow on every keystroke), while query-driven auto-expand keeps the visible set small without hiding the structure. Reusing the existing match scoring keeps switch-by-typing behavior unchanged.

## Consequences

- The dialog is the topmost modal: its key handler must run before the program-popup and minibuffer gates so a dialog opened over the program view captures input instead of leaking it into the buffer.
- The dialog's grouping/ordering is defined by reference to the session-list view. If that view's grouping or ordering changes, the dialog must follow.
- The `C-x b` chord no longer opens a text-input minibuffer; the prior minibuffer-based switcher and its bespoke ranking/hint helpers were removed. The shared per-session match scoring remains and now also drives the dialog's dimming.
- Expansion is a pure function of the query and the session set — the dialog keeps no manual per-group open/closed state, so its view is fully determined by what the user has typed.
- The `@`→session variant does not own its query: it reads the program's live `@<typeahead>` token, and keeps the underlying `@` smart-clip search alive so confirming can replace the token. The program publishes the inline picker's anchor (cursor position + program rect) each frame so the dialog can hang there; without that anchor it falls back to a centered, search-line-less list.
- The switcher's fixed height is a function of the unfiltered session set, not the live result count — so it stays stable while filtering, at the cost of empty body space when few sessions match (and extra scrolling when archive sections expand past the unfiltered row count).

## Non-Goals

- Not a command palette: it only switches focus or inserts a session clip.
- No mouse interaction in v1 — it is keyboard-driven.
- It does not reproduce every list-view affordance (pinning, rename, subagent nesting); it surfaces sessions, project groups, and archived sessions for selection only.

## Examples

- `C-x b` with sessions `alpha`, `beta`, and a `Proj` group holding `gamma`: the centered dialog lists all three bright under their headers; the archived sessions inside `Proj` stay hidden behind a collapsed "N archived" row.
- Typing `al`: `alpha` stays bright and selectable, `beta` dims in place, and `Proj` collapses to its header because none of its sessions match — and the dialog's frame does not shrink as those rows collapse.
- Typing the title of an archived session: its project opens and the "N archived" row expands to reveal it, bright and selectable.
- In a program, `@` then the `session` category: the dialog opens **anchored where the `@` menu was**, with no search line. Typing more (e.g. `al`) extends the buffer's `@al` token and dims non-matches in place; picking `alpha` replaces the `@al` token with `@{session:<id>}` and closes both the dialog and the `@` search.
