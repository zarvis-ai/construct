# 0027-tui-terminal-theme-aware

Status: accepted
Date: 2026-06-12
Area: tui
Scope: How the TUI palette adapts to the terminal's light/dark background.

## Decision

The TUI ships terminal-background-aware palettes in dark-background and light-background variants. Matrix uses `Theme::dark()` / `Theme::light()`, and Basic uses its own neutral dark/light variants. `theme.toml` gains a `mode` key: `"auto"` (default), `"light"`, `"dark"`. In `auto`, the TUI queries the terminal's background color via the **OSC 11** escape (`\x1b]11;?\x07`), parses the reply's RGB into perceived luminance, and picks light (luminance > 0.5) or dark for background-aware themes. `[colors]` overrides apply on top of whichever variant is active.

The query runs once, right after `enable_raw_mode` and before the event loop consumes stdin, with a ~120 ms timeout (poll/read on the tty fd, Unix-only). A terminal that doesn't answer falls back to dark.

## Reason

The palette was a single dark-tuned set of foreground colors drawn on the terminal's own background (`Color::Reset`, no `bg` of its own), so on a light terminal the light-green text was low-contrast and hard to read. OSC 11 is the portable way to learn the terminal background (supported by Terminal.app, iTerm2, kitty, alacritty, wezterm; passed through by tmux), so the TUI can be genuinely terminal-theme-aware instead of assuming dark.

## Consequences

Light-terminal users get a readable variant of whichever background-aware palette is active. Dark-terminal users are unchanged for Matrix (detection → dark, same palette as before). The `mode` knob is an escape hatch for terminals that don't answer or where the user prefers a fixed variant. Cost: one `libc` dep (the timed tty read) and a ~≤120 ms startup query (skipped for forced modes / no answer).

## Non-Goals

This does not require every theme to be background-aware; painted themes are covered separately. It also does not provide live re-detection on terminal theme change (resolved once at startup) or Windows OSC support (falls back to dark there).
