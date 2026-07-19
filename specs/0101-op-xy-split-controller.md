# 0101-op-xy-split-controller

Status: accepted
Date: 2026-07-18
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
split. When the lineage section holds keyboard focus, those notes instead act
on the section exactly as the equivalent keyboard keys: arrows move its
highlight, and Enter switches the previously focused split to the highlighted
lineage node and moves focus into it — the same behavior as clicking that node
with the mouse. They must not silently pull focus back to the split while the
section owns the keyboard. Prompt-slot notes keep targeting the session's
composer regardless of section focus.

Session titles beginning with `[1]` through `[8]` assign those sessions to the
corresponding hardware slots. The marker may be followed immediately by title
text or by whitespace. Only non-archived top-level user sessions are eligible;
subagents and hidden internal sessions cannot claim or displace a hardware
slot. If multiple eligible titles claim one slot, the session with the latest
activity wins; creation time breaks the absence of activity, and session id
makes exact ties deterministic. Session commands locate and focus the split
already displaying the channel-selected session; they never silently target a
different session. The reserved black-key no-op never changes focus.

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

Scene and transport feedback default to using the same session eligibility rule
as Matrix Rain intensity: every non-archived user session. A configurable
mapped scope instead aggregates only the eight sessions resolved into
`[1]`–`[8]` hardware slots independently of TUI focus. Subagents and
orchestrator/system sessions never contribute. Unassigned sessions contribute
only in the all-session scope. Mixer and synth feedback remain mapped to
hardware slots regardless of aggregate scope. Scene encodes attention:
Scene 2 is selected when any included session needs attention, otherwise Scene
1 is selected. Transport independently encodes activity. The all-session scope
uses Matrix Rain's live signal—an active agent or recent PTY output—so a stale
persisted Running state cannot hold transport on. The mapped scope uses the
resolved sessions' pending/running states. The four possible combinations are
therefore Scene 1 stopped, Scene 1 running, Scene 2 stopped, and Scene 2
running.
Construct supplies MIDI real-time Start/Stop while OP-XY retains its internal
clock. Session-state and attention-marker changes update feedback as part of
handling the event that changed them; feedback must not depend on an animation
timer.

Sequencer tempo encodes fleet activity volume. The count of live-active
sessions — the same fleet-wide Matrix Rain signal, in every aggregate scope —
runs through the rain intensity curve (each session a quarter step,
saturating at four) onto a configurable BPM range, so the sequencer's LED
chase speeds up as more sessions work and slows as the fleet quiets. Tempo is
set through the device's dedicated tempo controller message, never by
streaming MIDI clock, and it travels in the same packet as the scene
reasserts so activity-driven tempo adds no sustained Bluetooth traffic. The
range is clamped to the tempo values the device can express; a zeroed range
disables tempo control and equal bounds pin a fixed tempo. A tempo-tier
change refreshes the global state immediately, and reasserting tempo must
never restart or reposition the sequencer.

Mixer tracks 1–8 are a global activity overview for assigned session slots
`[1]`–`[8]`, independent of pane focus. Idle and terminal slots have track
volume zero. Pending and running slots move gently between 25% and 40%. A blue
attention marker takes precedence and animates that slot with a damped bounce
between 30% and 70%. Simultaneous active and attention slots animate together.
Feedback shutdown resets all eight volumes to zero.

Synth tracks 1–4 are a second activity display for session slots `[1]`–`[4]`,
independent of split placement and focus. The four primary synth parameters
move together. Their starting CC is configurable and defaults to parameter 1,
producing CC 12–15. Their animation ranges are configurable as percents of the
0–127 CC range: active sessions jump between three levels — minimum,
midpoint, maximum — within the configured bounds (default 25–40%), while
attention snap-bounces between them, leaping to the maximum in one frame and
falling back the next, with a pause at the minimum (default 30–70%). These
ranges apply only to the synth
parameters; mixer volumes always keep the fixed 25–40% / 30–70% envelopes.
The four parameters of each track play the same curve phase-offset by one
frame, so they show different levels at any moment — a wave across the synth
graphic.
While streaming, the jumps repeat continuously; how long streaming lasts is
governed by the Bluetooth burst rule below, with held synth values resting at
each curve's own configured minimum.

Auxiliary track 3 supplies generic, focus-sensitive navigation on MIDI channel
10. Absolute CC 2 maps value changes to Up/Down and absolute CC 3 maps changes
to scroll up/down. Each encoder's first received value calibrates its independent
position without producing an action. Subsequent messages produce one action
in the shortest direction around the 0–127 range, so boundary crossings do not
reverse the control unexpectedly.

OP-XY Bank Select and Program Change provide the same focus-sensitive
navigation on session channels 1–8 and Auxiliary 3 channel 10. Bank Select CC
0 maps increasing/decreasing values to Down/Up. Program Change maps
increasing/decreasing program numbers to focused-surface scroll down/up. Bank
and Program positions calibrate independently per channel, so the first message
for either control on a track establishes its baseline without acting. The
participating channels and Bank Select CC are configurable; changes retain the
shortest-direction boundary behavior used by the Aux 3 encoders.

Bluetooth feedback traffic is bounded by decoded CC work, not just packet
count. Animation dynamically slows as more mixer tracks and synth parameters
are visible so it emits at most sixteen CC messages per second; all messages
for a frame remain batched into one packet. Activity animation is a burst,
not a sustained stream: after an activity change the motion plays a full
cycle, then rests at steady held levels — with held attention
distinguishably louder than held activity — for a configurable interval
before replaying one heartbeat cycle, so long-running unchanged activity
still pulses periodically and held levels cannot go stale. Sustained
continuous streaming is what locks the OP-XY's Bluetooth receive path, so
the default rest keeps the duty cycle low; shortening the rest toward zero
approaches continuous streaming and consciously trades that wedge risk
back, and a zero rest is the explicit opt-in to continuous animation. State updates queued while the
transport is busy are coalesced to the newest snapshot so stale scene,
transport, mixer, and synth states are never replayed after backpressure clears.
The desired global state is reasserted after reported success because
Bluetooth delivery is not acknowledged: scenes are resent, stopped transport
receives Stop again, and running transport receives Continue so its playhead
is not reset. The reassert schedule decays: the first reassert follows a
state change within a bounded short interval, and while the state stays
unchanged the interval backs off to a bounded ceiling, so an unchanged fleet
settles to a near-silent trickle instead of a permanent fixed-rate drip. Any
global-state change resets the schedule and sends immediately. A transient
or silently dropped CoreMIDI send must not permanently freeze global feedback.

Feedback owns its device connection and self-heals it. A failed CoreMIDI send
marks the connection dead, and a dead or never-established connection is
re-attempted at a slow bounded cadence, so feedback starts working when the
device pairs after the TUI launched and comes back on its own after the
device reboots or Bluetooth drops. Reconnecting assumes nothing about device
state: previously asserted scene, transport, mixer, and synth values are
forgotten and the full state is resynchronized from scratch. Feedback startup
must not require the device to be present.
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
