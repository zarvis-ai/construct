# 0072-vim-modal-keymap

Status: accepted
Date: 2026-07-05
Area: ux
Scope: Vim-profile keyboard input in the TUI.

## Decision

The vim keymap profile is modal, with NORMAL and INSERT modes. NORMAL mode is command-only: when a live terminal session is visible, NORMAL keys are resolved as construct commands and unbound keys are dropped instead of being forwarded to the child PTY. INSERT mode forwards keys to the child PTY except for the construct `C-x` escape prefix and the terminal-mode `C-\ C-n` sequence, which returns to NORMAL. Esc always belongs to the child PTY. The emacs profile remains non-modal and keeps its existing autofocus and PTY-forwarding behavior.

## Reason

Vim users expect bare keys to be commands unless they explicitly enter insert-like input. Forwarding unbound NORMAL keys to an interactive child made command entry risky because a missed binding could type into an agent or shell. Keeping INSERT as PTY-forwarding preserves direct terminal use once the user has intentionally entered that mode, while `C-\ C-n` matches established terminal-mode practice.

## Consequences

Future vim-profile bindings must be evaluated in terms of NORMAL versus INSERT behavior. NORMAL-mode keys must not leak into child sessions, even when the view pane is focused on a live terminal. INSERT-mode escape hatches must remain narrow so terminal applications keep ownership of ordinary editing keys, especially Esc. Changes to the emacs profile must not inherit vim modal behavior.

## Non-Goals

This does not make the emacs keymap modal and does not implement full vim editing semantics inside construct prompts.
