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

Construct detects `[1]` through `[8]` at the beginning of session titles. If
multiple sessions claim the same number, the one with the latest activity is
selected.

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
direction. CC 0 and CC 1 are currently unassigned.

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
numbers as the one-based numbers shown on the device and aggregates the state
of the eight sessions assigned to `[1]`–`[8]`, independent of TUI focus. Hidden,
archived, program, and unassigned sessions do not affect it. Construct sends
immediate scene changes using CC 85. Scene and transport encode attention and
activity independently:

- Scene 1 with MIDI Stop when no assigned session is active or needs attention.
- Scene 1 with MIDI Start when one or more assigned sessions are pending or running
  and none needs attention.
- Scene 2 with MIDI Start when one or more assigned sessions are pending or running
  and one or more needs attention.
- Scene 2 with MIDI Stop when one or more assigned sessions need attention but none
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

Synth tracks 1–4 mirror session slots `[1]`–`[4]`, using the same idle, running,
and attention envelopes independently of split placement or keyboard focus.
All four primary synth parameters move together. Their default targets are CC
12–15; choose another starting CC from the OP-XY track-parameter range when the
template uses a different engine or preferred visual controls.

Construct sends MIDI Start/Stop for transport but deliberately leaves timing to
the OP-XY's internal clock. Animation updates are limited to five batched packets
per second. Avoiding a continuous external clock and batching all animated
track volumes keeps Bluetooth MIDI traffic low enough for long-running use.
Scene defaults can be edited in `midi.toml`:

```toml
[op_xy.feedback]
enabled = true
normal_scene = 1
attention_scene = 2
track_activity_cc = 12
```

`track_activity_cc` is the first of four consecutive parameter CCs. The legacy
name `split_activity_cc` is still accepted when reading an existing profile.

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
