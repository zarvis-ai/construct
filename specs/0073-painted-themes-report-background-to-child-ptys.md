# 0073-painted-themes-report-background-to-child-ptys

Status: accepted
Date: 2026-07-07
Area: tui
Scope: How TUI color themes answer terminal-background probes from child PTY sessions.

## Decision

When a client theme paints the full terminal background itself, child PTY sessions that ask for the terminal background color must receive the theme's painted background color. Themes that intentionally leave the terminal background visible, such as Matrix and Basic, must not synthesize a background response.

## Reason

Interactive CLIs often query terminal background color to choose readable foreground colors. For dark and light Construct themes, the visually relevant background is the one Construct paints, not the user's underlying terminal. Matrix and Basic remain background-aware by design, so the user's terminal must stay authoritative there.

## Consequences

Future TUI theme changes must preserve this distinction: painted themes report their own background to child sessions, and background-aware themes abstain. Background responses should be generated from theme data so live theme changes affect subsequent child probes without changing daemon or adapter contracts.

## Non-Goals

This does not require replayed transcript bytes or historical PTY snapshots to answer terminal probes. Only live child PTY output should trigger a response.
