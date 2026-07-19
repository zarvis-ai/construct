# MIDI control surfaces

On macOS, Construct can learn MIDI notes, buttons, pads, and encoders as native TUI
controls. MIDI actions go directly through Construct's action dispatcher, so
the terminal does not need desktop focus and no keyboard-emulation or
Accessibility permission is involved.

The first native backend is CoreMIDI on macOS. Other platforms report that the
feature is unsupported instead of acquiring a system audio-library dependency.

## Generic OP–XY controller-mode setup

1. Connect the OP–XY over USB-C, or pair it with macOS as a Bluetooth MIDI
   device.
2. On the OP–XY, press `com`, then `M2` to enter MIDI controller mode.
3. Optionally dedicate channel 16: hold `shift` and turn the dark gray encoder.
4. For navigation encoders, select relative knob behavior: hold `shift` and
   turn the mid-gray encoder.
5. Confirm that Construct sees it:

   ```sh
   construct midi devices
   ```

## Dedicated OP-XY split controller

Construct also supports an OP-XY project template that stays in Instrument
mode. MIDI channels 1–8 directly address sessions `[1]`–`[8]`. Instrument
tracks 1–4 may optionally be linked to external-MIDI tracks configured for
channels 1–4; linking is only needed when those synth tracks should remain
playable and show Construct's parameter animation. MIDI tracks for channels
5–8 can be used directly.

Stop the OP-XY sequencer, connect Bluetooth or USB MIDI, then run:

```sh
construct midi op-xy-learn --device OP-XY
```

The wizard verifies channels 1–8 and captures each track's first-key anchor,
the eight black keys, four arrow keys, and Enter. Per-track anchors normalize
octave differences between linked OP-XY tracks. The result is stored under
`[op_xy]` in `midi.toml`; normal learned mappings can coexist with it.

Prefix a session title with its black-key slot number:

```text
[1] primary implementation
[2] test runner
[8] documentation
```

Construct detects `[1]` through `[8]` at the beginning of session titles, with
or without a following space (`[1] primary` and `[1]primary` are equivalent).
Only live top-level user sessions can claim a slot; subagent and archived
sessions are ignored. If multiple eligible sessions claim the same number, the
one with the latest activity is selected.

The MIDI channel selects the session. Black keys then choose the destination
or action:

- Black keys 1–4 display the selected session in split panes 1–4 and focus it.
- Black key 5 cycles focus through the split containing the selected session,
  its session-list row, and its lineage section. If it is not already shown in
  a split, the cycle starts at the list.
- Black key 6 sends Escape.
- Black key 7 is the sequencer-display no-op and never changes focus.
- Black key 8 sends Backspace.

Arrows, Enter, and prompt keys locate the channel-selected session in a visible
split, focus that split, and act on it. If the session is not displayed, these
commands report that condition rather than targeting another session.

White keys 1–6 can insert user-defined prompt text. Construct first focuses the
split containing the channel-selected session, then pastes the configured text
into that session's input without submitting it, leaving it available to edit
or send with the Enter key. Assign
the keys in order under `[op_xy]`; use an empty string to leave a position
unassigned:

```toml
[op_xy]
prompt_texts = [
  "Review the current changes.",
  "Run the relevant tests and fix failures.",
  "Summarize progress and remaining work.",
  "",
  "",
  "",
]
```

After learning the session-centric profile, prompt text can change without
relearning. Construct derives the six white-key notes from the profile's
first-black-key anchor and applies the same per-track octave normalization.
Profiles learned with the older pane-centric layout should be learned again.

Auxiliary track 3 can send the same black- and white-key controls on MIDI
channel 10. Because it has no `[N]` session channel, it uses the session in the
currently focused split. Split selection, focus cycling, Escape, Backspace,
custom prompts, arrows, Enter, and the reserved no-op retain the learned
profile's note meanings. Keep Aux 3 at the same note octave as the learned
reference track.

Auxiliary track 2 is OP-XY's internal Punch-In FX track. Its keys and encoders
control that effect engine and do not produce external MIDI for Construct to
receive, so it cannot provide this focused-pane control path.

Auxiliary track 3 also provides focus-sensitive generic encoder controls on
MIDI channel 10. Its third encoder (CC 2) sends Up/Down, and its fourth encoder
(CC 3) sends scroll up/down. Because OP-XY reports absolute values, Construct
uses the first value received from each encoder only to establish its position.
Later changes produce one action per MIDI message; increasing values move or
scroll down and decreasing values move or scroll up. The scroll encoder follows
TUI focus: it scrolls the session list, focused lineage diagram, program
document, dynamic panel, help, chat, or terminal history rather than always
targeting the session pane. Crossing between 127 and 0 preserves the physical
direction.

On every session track (MIDI channels 1–8) and Auxiliary 3 (channel 10), the
OP-XY Bank value also sends Up/Down and the Program value sends focused-surface
scroll up/down. These values are absolute and independently calibrated per
channel and control: the first Bank and first Program message received on each
channel establish baselines without acting. Increasing Bank or Program moves
or scrolls down; decreasing values move or scroll up. This lets the same two
track-menu controls navigate Construct regardless of which track is selected.

The participating channels and Bank Select CC can be changed under `[op_xy]`:

```toml
[op_xy]
navigation_channels = [1, 2, 3, 4, 5, 6, 7, 8, 10]
bank_cc = 0
```

The defaults can be changed in `midi.toml`:

```toml
[op_xy.aux]
enabled = true
channel = 10
focused_note_channels = [10]
arrow_cc = 2
scroll_cc = 3
```

### Scene feedback

The learned profile enables feedback by default. Construct treats OP-XY scene
numbers as the one-based numbers shown on the device. By default,
`aggregate_scope = "all"` uses the same session scope as Matrix Rain intensity:
every non-archived user session, including sessions without a `[N]` mapping.
Set it to `"mapped"` to aggregate only the eight sessions assigned to
`[1]`–`[8]`, independent of TUI focus. Subagents and orchestrator/system
sessions do not contribute. In the default all-session scope, transport uses
the same live activity signal as Matrix Rain: an active agent or recent PTY
output. Stale persisted `running` records therefore do not hold the sequencer
on. The mapped scope uses the assigned sessions' pending/running states.
Construct sends immediate scene changes using CC 85. Scene and transport encode
attention and activity independently:

- Scene 1 with MIDI Stop when no included session is active or needs attention.
- Scene 1 with MIDI Start when one or more included sessions are active
  and none needs attention.
- Scene 2 with MIDI Start when one or more included sessions are active
  and one or more needs attention.
- Scene 2 with MIDI Stop when one or more included sessions need attention but none
  is pending or running.

The OP-XY mixer provides an eight-session activity overview independently of
the aggregate scene. Mixer tracks 1–8 correspond to title slots
`[1]`–`[8]`. Construct drives CC 7 (track volume) with three visual states:

- Idle or terminal: fixed at 0.
- Pending or running: gentle motion between 25% and 40%.
- Blue attention dot: damped bounce between 30% and 70%, taking precedence
  over running.

Multiple active and attention slots animate together. Exiting Construct resets
all eight track volumes to zero.

Synth tracks 1–4 mirror session slots `[1]`–`[4]`, independently of split
placement or keyboard focus. All four primary synth parameters move together.
Their default targets are CC 12–15; choose another starting CC from the OP-XY
track-parameter range when the template uses a different engine or preferred
visual controls. Unlike the fixed mixer envelopes, the synth animation ranges
are configurable in `midi.toml` as percents of the 0–127 CC range:

- Pending or running: a smooth sweep between `active_range`
  (default `[25, 40]`).
- Blue attention dot: a bounce between `attention_range` (default `[30, 70]`)
  — a quick rise toward the maximum, a fall back to the minimum, then a hold
  at the minimum for several frames before the next bounce.

Mixer and synth animation is a burst, not a continuous stream: after each
activity change the motion plays a few full cycles, then freezes at steady
levels (refreshed every 30 seconds) until the next change. Sustained
streaming is what can lock the OP-XY's Bluetooth receive path, so
long-unchanged activity intentionally goes quiet.

Widen both ranges to make the OP-XY synth graphics move more visibly, e.g.
`active_range = [10, 90]` and `attention_range = [10, 90]`. These keys affect
only the synth parameter animation; mixer CC 7 volumes always use the fixed
25–40% and 30–70% envelopes above.

The sequencer tempo follows fleet activity on the same curve as the TUI's
Matrix Rain intensity: each live-active session adds a quarter of the
configured `tempo_range` (default `[60, 180]` BPM), saturating at four — so
with the defaults the sequencer LEDs chase at 60 BPM when everything is idle,
120 BPM with two sessions working, and 180 BPM with four or more. Tempo is
set with OP-XY's CC 80 (no MIDI clock is streamed), rides inside the existing
global-state packet so it adds no extra Bluetooth traffic, and is clamped to
the device's 40–220 BPM scale. Set `tempo_range = [0, 0]` to leave the
device's tempo alone entirely, or two equal values to pin it.

Construct sends MIDI Start/Stop for transport but deliberately leaves timing to
the OP-XY's internal clock. Animation dynamically slows as more mixer tracks
and synth parameters are active, with a ceiling of sixteen decoded CC messages
per second. Frames remain batched, and queued state changes are coalesced to the
newest state if Bluetooth applies backpressure. Avoiding a continuous external
clock and bounding the actual parameter workload keeps Bluetooth MIDI traffic
low enough for long-running use. Construct reasserts the global state every
two seconds so a silently dropped Bluetooth packet
self-recovers. Running transport is reasserted with MIDI Continue rather than
Start, preserving the playhead. If CoreMIDI reports a failed send, Construct
drops the connection and re-probes for the device every five seconds,
resynchronizing all feedback state from scratch once it reconnects — the
OP-XY may also be paired after Construct is already running and feedback
comes up on its own.
Scene defaults can be edited in `midi.toml`:

```toml
[op_xy.feedback]
enabled = true
aggregate_scope = "all" # or "mapped"
normal_scene = 1
attention_scene = 2
track_activity_cc = 12
# Synth-track animation ranges, as percents of 0–127:
active_range = [10, 90]
attention_range = [10, 90]
# Sequencer BPM at zero → four-plus live-active sessions ([0, 0] disables):
tempo_range = [60, 180]
```

`track_activity_cc` is the first of four consecutive parameter CCs. The legacy
name `split_activity_cc` is still accepted when reading an existing profile.
`aggregate_scope` changes only scene and transport feedback; mixer volumes and
synth parameters always represent the mapped `[1]`–`[8]` slots.

Scenes store track volume and mute state, so the Construct template should use
identical volume/mute settings in every feedback scene. Disable MIDI echo on
the OP-XY, and reserve black key 7 exclusively for the display patterns.

## Learn controls

List the available semantic actions:

```sh
construct midi actions
```

Then name an action and move or press its physical control:

```sh
construct midi learn next-session --device OP-XY
construct midi learn previous-session
construct midi learn new-session
construct midi learn approve
construct midi learn reject
```

The first learn stores the full device name. Later learns reuse it, so
`--device` is only needed when no device has been selected or the selector
would be ambiguous.

Note-on messages learn as `press` and ignore the matching release. Relative CC
values automatically learn as `increase` (1–63) or `decrease` (65–127). For a
button that emits CC 127 when pressed, override the relative-value inference:

```sh
construct midi learn interrupt --trigger high
```

Open or reopen the normal TUI after learning. It automatically connects to the
configured device and the mappings operate on that TUI's current selection.

## Inspect and remove mappings

```sh
construct midi mappings
construct midi forget interrupt
```

Mappings live in `midi.toml` under the directory printed by `construct paths`
as `config`. The file is deliberately readable and can be versioned or edited
by hand. `CONSTRUCT_CONFIG_DIR` relocates it along with the rest of Construct's
configuration.

If a configured device is absent or the configuration is invalid, the TUI
still opens and reports `MIDI disabled: …` in its status line.
