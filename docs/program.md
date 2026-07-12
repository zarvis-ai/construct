# Program

Every construct session owns a **Program**: a durable Markdown document that you
and the session's agent edit together and run. Use it as a task board, an
investigation log, or any orchestration state you want to survive across turns,
restarts, and clients. The Markdown is the source of truth — everything you see
in the Program view is a projection of a plain `program.md` you could edit with
any tool.

Running a Program does not start a separate workflow engine. Run submits the
document (or your selection) to the owning session's agent as one autonomous
instruction turn: the agent infers the objective from your prose, keeps working
while there is actionable work, delegates independent subtasks to subagents, and
writes results back into the document.

## Opening the Program

| Surface | How |
|---|---|
| TUI | `C-x Space` toggles the Program for the selected session, or click the `▣` glyph on the session title bar. |
| Web client | Switch the session's view mode to **Program** (peer to Terminal and Chat). |

In the TUI the Program renders as a resizable roll-down over the top of the
session pane — drag the bottom border to resize; the terminal stays visible
underneath. Only the explicit toggle closes it: `Esc` and clicking elsewhere
never discard your Program. Selecting another session swaps to that session's
Program and stashes the current one, caret and scroll position included. Open
Programs are restored when the TUI restarts.

An empty Program shows the available templates as one-click starting points,
plus a smart-clip syntax reference.

## Editing

The Program is a full Markdown editor with Emacs-style bindings. Human and
agent edits merge automatically (see [Collaboration](#collaboration)).

| Keys | Action |
|---|---|
| `C-x C-s` | Save |
| `C-x u` / `C-/` | Undo |
| `C-a` / `C-e`, `Home` / `End` | Line start / end |
| `C-b` / `C-f`, `C-p` / `C-n`, arrows | Move by character / visual row |
| `C-l` | Recenter on the caret |
| `C-Space` | Set mark (start keyboard selection) |
| `Shift`+arrows / `Shift`+click / drag | Extend or make a selection |
| `C-w` / `M-w` | Cut / copy selection (system clipboard) |
| `C-y` / `C-v` | Paste |
| `C-k` | Kill line |
| `Tab` / `Shift-Tab` | Nest / un-nest the current list item |
| `C-s` / `C-r` | Incremental search forward / backward (`Enter` accepts, `C-g` cancels) |
| `C-g` | Cancel selection or search |
| `@` | Open the smart-clip picker |

The web client uses native browser editing (caret, selection, clipboard, undo,
IME) with the same capabilities, plus `Ctrl+S` to save, `Ctrl+F` for
Emacs-style cursor-forward (click the Find button to search), and
`Ctrl+Enter` to run.

## Smart clips

Smart clips are typed references stored as plain Markdown:

| Syntax | Meaning |
|---|---|
| `@{session:<id>}` | Reference a session. Renders as a chip with a **live status badge** (running / awaiting input / done / errored / missing) driven by daemon events. Click to focus that session — including subagents with no session-list row. Hover for a live preview of its terminal. |
| `@{harness:<name>}` | Reference a harness (`claude`, `codex`, `shell`, …). Used by Run to dispatch work (see below). |
| `:::clip <type> … :::` | A fenced clip block for larger embeds. |

Typing `@` opens a cursor-anchored picker that filters sessions and harnesses
as you type. The daemon stamps each clip with a `clip_id` so repeated
references to the same target stay distinguishable — preserve those ids when
editing existing clips.

## Running

| Control | Action |
|---|---|
| `C-x C-r` or the `▶` Run button | Run the whole document |
| Select text, then `▶ Run` (context menu) or `C-x C-r` | Run only the selection |

Two execution paths:

- **Agent run (default).** The daemon delivers the Markdown to the owning
  session's agent as one submitted turn with an autonomous-run contract: keep
  taking useful actions while work remains, write state back, and record
  blockers on the document instead of asking you to run again.
- **Instant dispatch (fast path).** A selection-Run on list items that each
  contain an `@{harness:<name>}` clip skips the agent round trip entirely: the
  daemon spawns a subagent per item with the item text as its prompt, appends
  the `@{session:<id>}` clip, and marks the block running — sub-second, no LLM
  in the loop.

### Watching a run

- **Shimmer** — blocks still pending in the run animate; settled blocks are
  calm. Shimmer means "work on this block is queued or in flight", nothing
  more. Editing a shimmering block hands it back to you (its shimmer clears).
- **Tooltips** — hover a shimmering block for its status. Agents attach a
  concise status to every block they keep pending; before the agent's first
  writeback the daemon supplies its own truthful status ("Queued behind
  current turn — 2m 10s", "Delivered — waiting for agent") with elapsed time.
- **Staged run indicator** — the title bar shows where the run actually is:
  pressed → delivered → first output → planning pass → N/M settled.
- **Settle flourish** — a block flashes briefly when it settles.
- **Agent presence cursor** — when the agent edits the document you see its
  labeled cursor and a brief highlight where the edit landed.

Runs never lock the document; you can keep editing throughout. Re-running
preserves the progress an in-flight run already showed, and a selection run
adds to existing shimmer instead of replacing it.

## Templates

An empty Program offers templates as one-click buttons. Built-ins: **Tasks**
(a Todo / In progress / Done board), **Investigation** (question, context,
plan, findings), and **Goal** (goal, context, requirements, verification, and
done).
Add your own by dropping `*.md` files into the template directory — the
filename becomes the template id, the contents are inserted
verbatim, and edits are picked up the next time a Program opens (no restart):

1. `CONSTRUCT_PROGRAM_TEMPLATES_DIR` (environment), else
2. `[program].templates_dir` in `config.toml`, else
3. `<data_dir>/program/templates` (default).

Selecting a template copies its Markdown into the Program; it is not
live-linked afterwards.

## Collaboration

Programs are co-edited live. Agents write with **anchored edits** (targeted
find/replace against the latest document), so agent and human edits to
different regions merge with no conflict and no locking. Whole-document saves
carry a version; if the document moved underneath you, the save reconciles by
3-way merge — disjoint edits merge silently, and only genuinely overlapping
edits surface as standard conflict markers for you to resolve. No edit is ever
silently lost.

Every connected client (TUI and web) sees everyone else's labeled cursor in
real time; idle cursors expire after a minute.

## CLI and MCP

```
construct program get <session-id>            # print metadata + markdown
construct program set <session-id> --file …   # replace (also --stdin, --template <id>)
construct program edit <session-id>           # open in $EDITOR, save back
construct program execute <session-id>        # run (optionally --selection <md>)
construct program templates                   # list available templates
```

Agents use the MCP tools `construct_program_get`, `construct_program_edit`
(anchored edits — preferred), `construct_program_update` (wholesale replace),
`construct_program_execute`, and `construct_program_list_templates`.

## Persistence

The document lives at `program.md` inside the session's data directory, with
versioned metadata alongside; recent agent revisions are retained. Programs
survive daemon restarts, and forking a session copies its Program to the fork.

## Design references

Normative design records live in `specs/` — start with
`0041-session-program-is-orchestration-state.md` (core model),
`0042-program-run-progress-affordance.md` (run shimmer lifecycle), and
`0065-program-live-collaboration.md` (live cursors).
