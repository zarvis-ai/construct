# 0019-operator-widgets-use-matrix-rain-viewport

Status: accepted
Date: 2026-06-05
Area: tui
Scope: Defines how the collapsed Operator communicates through the Matrix-rain panel.

## Decision

When the Operator session is collapsed, the Matrix-rain panel may act as a transient viewport over the Operator session's normal sticky widgets. Operator widgets keep the same lifecycle as all session widgets: sessions create, update, and delete them, and the viewport only controls temporary visibility.

Updating an Operator widget briefly reveals it in the Matrix-rain panel. The title bar shows the lowercase `operator` label followed by one square indicator per visible Operator widget. Hovering the Operator label may reveal the current Operator status in a tooltip. Hovering a widget indicator may reveal that widget's title. Clicking an empty square selects and shows the widget; clicking the filled square hides the widget viewport. The existing Matrix-rain close button continues to hide the Operator/rain panel itself. When the widget viewport hides or no Operator widgets exist, the panel returns to Matrix rain.

## Reason

The Operator is an ambient companion, not a critical notification system. Reusing normal session widgets avoids a separate notification lifecycle while still giving the collapsed Operator a peripheral surface for timely, glanceable help.

## Consequences

Missing or ignoring the Matrix-rain widget viewport must not block any user journey. The authoritative widget state remains the Operator session's widget set, and deeper interaction routes through normal widget actions or the Operator session. Future clients may choose a different compact presentation, but should preserve the same separation between widget lifecycle and transient ambient visibility.

## Non-Goals

This does not introduce widget TTLs, dismissed states, or guaranteed notification delivery. It does not make the Matrix-rain panel an arbitrary model-drawn canvas independent of session widgets.
