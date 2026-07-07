# 0075-minibuffer-choices-are-clickable

Status: accepted
Date: 2026-07-08
Area: tui
Scope: Every minibuffer confirm/approval prompt's keyboard choices must also be mouse-clickable, with a consistent visual affordance.

## Decision

Any minibuffer prompt that offers a small, fixed set of keyboard choices (`y`/`N`, `y`/`n`/`a`/`f`, `d`/`a`/`N`, ...) renders each choice as its own hoverable, clickable span, in addition to remaining fully keyboard-driven. This generalizes the precedent already established by the new-session harness picker (each harness name is its own clickable span with hover styling) to every other confirm/approval prompt.

A click on a rendered choice must produce the *exact same outcome* as the equivalent keypress — never a third, click-only decision path. Concretely:

- Prompts whose keyboard mechanism is a single keypress (an early-return fast path that bypasses normal text entry — e.g. restart/upgrade/daemon-restart confirmations, tool-call approval) dispatch a click by synthesizing the matching key event and feeding it through the same keypress handler a real keystroke uses.
- Prompts whose keyboard mechanism is typed-text-then-Enter (the user types a short string like `d`, `all`, `y` and presses Enter) dispatch a click by submitting the same literal string through the same submit path Enter uses.

Rendering may restandardize a prompt's phrasing (e.g. splitting a `[d/y]` alias into a single canonical `d` click target, or wrapping a choice in a bracket) as long as the prompt still communicates the same choices it did before, and the underlying decision logic for what each choice *does* never changes or duplicates.

Hover styling matches the harness picker: a choice bolds and underlines when the mouse is over it, and shows a plain underline otherwise — a consistent clickable-affordance look across every minibuffer flavor.

## Reason

The minibuffer already rendered its harness-picker prompt with individually clickable, hoverable names, but every other confirm/approval prompt was keyboard-only despite looking like normal text — nothing on screen suggested `y`/`N` etc. were clickable, and clicking anywhere in an open `ApproveTool` prompt was an explicit no-op. A construct instance is routinely driven from a mouse-first client (the web UI's terminal view, a screen-sharing session, someone without muscle memory for the keybinding), so any prompt that blocks all progress until a specific key is pressed is a dead end for that user.

Scanning the free-form prompt text for known substrings (e.g. searching for the letter "y") was considered and rejected: prompts aren't phrased consistently (some use `[y/N]`, some `(y/N)`, some spell out `(y = orphan / N = cancel)`), and a plain substring search can false-match inside unrelated text (a tool name, a session title, arbitrary tool-call arguments). Instead, the clickable choice cluster is built structurally from the intent's own fields — never by re-parsing rendered text.

## Consequences

- A `MinibufferChoiceHit` (mirroring the existing `HarnessHit`) is registered every render frame for each clickable choice, carrying enough information to dispatch identically to the matching keypress — either "synthesize this key" or "submit this literal string."
- Any future confirm/approval intent should render its choices through this same mechanism rather than inventing a new one-off clickable region, unless it doesn't fit the "small fixed choice set" shape at all.
- Adding a new choice to an existing prompt (or changing what a choice means) only requires updating the one place that already owns that decision (the keypress handler or the submit handler) — the click path re-derives its behavior from the same place and cannot drift out of sync.
- This does not add mouse support to free-text minibuffer input (e.g. typing a session rename, a command-palette query) — those remain keyboard/paste-only. It only covers the fixed-choice confirm/approval shape.

## Non-Goals

- Does not change any prompt's underlying decision semantics (what typing `d` vs `a` vs `N` does) — only how those same choices can be triggered.
- Does not attempt to make arbitrary free-form minibuffer text clickable or spell-checked; only the small set of intents with a genuinely fixed choice list.
