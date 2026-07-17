# MIDI control surfaces

On macOS, Construct can learn MIDI notes, buttons, pads, and encoders as native TUI
controls. MIDI actions go directly through Construct's action dispatcher, so
the terminal does not need desktop focus and no keyboard-emulation or
Accessibility permission is involved.

The first native backend is CoreMIDI on macOS. Other platforms report that the
feature is unsupported instead of acquiring a system audio-library dependency.

## OP–XY setup

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
