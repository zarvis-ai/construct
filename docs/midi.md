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
mode. Link instrument tracks 1–4 to external-MIDI tracks 5–8 and give those
external tracks four distinct MIDI channels. The four channels address the
visible Construct split panes in reading order: left-to-right, then
top-to-bottom.

Stop the OP-XY sequencer, connect Bluetooth or USB MIDI, then run:

```sh
construct midi op-xy-learn --device OP-XY
```

The wizard captures each pane track's channel and first-key anchor, eight black
session keys, four arrow keys, Enter, and the white note reserved for sequencer
display. Per-track anchors normalize octave differences between linked OP-XY
tracks. The result is stored under `[op_xy]` in `midi.toml`; normal learned
mappings can coexist with it.

Prefix a session title with its black-key slot number:

```text
[1] primary implementation
[2] test runner
[8] documentation
```

Construct detects `[1]` through `[8]` at the beginning of session titles. If
multiple sessions claim the same number, the one with the latest activity is
selected.

Every recognized OP-XY key first focuses the pane addressed by the current
track and then performs its action. Session keys switch that pane's session,
arrows dispatch the corresponding native TUI arrow, and Enter acts on the now
focused pane. The reserved sequencer-display no-op does not change focus.

White keys 1–6 can insert user-defined prompt text. Like every other recognized
track key, a prompt key first focuses the split pane addressed by that track.
Construct then pastes the configured text into that pane's session input without
submitting it, leaving it available to edit or send with the Enter key. Assign
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

Existing learned profiles do not need to be relearned. Construct derives the
six white-key notes from the profile's first-black-key anchor and applies the
same per-track octave normalization used by the session keys.

Auxiliary track 3 can provide focus-sensitive generic controls on MIDI channel
10. Its third encoder (CC 2) sends Up/Down, and its fourth encoder (CC 3) sends
scroll up/down. Because OP-XY reports absolute values, Construct uses the first
value received from each encoder only to establish its position. Later changes
produce one action per MIDI message; increasing values move or scroll down and
decreasing values move or scroll up. Crossing between 127 and 0 preserves the
physical direction. CC 0 and CC 1 are currently unassigned.

The defaults can be changed in `midi.toml`:

```toml
[op_xy.aux]
enabled = true
channel = 10
arrow_cc = 2
scroll_cc = 3
```

### Scene feedback

The learned profile enables feedback by default. Construct treats OP-XY scene
numbers as the one-based numbers shown on the device and aggregates the state
of the eight sessions assigned to `[1]`–`[8]`, independent of TUI focus. Hidden,
archived, program, and unassigned sessions do not affect it. Construct sends
immediate scene changes using CC 85:

- Scene 2 with MIDI Start when any assigned non-terminal session has the blue
  attention dot. Attention takes precedence over ordinary activity.
- Otherwise, Scene 1 with MIDI Start when any assigned session is pending or running.
- Scene 1 with MIDI Stop when no session needs attention or is active.

The OP-XY mixer provides an eight-session activity overview independently of
the aggregate scene. Mixer tracks 1–8 correspond to title slots
`[1]`–`[8]`. Construct drives CC 7 (track volume) with three visual states:

- Idle or terminal: fixed at 0.
- Pending or running: gentle motion between 25% and 40%.
- Blue attention dot: damped bounce between 30% and 70%, taking precedence
  over running.

Multiple active and attention slots animate together. Exiting Construct resets
all eight track volumes to zero.

Synth tracks 1–4 independently mirror the sessions shown in split panes 1–4,
using the same idle, running, and attention envelopes. This indicator is based
on each pane's session rather than keyboard focus. All four primary synth
parameters move together. Their default targets are CC 12–15; choose another
starting CC from the OP-XY track-parameter range when the template uses a
different engine or preferred visual controls.

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
split_activity_cc = 12
```

`split_activity_cc` is the first of four consecutive parameter CCs.

Scenes store track volume and mute state, so the Construct template should use
identical volume/mute settings in every feedback scene. Disable MIDI echo on
the OP-XY, and reserve the learned no-op note exclusively for the display
patterns.

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
