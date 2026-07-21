# 0076-program-selection-run-comment

Status: accepted
Date: 2026-07-08
Area: tui
Scope: Keyboard behavior and prompt delivery for running a selected Program region with an optional one-line instruction.

## Decision

When Program text is selected, the TUI selection context menu offers one Run button and an optional instruction field. Pressing Tab while a non-empty Program selection is active moves keyboard focus from the editor into this context menu. Typing while the menu is focused edits the instruction as a single logical line. The instruction may wrap visually in the menu, but newline insertion is not part of the affordance. Enter, or clicking the Run button, runs the selection; when the instruction is non-empty, it is passed with the Program Run prompt.

Selection Run executes in a visible interactive same-harness fork by default, even when the optional instruction is populated. The fork receives the selected Run context but writes progress and results directly to the Program-owning session's document with an explicit target session id; it does not return work for the owner to merge or queue a follow-up turn onto the owner. Holding Shift reverses only the execution destination: Shift+Enter and Shift+click Run deliver the same Run to the Program-owning session. While Shift is held, the menu label and focused-row description preview that it will run on the main session. Full-document/title-bar Run and non-selection API callers retain their established owner-session behavior unless they explicitly request a fork.

The focused instruction editor supports the same basic single-line movement and deletion keys users expect elsewhere in the TUI: C-a, C-e, C-b, C-f, C-d, and C-k. Because the field can wrap visually, C-p, C-n, Up, and Down move between wrapped visual rows. Its Run button remains visually distinct from typed text, is aligned to the right edge, and is highlighted only while the context menu is focused or hovered. The context menu content has one-column horizontal padding inside the border.

The extra instruction is run metadata, not Program content. It must not alter the selected markdown, selection block identity, or optimistic shimmer scope. The daemon appends the instruction to the generated Program Run prompt and disables mechanical fast paths that cannot interpret it.

## Reason

Selection Run is often used to steer a narrow region without editing the Program itself. A transient comment lets the user give one-off direction while preserving the Program document as durable plan/state rather than turning every small instruction into persistent markdown.

## Consequences

Future TUI changes must preserve Tab as the keyboard entry point to the selected-text Run menu while a non-empty selection is active. The comment is optional and trimmed before dispatch. Two otherwise identical Runs with different comments are distinct user intents and must not be deduplicated together.

Non-TUI clients may omit the comment field; compatibility requires the daemon to treat absence as the existing plain Program Run behavior.

## Non-Goals

This does not define a persistent Program comment model, multi-line comments, or web UI parity for the comment affordance.
