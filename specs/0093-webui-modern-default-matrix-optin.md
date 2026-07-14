# 0093-webui-modern-default-matrix-optin

Status: accepted
Date: 2026-07-13
Area: webui
Scope: The web UI defaults to a conventional modern AI-chat-app look; the Matrix CRT treatment is an opt-in theme, not the baseline.

## Decision

The web UI's baseline design language follows the conventions users already
know from mainstream AI chat apps:

- Sans-serif system UI font for chrome and chat; monospace is reserved for
  terminal output, code, tool call cards, and the program editor.
- Chat transcript rendered as a centered reading column: user messages as
  right-aligned rounded bubbles, assistant messages as plain text on the page
  background (no bubble), tool calls as rounded bordered cards, and no
  per-message role captions.
- A rounded composer surface with an embedded round accent send button.
- Rounded, softly bordered surfaces throughout (sidebar rows, dialogs,
  overlay buttons); on desktop widths, dialogs present as centered modal
  cards while narrow (phone) widths keep the bottom-sheet idiom.
- Theme colors are restrained: neutral dark/light bases with the construct
  green as the accent. The default theme is "System", which follows the
  OS light/dark preference live and resolves to the modern dark or light
  palette.

The full CRT treatment — monospace chrome, scanline overlay, phosphor glow,
neon palette — remains available as the "Matrix" theme, applied via
theme-scoped styling on top of the same modern layout. Structural layout is
never forked per theme; a theme may only recolor and re-texture.

Construct identity survives in the default themes only as accents: the green
accent color and the live matrix-rain connection indicator in the header
(framed as a small badge). New UI work must keep functional controls plainly
labeled and conventional; Matrix flavor belongs in titles, animation, and
opt-in theming.

## Reason

The web UI is used from phones and desktop browsers by people who already
use mainstream AI chat apps daily. An all-custom CRT baseline made the UI
feel unfamiliar and harder to parse (dense mono text, uppercase labels,
low-contrast green-on-green). Familiar conventions lower friction; the
construct personality is preserved where it doesn't cost usability, and the
full nostalgia skin stays one picker option away.

## Consequences

- New web UI surfaces must be designed against the modern baseline first,
  using the shared theme variables (never hardcoded palette values), so all
  themes — including Matrix — pick them up automatically.
- Matrix-only effects (scanlines, glow, mono chrome) must stay scoped to the
  Matrix theme selector and must not leak into the shared structural rules.
- Theme data must keep an identical variable key set across themes; the
  runtime applies only the selected theme's keys, so a key missing from one
  theme would leave stale values behind on switch.
- The default for new clients is "System" (OS-following light/dark); an
  explicit theme choice always wins over the OS preference. Changing the
  default again is a product decision, not a styling detail.

## Non-Goals

- This does not decide the native TUI's look, which keeps its own theming.
- It does not mandate any specific framework or asset pipeline; the web UI
  remains a single embedded HTML file.
