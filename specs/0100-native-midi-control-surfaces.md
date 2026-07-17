# 0100-native-midi-control-surfaces

Status: accepted
Date: 2026-07-17
Area: tui
Scope: MIDI control surfaces invoke Construct TUI actions through learned, persistent mappings.

## Decision

Construct treats MIDI as a native TUI input source. A learned MIDI message
dispatches the same semantic action used by keyboard, mouse, palette, and
clickable controls; it does not synthesize operating-system keyboard events.

Mappings persist in a dedicated human-readable configuration file. Each
mapping identifies the device, message kind, one-based MIDI channel, control
number, value trigger, and semantic Construct action. Note edges and MIDI CC
value direction are explicit so note release does not double-trigger buttons
and a relative encoder can bind its two directions independently.

The configuration CLI owns device discovery, learning, inspection, and
removal. The ordinary TUI automatically opens the configured device when at
least one mapping exists. Missing devices and invalid configurations degrade
to a visible TUI status message rather than preventing Construct from opening.
The initial native backend is CoreMIDI on macOS; unsupported platforms expose
an explicit error without making their normal Construct build depend on a
system MIDI/audio library.

## Reason

Control surfaces should operate Construct without terminal focus, desktop
Accessibility permission, or a second application translating gestures into
keystrokes. Dispatching semantic actions also preserves the behavior of every
input surface as actions evolve and avoids encoding profile-specific key
chords into MIDI configuration.

MIDI notes and controls emit both edges or changing values. Persisting trigger
semantics prevents a single physical gesture from firing twice and supports
the OP–XY's relative encoder mode without treating its values as absolute
positions.

## Consequences

- MIDI affects the active TUI instance and its current selection and focus.
- A TUI reads mappings when it opens; learning in another process takes effect
  the next time that TUI opens.
- Multiple controls may map to one action, and the two directions of one CC
  may map to different actions.
- MIDI clock, active sensing, SysEx, and unsupported message kinds never become
  TUI actions.
- Device output and status-light feedback are outside this decision; this is
  an input contract.

## Non-Goals

- Replacing the normal keymap.
- Sending global desktop shortcuts.
- Sequencing daemon operations while no Construct TUI is open.

## Examples

- Note-on 60 on channel 16 can open a new session; its note-off is ignored.
- CC 14 values 1–63 can select the next session, while values 65–127 from the
  same CC select the previous session.
- A disconnected learned device leaves the TUI usable and surfaces the
  connection problem in its status line.
