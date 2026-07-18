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

Pressing a session key places that session in the pane addressed by the
current OP-XY track without stealing keyboard focus. An arrow key focuses that
pane and dispatches the corresponding native TUI arrow. The first Enter press
focuses an unfocused pane; another Enter press acts on the focused pane.

### Scene feedback

The learned profile enables feedback by default. Construct treats OP-XY scene
numbers as the one-based numbers shown on the device and sends immediate scene
changes using CC 85:

- Scene 1 while an assigned session is running.
- Scenes 3 and 4 alternating when an assigned session needs attention.
- MIDI Stop when no assigned session is running or needs attention.

While feedback is active Construct sends MIDI Start and a 24-PPQN clock at 120
BPM so the template's sequencer LEDs animate. These defaults can be edited in
`midi.toml`:

```toml
[op_xy.feedback]
enabled = true
working_scene = 1
attention_scene_a = 3
attention_scene_b = 4
clock_bpm = 120.0
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
