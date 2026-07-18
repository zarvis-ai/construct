# 0101-op-xy-split-controller

Status: accepted
Date: 2026-07-17
Area: tui
Scope: An OP-XY Instrument-mode template selects eight sessions, places them in four split panes, and receives status feedback.

## Decision

Construct supports an opt-in OP-XY profile alongside generic learned MIDI
mappings. MIDI channels 1–8 address session title slots `[1]`–`[8]`. Each track
has a learned first-key anchor so differing octaves normalize to one physical
key layout. Black keys 1–4 place the channel-selected session in the
corresponding visible split pane in reading order. Black key 5 cycles focus
among that session's split, list row, and lineage section. Black keys 6–8 are
Escape, the sequencer-display no-op, and Backspace respectively. Learned arrow
and Enter notes dispatch native TUI input to the selected session's visible
split.

Session titles beginning with `[1]` through `[8]` assign those sessions to the
corresponding hardware slots. If multiple titles claim one slot, the session
with the latest activity wins; creation time breaks the absence of activity,
and session id makes exact ties deterministic. Session commands locate and
focus the split already displaying the channel-selected session; they never
silently target a different session. The reserved black-key no-op never changes
focus.

White keys 1–6 are configurable prompt slots. A configured key focuses the
split containing the channel-selected session and inserts text into its composer
without submitting it. Empty or missing prompt slots do nothing. Their notes
are derived from the learned first-black-key anchor and normalized across
session-track octaves, so adding prompts does not require relearning the
controller.

The Auxiliary 3 external-MIDI track reuses every learned black- and white-key
meaning but derives session identity from the currently focused split because
its channel does not represent a numbered session. Its note channels are
configurable and default to MIDI channel 10. Its existing absolute-encoder
controls remain independent of this
note routing. Auxiliary 2 is reserved for OP-XY's internal Punch-In FX engine
and does not emit a native MIDI control stream for Construct.

The Auxiliary 3 scroll encoder follows TUI focus. A focused sidebar scrolls its
session rows or lineage diagram, a focused document or dynamic panel scrolls
its own content, and a focused session scrolls chat or terminal history. It
must not route every scroll event to the session pane regardless of focus.

Scene and transport feedback aggregate the eight sessions resolved into
`[1]`–`[8]` hardware slots independently of TUI focus. Hidden, archived,
program, and unassigned sessions do not contribute. Scene encodes attention:
Scene 2 is selected when any assigned session needs attention, otherwise Scene
1 is selected. Transport independently encodes activity: it runs when any
assigned session is pending or running and stops otherwise. The four possible
combinations are therefore Scene 1 stopped, Scene 1 running, Scene 2 stopped,
and Scene 2 running.
Construct supplies MIDI real-time Start/Stop while OP-XY retains its internal
clock. Session-state and attention-marker changes update feedback as part of
handling the event that changed them; feedback must not depend on an animation
timer.

Mixer tracks 1–8 are a global activity overview for assigned session slots
`[1]`–`[8]`, independent of pane focus. Idle and terminal slots have track
volume zero. Pending and running slots move gently between 25% and 40%. A blue
attention marker takes precedence and animates that slot with a damped bounce
between 30% and 70%. Simultaneous active and attention slots animate together.
Feedback shutdown resets all eight volumes to zero.

Synth tracks 1–4 are a second activity display for session slots `[1]`–`[4]`,
independent of split placement and focus. They use the same idle, running, and
attention envelopes. The four primary synth parameters move together. Their
starting CC is configurable and defaults to parameter 1, producing CC 12–15.

Auxiliary track 3 supplies generic, focus-sensitive navigation on MIDI channel
10. Absolute CC 2 maps value changes to Up/Down and absolute CC 3 maps changes
to scroll up/down. Each encoder's first received value calibrates its independent
position without producing an action. Subsequent messages produce one action
in the shortest direction around the 0–127 range, so boundary crossings do not
reverse the control unexpectedly. CC 0 and CC 1 remain unassigned.

Bluetooth feedback traffic is bounded: animation is at most five packets per
second, with all mixer-volume and session-track synth-parameter messages for a
frame batched into one packet.
Construct does not stream MIDI clock because OP-XY can start its sequencer from
its internal clock, and sustained clock plus per-track packets can lock its BLE
receive path until the device is power-cycled.

## Reason

OP-XY Instrument mode exposes linked-track notes, its sequencer, and MIDI
parameter reception simultaneously, while Controller Mode loses track and
mode buttons. Channel-addressed sessions keep the TUI `[N]` title, track button,
mixer track, and synth feedback identity consistent. Persistent session slots
avoid coupling hardware keys to a changing session-list order.

OP-XY does not expose documented direct LED control, MMC reception, record
arming, parameter readback, or incoming virtual-button commands. Preconfigured
scenes plus standard CC and real-time MIDI provide useful feedback without
depending on proprietary SysEx.

## Consequences

- MIDI channels 1–8 have fixed session-slot meaning; the key layout, octave
  anchors, and display no-op are learned.
- Instrument-to-MIDI linking is optional and affects OP-XY playability and
  synth visualization, not Construct's session identity.
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
