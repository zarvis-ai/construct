# 0101-op-xy-split-controller

Status: accepted
Date: 2026-07-17
Area: tui
Scope: An OP-XY Instrument-mode template controls four Construct split panes and receives global session-status feedback.

## Decision

Construct supports an opt-in OP-XY profile alongside generic learned MIDI
mappings. Four learned MIDI channels address visible split panes in visual
reading order. Each pane track also has a learned first-key anchor so differing
track octaves normalize to one physical-key layout. Eight learned notes select
addressed pane; learned arrow and Enter notes dispatch native TUI input to that
pane. A reserved sequencer-display note is always consumed as a no-op.

Session titles beginning with `[1]` through `[8]` assign those sessions to the
corresponding hardware slots. If multiple titles claim one slot, the session
with the latest activity wins; creation time breaks the absence of activity,
and session id makes exact ties deterministic. Selecting a slot changes the
addressed pane without taking focus. Enter focuses an unfocused addressed pane
before it can dispatch Enter to that pane.

Feedback is global rather than per-pane. Assigned-session state drives
preconfigured OP-XY scenes: a working scene while any assigned session runs,
two alternating attention scenes while any assigned session needs the user,
and stopped transport otherwise. Construct supplies MIDI real-time transport
and clock while feedback animation is active.

## Reason

OP-XY Instrument mode exposes linked-track notes, its sequencer, and MIDI
parameter reception simultaneously, while Controller Mode loses track and
mode buttons. Channel-addressed panes preserve the physical track model, and
persistent session slots avoid coupling hardware keys to a changing session
list order.

OP-XY does not expose documented direct LED control, MMC reception, record
arming, parameter readback, or incoming virtual-button commands. Preconfigured
scenes plus standard CC and real-time MIDI provide useful feedback without
depending on proprietary SysEx.

## Consequences

- The OP-XY project template, key layout, channels, and display no-op are
  learned rather than hard-coded.
- Pane numbering follows current visual geometry, not split-tree creation
  order.
- Missing panes and unresolved session slots produce visible status messages and do
  not retarget another pane or session.
- Scene feedback takes ownership of OP-XY transport and incoming clock while
  assigned Construct work is active.
- Feedback scenes must keep volume and mute settings consistent because those
  values are stored by OP-XY scenes.
- MIDI echo must be disabled to prevent linked-track feedback loops.

## Non-Goals

- Reading the OP-XY's current parameter or sequencer state.
- Direct control of OP-XY button LEDs.
- Remote OP-XY record arming.
- Independent simultaneous LED animations for all four Construct panes.
