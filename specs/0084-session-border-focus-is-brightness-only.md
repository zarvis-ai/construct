# 0084-session-border-focus-is-brightness-only

Status: accepted
Date: 2026-07-11
Area: ux
Scope: The session/split-pane border's focused vs. unfocused color, across all client UI themes.

## Decision

For every named UI theme (`matrix`, `basic`, `dark`, `light`), a session or
split pane's focused border must differ from its unfocused border only in
brightness (lightness) — never by introducing or changing hue. Focus should
read as "this border lit up," not "this border changed color."

Two concrete implementations satisfy this, depending on the theme:

- **Matrix** keeps a fixed green hue for the session border; only
  saturation/lightness shift between unfocused and focused.
- **Basic, Dark UI, and Light UI** use a neutral, fully achromatic grey for
  the session border (zero saturation) at two lightness levels — a dim grey
  when unfocused, a lighter grey when focused on dark-background themes
  (darker when focused on light-background themes, for contrast against a
  light backdrop). Being achromatic, this border can never carry a hue that
  might be confused with anything else on screen.

This is a separate rule from [[0083-program-border-fixed-across-themes]],
which fixes the Program pane's frame to one hue (cyan) across every theme and
signals focus with a Bold/Dim modifier instead of a color change.

## Reason

Matrix's session border already behaved this way: unfocused and focused share
a hue, differing mainly in saturation/lightness. Basic/Dark UI/Light UI
originally used a blue accent for the border, at low saturation when
unfocused and high saturation when focused. Two problems with that: the
unfocused state was desaturated enough to read as plain gray, so the jump to
a vivid focused blue looked like a color swap rather than a brightness
change; and blue sits close enough to the Program pane's fixed cyan frame
(spec 0083) that a session border and the Program border could be
misidentified for one another at a glance. Making the session border fully
achromatic for these three themes fixes both: there is no hue left to drift
on focus, and zero saturation can never collide with the Program pane's
chromatic cyan.

## Consequences

Adding or editing a named theme's `border`/`border_focused` pair must either
(a) keep both at the same hue if the theme wants a colored border (Matrix's
approach), or (b) make both fully achromatic (zero saturation) if the theme
wants a neutral border distinct from the Program frame's accent (Basic/Dark
UI/Light UI's approach). A regression test asserts hue continuity for Matrix
and zero saturation (plus a chromatic `program_border` for contrast) for the
other three, so a future edit can't silently reintroduce a colored-but-wrong
border or a hue collision with the Program frame.

The `[colors]` `theme.toml` escape hatch can still override `border` and
`border_focused` independently per spec [[0070-client-ui-themes]] — this
decision governs the *default* palettes shipped for each named theme, not
what a user's custom override is allowed to do.

## Non-Goals

This does not change `program_border` (spec 0083, a distinct fixed-hue
design) or any other theme slot (`accent`, `accent_alt`, `highlight_bg`,
etc.) — those keep following the active theme's own palette. It does not
mandate specific lightness values, only that unfocused/focused pairs differ
in brightness and not hue.
