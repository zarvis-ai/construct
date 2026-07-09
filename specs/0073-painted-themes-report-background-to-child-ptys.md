# 0073-painted-themes-report-background-to-child-ptys

Status: accepted
Date: 2026-07-09
Area: architecture
Scope: How child PTY terminal-background probes (OSC 11) are answered when a client theme paints the frame background.

## Decision

When a connected client's theme paints the full frame background itself, child PTY sessions that ask for the terminal background color must receive that painted color. Themes that intentionally leave the terminal background visible, such as Matrix and Basic, must not cause a synthesized response.

The daemon is the single authority for answering these probes:

- Clients report their painted background (or "none") to the daemon as per-connection state, re-sent on connect and on theme change. Reports are removed when the connection closes.
- The effective background is the most recent report among live connections; when it is "none", probes pass through unanswered and the child's own fallback applies, as before this spec.
- The daemon scans live child PTY output for background probes, answers each probe exactly once by writing the reply into the child's input, and strips the probe from the byte stream that clients, transcripts, and replay logs see — so no attached terminal emulator (a real terminal, or xterm.js in the web client) can answer a second time.

Clients must never answer child probes themselves by injecting bytes into a session's input.

## Reason

Interactive CLIs query the terminal background color to choose readable foreground colors. For painted Construct themes, the visually relevant background is the one Construct paints, not the user's underlying terminal.

The first implementation answered from the TUI by watching the broadcast byte stream and injecting replies into child input. That design corrupts sessions: every attached client answers independently (duplicate replies), replies arrive when the child is no longer waiting for them (stray input a cooked-mode shell echoes into its output stream), and the same probe bytes can be re-observed and re-answered. A probe must be answered by exactly one authority that sits in front of the child's PTY — the daemon.

## Consequences

- Future theme changes must preserve the painted/background-aware distinction: painted themes report their color, background-aware themes report "none".
- Responses are generated from reported theme data, so live theme changes affect subsequent probes without adapter changes.
- Because probes are stripped from the downstream stream whenever a painted background is in effect, client-side terminal emulators only see (and may answer) probes when no painted background is reported — which preserves pre-spec behavior for background-aware themes.
- With several clients connected on different themes, the most recent reporter wins; children see one coherent answer, not one answer per client.

## Non-Goals

- Replayed transcript bytes and historical PTY snapshots never trigger responses; only live child output does.
- The web client does not yet report its painted background; wiring that report (and deciding how an idle background tab should rank against the client the user is actually looking at) is follow-up work. Until then, web-only users keep xterm.js's native probe answering.

## Examples

- A child runs `printf '\x1b]11;?\x07'` while the user's TUI uses the dark painted theme: the child receives one `\x1b]11;rgb:…\x07` reply with the theme's background; the probe bytes never appear in the transcript or any client's stream.
- The same child under the Matrix theme: no reply is synthesized, the probe passes through to clients, and the child times out to its own default (or an attached terminal emulator answers, as before).
