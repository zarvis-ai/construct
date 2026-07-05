# 0072-webui-session-activity-spinner-parity

Status: accepted
Date: 2026-07-06
Area: webui
Scope: Web UI session activity indicators use the same animated glyph sequence and cadence as the TUI.

## Decision

When the web UI animates session activity, it must swap the status indicator glyph through the TUI spinner sequence `["✦", "✧", "✶", "✷", "✸", "✷", "✶", "✧"]` at the TUI cadence of 120 ms per frame.

This applies to both the session list status indicator and session smart-clip chips in the Program editor. The indicator itself may change glyphs, but the row or chip must not bounce, pulse, scale, or otherwise move as the activity cue.

## Reason

The TUI and web UI represent the same fleet. Matching the activity indicator avoids a client-specific motion language and keeps Program session clips visually stable while work runs.

## Consequences

- Web surfaces that show active sessions should update only the status glyph, not animate the container.
- The web UI should use the same activity gate as the TUI: Smith-like sessions animate only while the agent turn is active, headless sessions animate while running, and PTY sessions animate during the recent PTY quiescence window.
- Future changes to the TUI spinner sequence or cadence should update the web UI in the same change.

## Non-Goals

- This does not require every client to show session activity, only that clients that do animate it use the shared sequence and cadence.
