# 0084-session-border-focus-is-brightness-only

Status: accepted
Date: 2026-07-11
Area: ux
Scope: The session/split-pane border's focused vs. unfocused color, across all client UI themes.

## Decision

For every named UI theme (`matrix`, `basic`, `dark`, `light`), a session or
split pane's focused border color must be a visibly brighter/more-saturated
variant of the *same hue* as its unfocused border color — never a
perceptibly different hue. Focus should read as "this border lit up," not
"this border changed color."

Concretely: the unfocused border must carry enough saturation to be
recognizable as belonging to the theme's accent hue family (not desaturated
to near-gray), so that the jump to the focused border's higher
saturation/lightness reads as a brightness change rather than a hue swap.

This is a separate rule from [[0083-program-border-fixed-across-themes]],
which fixes the Program pane's frame to one hue across every theme and
signals focus with a Bold/Dim modifier instead of a color change. Session
border focus, by contrast, is themed (its hue follows the active palette) —
the rule here only constrains the *relationship* between its two states.

## Reason

The Matrix theme's session border already behaved this way: unfocused and
focused share essentially the same hue, differing mainly in
saturation/lightness. The other named themes (Basic, Dark UI, Light UI)
technically preserved hue too, but their unfocused border sat at ~10-22%
saturation — desaturated enough to read as plain gray — while the focused
border jumped to ~90-100% saturation. Because the unfocused state carried no
visible hue, the focus transition perceptually looked like the border
changing to an unrelated color, even though the numeric hue angle barely
moved. Raising the unfocused border's saturation (roughly to the ~45-55%
range) makes the same-hue relationship actually visible, matching the
already-correct Matrix behavior.

## Consequences

Adding or editing a named theme's `border`/`border_focused` pair must keep
both at (approximately) the same hue, and the unfocused color must be
saturated enough to visibly belong to that hue rather than reading as
neutral gray. A regression test asserts both properties (hue distance within
a tolerance, and a saturation floor for the themes retuned by this decision)
so a future edit can't silently reintroduce a gray unfocused border or an
unrelated-hue focused border.

The `[colors]` `theme.toml` escape hatch can still override `border` and
`border_focused` independently per spec [[0070-client-ui-themes]] — this
decision governs the *default* palettes shipped for each named theme, not
what a user's custom override is allowed to do.

## Non-Goals

This does not change `program_border` (spec 0083, a distinct fixed-hue
design) or any other theme slot (`accent`, `accent_alt`, `highlight_bg`,
etc.). It does not mandate a specific saturation/lightness value, only that
the unfocused/focused pair stay recognizably the same hue with unfocused
carrying visible color rather than being desaturated to gray.
