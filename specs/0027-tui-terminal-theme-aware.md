# 0027-tui-terminal-theme-aware

Status: accepted
Date: 2026-06-12
Area: tui
Scope: How the TUI palette adapts to the terminal's light/dark background.

## Decision

The TUI ships the Matrix palette in two variants — `Theme::dark()` and `Theme::light()` — and resolves which to use at startup. `theme.toml` gains a `mode` key: `"auto"` (default), `"light"`, `"dark"`. In `auto`, the TUI queries the terminal's background color via the **OSC 11** escape (`\x1b]11;?\x07`), parses the reply's RGB into perceived luminance, and picks light (luminance > 0.5) or dark. `[colors]` overrides apply on top of whichever variant is active.

The query runs once, right after `enable_raw_mode` and before the event loop consumes stdin, with a ~120 ms timeout (poll/read on the tty fd, Unix-only). A terminal that doesn't answer falls back to dark.

## Reason

The palette was a single dark-tuned set of foreground colors drawn on the terminal's own background (`Color::Reset`, no `bg` of its own), so on a light terminal the light-green text was low-contrast and hard to read. OSC 11 is the portable way to learn the terminal background (supported by Terminal.app, iTerm2, kitty, alacritty, wezterm; passed through by tmux), so the TUI can be genuinely terminal-theme-aware instead of assuming dark.

## Consequences

Light-terminal users get a readable, still-Matrix-flavored palette (dark-green functional text, darker rain heads, pale tails) with no config. Dark-terminal users are unchanged (detection → dark, same palette as before). The `mode` knob is an escape hatch for terminals that don't answer or where the user prefers a fixed variant. Cost: one `libc` dep (the timed tty read) and a ~≤120 ms startup query (skipped for forced modes / no answer).

## Non-Goals

Not a full bg-aware repaint (the TUI still doesn't paint its own background — it relies on the terminal's); not live re-detection on terminal theme change (resolved once at startup); not Windows OSC support (falls back to dark there).
