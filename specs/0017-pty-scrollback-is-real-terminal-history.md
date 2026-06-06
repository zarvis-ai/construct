# 0017-pty-scrollback-is-real-terminal-history

Status: accepted
Date: 2026-06-01
Area: tui
Scope: What the TUI shows when the user scrolls back a PTY-backed session.

## Decision

Scrolling back a PTY session shows the **real terminal history** — every
line the child actually painted (prose, tool calls, command output,
diffs), in order — not a semantic substitute. The history comes from a
faithful terminal emulation of the child's byte stream.

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
  scroll-region TUIs (codex) the same way it already did for
  line-oriented children (claude/shell) — no per-harness handling.
- The vendored `vt100` must be kept patched across upstream bumps; the
  change is isolated to `Grid::scroll_up` and marked in-source as an
  `agentd fork patch`. Prefer upstreaming it so the fork can be dropped.
- No agentd code changes are required: the existing scrollback path fills
  correctly once the emulator retains the lines.

## Non-Goals

- Not altering the live (non-scrolled) render.
- Not changing alt-screen apps (vim/htop), which have no scrollback by
  design.
- Not a general rewrite of the scrollback emulation; only the
  region-top-row-0 rule is corrected.

## Examples

- codex prints a long answer above a footer pinned by `ESC[1;37r`. Before:
  scroll-up was empty / fragmented (`maxsb == 0`). After: the full
  conversation is in scrollback, in order.
- claude appends lines (full-screen scroll, no region) — unchanged; it
  already populated scrollback.
