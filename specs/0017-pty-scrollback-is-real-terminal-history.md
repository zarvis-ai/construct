# 0017-pty-scrollback-is-real-terminal-history

Status: accepted
Date: 2026-06-24
Area: ux
Scope: What clients show when the user scrolls back a PTY-backed session.

## Decision

Scrolling back a PTY session shows the **real terminal history**: the
terminal bytes the child painted, in order, not a semantic substitute.
Scrollback must stay reachable even when an interactive child enters an
alternate/full-screen buffer for its live UI.

The live viewport may remain faithful to the child UI, but scrollback
controls should expose the normal terminal history that would otherwise
be hidden by the alternate buffer. The TUI does this with a normal-screen
shadow parser. The web UI does this by preventing xterm.js from switching
PTY sessions into the alternate buffer, so browser scrollback continues
to operate on the normal buffer.

To make that possible for children that pin a footer with a scroll
region, agentd **vendors the `vt100` crate** (`vendor/vt100`, wired via
`[patch.crates-io]`) with one change to `Grid::scroll_up`: a scrolled-off
line is saved to scrollback whenever the scroll region's **top margin is
row 0** (`scroll_top == 0`), instead of only when no region is active.

## Reason

The full terminal history is what's useful: a coding session is mostly
tool calls and output, so any approach that scrolls only the conversation
prose drops most of the content and is a net regression (we tried it).

But the emulator must actually retain that history. Upstream `vt100`
0.16.2 only pushes scrolled-off lines to scrollback when there is no
active scroll region. Codex pins its input box with a top-anchored
DECSTBM region (`ESC[1;Nr`), so its entire conversation scrolled off into
nothing — `maxsb == 0`, and scroll-up showed an empty or
snapshot-reconstructed mess. Real terminals don't behave that way: xterm
(and xterm.js, which is why the web client never had this bug) save lines
that scroll off the top of the screen whenever the region's top margin is
the top line. The fork brings `vt100` in line with that.

Vendoring is accepted over the alternatives: there is no upstream
callback to hook the scroll event, and reconstructing scrollback from
viewport snapshots is lossy. The fork is one line in one function.

## Consequences

- PTY scroll-up shows the complete, faithful terminal history for
  scroll-region TUIs (codex) and for children that use alternate-screen
  full-screen mode (claude), with no semantic chat fallback.
- The vendored `vt100` must be kept patched across upstream bumps; the
  change is isolated to `Grid::scroll_up` and marked in-source as an
  `agentd fork patch`. Prefer upstreaming it so the fork can be dropped.
- Client renderers must keep replay/live PTY paths consistent, or a
  reconnect or lazy history load can reintroduce hidden or duplicated
  scrollback behavior.

## Non-Goals

- Not replacing PTY history with semantic message history.
- Not guaranteeing useful history for every arbitrary full-screen app:
  if the app never left normal-buffer history before entering its private
  UI, there may be little to reveal.
- Not a general rewrite of the scrollback emulation; only the
  rules needed to preserve normal terminal history are covered.

## Examples

- codex prints a long answer above a footer pinned by `ESC[1;37r`. Before:
  scroll-up was empty / fragmented (`maxsb == 0`). After: the full
  conversation is in scrollback, in order.
- claude enters full-screen mode with `ESC[?1049h` after earlier terminal
  output. Scrolling up in either client should reveal that earlier output
  rather than being trapped in the alternate buffer.
