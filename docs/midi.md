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

### Scene feedback

The learned profile enables feedback by default. Construct treats OP-XY scene
numbers as the one-based numbers shown on the device and sends immediate scene
changes using CC 85:

- Scene 1 with MIDI Start while the focused session is running.
- Scene 1 with MIDI Stop for a completed, idle, paused, awaiting-input, or
  errored focused session, and whenever focus is outside a session view.
- Scene 2 with MIDI Start while a non-terminal focused session has
  the blue attention dot.

The OP-XY mixer provides an eight-session activity overview independently of
the focused-session scene. Mixer tracks 1–8 correspond to title slots
`[1]`–`[8]`. Construct drives CC 7 (track volume) with three visual states:

- Idle or terminal: fixed at 0.
- Pending or running: gentle motion between 25% and 40%.
- Blue attention dot: damped bounce between 30% and 70%, taking precedence
  over running.

Multiple active and attention slots animate together. Exiting Construct resets
all eight track volumes to zero.

Construct sends MIDI Start/Stop for transport but deliberately leaves timing to
the OP-XY's internal clock. Mixer updates are limited to five batched packets
per second. Avoiding a continuous external clock and batching all animated
track volumes keeps Bluetooth MIDI traffic low enough for long-running use.
Scene defaults can be edited in `midi.toml`:

```toml
[op_xy.feedback]
enabled = true
normal_scene = 1
attention_scene = 2
```

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
