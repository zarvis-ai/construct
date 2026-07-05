# 0070-client-ui-themes

Status: accepted
Date: 2026-07-06
Area: ux
Scope: How Construct clients expose and preserve user-selectable color themes.

## Decision

Construct clients expose three named UI themes: `matrix`, `dark`, and `light`.
Matrix remains the default identity theme. Dark and light are neutral palettes
for users who want higher contrast or a non-Matrix visual treatment.

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
mixed palette behind.

Matrix-specific visual mechanics, such as the operator rain viewport, may keep
their behavior across themes, but their colors must adapt to the active palette.

## Non-Goals

This does not define synchronized cross-client theme state, custom web theme
editing, or live terminal background detection for the browser.
