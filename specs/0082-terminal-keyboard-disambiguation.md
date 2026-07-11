# 0082-terminal-keyboard-disambiguation

Status: accepted
Date: 2026-07-11
Area: tui
Scope: The TUI requests the kitty keyboard protocol's escape-code disambiguation from its own terminal, as a progressive enhancement, so modified keys that legacy encodings cannot express reach the app.

## Decision

At startup, after entering the alternate screen, the TUI queries whether its
host terminal supports keyboard enhancement and, only when it does, pushes
the "disambiguate escape codes" flag (kitty keyboard protocol). The flag is
popped at teardown, before leaving the alternate screen. No other
enhancement flags (event types, alternate keys, associated text) are
requested.

This exists so bindings on keys that legacy terminal encodings fold onto
control characters — `Ctrl+digit` pane focus (`C-1` = the session list,
`C-2`..`C-5` = split windows) chief among them — actually arrive as
distinct key events in terminals that can express them. Terminals without
support keep the legacy encoding and those bindings remain silently
unreachable there; every such binding must have the behavior reachable
another way (`C-x o` cycling covers pane focus).

## Reason

Legacy encodings map `Ctrl+2`..`Ctrl+5` onto NUL/ESC/FS/GS and cannot
express `Ctrl+1` at all, so direct pane-focus keys never reached the app in
most terminals. The disambiguation flag is the narrowest protocol level
that fixes this; requesting more (key-release events, alternate layouts)
would change event semantics the app doesn't need and increase the blast
radius on terminals with partial implementations.

## Consequences

- The push must stay paired with a pop at teardown; an unpopped flag leaks
  enhanced encoding into the user's shell.
- Keys previously ambiguous with control characters (e.g. `Ctrl+I` vs Tab,
  `Ctrl+[` vs Esc) become distinct events in enhanced terminals. Bindings
  must target the semantic key (`Tab`, `Esc`), not its legacy control-char
  alias.
- Behavior is terminal-dependent by design: features must not RELY on
  enhanced-only keys; they may be accelerated by them.
- Keys forwarded into child PTYs are synthesized bytes and are unaffected
  by how the host terminal encoded them.
