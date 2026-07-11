# 0070-client-ui-themes

Status: accepted
Date: 2026-07-10
Area: ux
Scope: How Construct clients expose and preserve user-selectable color themes.

## Decision

Construct clients expose four named UI themes: `matrix`, `basic`, `dark`, and
`light`. Matrix remains the default identity theme and stays
terminal-background-aware in the TUI. Basic is also terminal-background-aware in
the TUI, but uses common neutral ANSI-style colors instead of the Matrix green
treatment. Dark and light are neutral palettes for users who want higher
contrast or a non-Matrix visual treatment, and the TUI paints their full frame
background so they look the same regardless of the host terminal background
color.

Background-aware TUI themes may probe a directly attached terminal to choose a
light or dark palette. They must not emit terminal-reply-producing probes when
the TUI is running through SSH: a reply can outlive a bounded probe reader,
become ordinary TUI input, and corrupt an attached child terminal. Matrix and
Basic use their dark variant as the safe SSH fallback; users can select the
painted Light theme when they need a fixed light palette remotely.

The TUI must support theme switching from a local slash command and from a
mouse-clickable affordance in the Operator/minibuffer area. The web UI must
support theme switching from a visible picker. Theme switching applies
immediately to the client surface and to embedded terminal views. Each client
persists its own selected theme in its local configuration or browser storage.

## Reason

Color theme is a client-side preference: terminal users and browser users may be
on different displays, backgrounds, or accessibility needs at the same time.
Keeping theme selection local avoids daemon protocol churn and lets each client
change presentation without affecting session state.

## Consequences

Future client UI colors should route through the active theme palette instead
of hardcoding Matrix colors. New embedded terminal surfaces should use the same
theme registry as the rest of their client so switching themes does not leave a
mixed palette behind. The Program pane's frame is a deliberate, narrow
exception to this rule — see [[0083-program-border-fixed-across-themes]].

Matrix-specific visual mechanics, such as the operator rain viewport, may keep
their behavior across themes, but their colors must adapt to the active palette.
TUI renderers must not assume the terminal's default background is visible under
neutral themes; neutral themes own the frame background.

Automatic remote background detection is intentionally less important than
keeping the terminal input stream unambiguous. Do not replace the SSH fallback
with a longer fixed timeout: network delay has no reliable upper bound.

## Non-Goals

This does not define synchronized cross-client theme state, custom web theme
editing, or live terminal background detection for the browser.
