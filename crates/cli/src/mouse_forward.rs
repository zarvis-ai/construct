//! Encode terminal mouse events for forwarding into a child PTY.
//!
//! When a child program (e.g. Claude Code in fullscreen) requests mouse
//! tracking via DECSET (`?1000h`/`?1002h`/`?1003h`, plus an encoding like
//! SGR `?1006h`), it expects the host terminal to translate physical mouse
//! events into the matching escape-sequence reports. The construct TUI sits
//! between the user's terminal and the child, so it has to do that
//! translation itself and pipe the bytes down the PTY — otherwise the child
//! (which has taken over scroll/click handling) sees nothing.
//!
//! This module turns a [`crossterm`] [`MouseEvent`] (already mapped to
//! 1-based, pane-local cell coordinates by the caller) into the byte sequence
//! the child asked for, honoring both the protocol *mode* (which events are
//! reportable) and the *encoding* (how the report is framed).

use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use vt100::{MouseProtocolEncoding, MouseProtocolMode};

/// Encode `ev` into the report a child expects under `mode`/`encoding`.
///
/// `col`/`row` are 1-based cell coordinates relative to the child's screen
/// (i.e. the pane's content area, borders already stripped). Returns `None`
/// when the event should not be reported under the active mode — the caller
/// can then fall back to its own handling of that event.
pub fn encode(
    ev: &MouseEvent,
    col: u16,
    row: u16,
    mode: MouseProtocolMode,
    encoding: MouseProtocolEncoding,
) -> Option<Vec<u8>> {
    if mode == MouseProtocolMode::None {
        return None;
    }

    // Low button code, whether this is a release, and whether it carries the
    // "motion" flag. Wheel events are modeled as button presses with bit 6
    // (0x40) set — 64 = up, 65 = down, 66/67 = left/right.
    let (mut cb, release, motion) = match ev.kind {
        MouseEventKind::Down(b) => (button_code(b), false, false),
        MouseEventKind::Up(b) => (button_code(b), true, false),
        MouseEventKind::Drag(b) => (button_code(b), false, true),
        MouseEventKind::Moved => (3u8, false, true), // motion with no button held
        MouseEventKind::ScrollUp => (64u8, false, false),
        MouseEventKind::ScrollDown => (65u8, false, false),
        MouseEventKind::ScrollLeft => (66u8, false, false),
        MouseEventKind::ScrollRight => (67u8, false, false),
    };
    let is_wheel = cb & 0x40 != 0;

    // Drop events the active mode does not report. Wheel events are always
    // forwarded (every real terminal reports the wheel whenever any tracking
    // mode is on), so they bypass the motion filters below.
    match mode {
        MouseProtocolMode::None => return None,
        // X10 (`?9h`): button presses only — no releases, no motion.
        MouseProtocolMode::Press => {
            if !is_wheel && (release || motion) {
                return None;
            }
        }
        // VT200 (`?1000h`): presses + releases, but no motion.
        MouseProtocolMode::PressRelease => {
            if !is_wheel && motion {
                return None;
            }
        }
        // Button-event tracking (`?1002h`): motion only while a button is held.
        MouseProtocolMode::ButtonMotion => {
            if matches!(ev.kind, MouseEventKind::Moved) {
                return None;
            }
        }
        // Any-event tracking (`?1003h`): report everything.
        MouseProtocolMode::AnyMotion => {}
    }

    if motion {
        cb += 0x20; // motion flag (bit 5)
    }
    cb += modifier_bits(ev.modifiers);

    Some(match encoding {
        MouseProtocolEncoding::Sgr => encode_sgr(cb, col, row, release),
        MouseProtocolEncoding::Utf8 => encode_utf8(cb, col, row, release),
        MouseProtocolEncoding::Default => encode_x10(cb, col, row, release),
    })
}

fn button_code(b: MouseButton) -> u8 {
    match b {
        MouseButton::Left => 0,
        MouseButton::Middle => 1,
        MouseButton::Right => 2,
    }
}

fn modifier_bits(m: KeyModifiers) -> u8 {
    let mut bits = 0;
    if m.contains(KeyModifiers::SHIFT) {
        bits += 4;
    }
    if m.contains(KeyModifiers::ALT) {
        bits += 8;
    }
    if m.contains(KeyModifiers::CONTROL) {
        bits += 16;
    }
    bits
}

/// SGR encoding (`?1006h`): `ESC [ < Cb ; Cx ; Cy {M|m}`, where the final
/// byte is `M` for press/motion/wheel and `m` for release. The real button is
/// preserved on release (unlike the legacy encodings). This is what Claude
/// Code's fullscreen mode requests.
fn encode_sgr(cb: u8, col: u16, row: u16, release: bool) -> Vec<u8> {
    let final_byte = if release { 'm' } else { 'M' };
    format!("\x1b[<{cb};{col};{row}{final_byte}").into_bytes()
}

/// Legacy single-byte encoding: `ESC [ M Cb Cx Cy`, each value offset by 32.
/// Releases collapse the low two bits to 3 ("button released, which unknown")
/// and coordinates saturate at 223 (the 255 ceiling minus the +32 offset).
fn encode_x10(cb: u8, col: u16, row: u16, release: bool) -> Vec<u8> {
    let cb = release_low_bits(cb, release);
    vec![
        0x1b,
        b'[',
        b'M',
        cb.wrapping_add(32),
        x10_coord(col),
        x10_coord(row),
    ]
}

/// UTF-8 encoding (`?1005h`): like the legacy encoding, but coordinates beyond
/// 95 are emitted as UTF-8 code points instead of raw bytes, lifting the 223
/// cap. Rarely requested in practice.
fn encode_utf8(cb: u8, col: u16, row: u16, release: bool) -> Vec<u8> {
    let cb = release_low_bits(cb, release);
    let mut out = vec![0x1b, b'[', b'M', cb.wrapping_add(32)];
    push_utf8_coord(&mut out, col);
    push_utf8_coord(&mut out, row);
    out
}

fn release_low_bits(cb: u8, release: bool) -> u8 {
    if release {
        (cb & !0b11) | 0b11
    } else {
        cb
    }
}

fn x10_coord(v: u16) -> u8 {
    (v.min(223) as u8).wrapping_add(32)
}

fn push_utf8_coord(out: &mut Vec<u8>, v: u16) {
    let v = v.saturating_add(32);
    match char::from_u32(v as u32) {
        Some(c) => {
            let mut buf = [0u8; 4];
            out.extend_from_slice(c.encode_utf8(&mut buf).as_bytes());
        }
        None => out.push(b' '),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ev(kind: MouseEventKind) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers: KeyModifiers::NONE,
        }
    }

    fn ev_mods(kind: MouseEventKind, modifiers: KeyModifiers) -> MouseEvent {
        MouseEvent {
            kind,
            column: 0,
            row: 0,
            modifiers,
        }
    }

    #[test]
    fn disabled_mode_emits_nothing() {
        let out = encode(
            &ev(MouseEventKind::ScrollUp),
            5,
            10,
            MouseProtocolMode::None,
            MouseProtocolEncoding::Sgr,
        );
        assert_eq!(out, None);
    }

    #[test]
    fn sgr_scroll_up() {
        let out = encode(
            &ev(MouseEventKind::ScrollUp),
            5,
            10,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        );
        assert_eq!(out, Some(b"\x1b[<64;5;10M".to_vec()));
    }

    #[test]
    fn sgr_scroll_down() {
        let out = encode(
            &ev(MouseEventKind::ScrollDown),
            1,
            1,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        );
        assert_eq!(out, Some(b"\x1b[<65;1;1M".to_vec()));
    }

    #[test]
    fn sgr_left_press_and_release() {
        let press = encode(
            &ev(MouseEventKind::Down(MouseButton::Left)),
            3,
            4,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        );
        assert_eq!(press, Some(b"\x1b[<0;3;4M".to_vec()));
        let release = encode(
            &ev(MouseEventKind::Up(MouseButton::Left)),
            3,
            4,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        );
        // SGR keeps the real button code on release; only the final byte flips.
        assert_eq!(release, Some(b"\x1b[<0;3;4m".to_vec()));
    }

    #[test]
    fn sgr_right_button_with_ctrl() {
        let out = encode(
            &ev_mods(
                MouseEventKind::Down(MouseButton::Right),
                KeyModifiers::CONTROL,
            ),
            2,
            2,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        );
        // right = 2, +ctrl (16) = 18.
        assert_eq!(out, Some(b"\x1b[<18;2;2M".to_vec()));
    }

    #[test]
    fn sgr_drag_sets_motion_bit() {
        let out = encode(
            &ev(MouseEventKind::Drag(MouseButton::Left)),
            7,
            8,
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
        );
        // left = 0, +motion (32) = 32.
        assert_eq!(out, Some(b"\x1b[<32;7;8M".to_vec()));
    }

    #[test]
    fn press_release_mode_drops_plain_motion() {
        let out = encode(
            &ev(MouseEventKind::Moved),
            1,
            1,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Sgr,
        );
        assert_eq!(out, None);
    }

    #[test]
    fn button_motion_mode_drops_buttonless_motion_but_keeps_drag() {
        let moved = encode(
            &ev(MouseEventKind::Moved),
            1,
            1,
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
        );
        assert_eq!(moved, None);
        let drag = encode(
            &ev(MouseEventKind::Drag(MouseButton::Left)),
            1,
            1,
            MouseProtocolMode::ButtonMotion,
            MouseProtocolEncoding::Sgr,
        );
        assert!(drag.is_some());
    }

    #[test]
    fn x10_mode_drops_release_but_keeps_wheel() {
        let release = encode(
            &ev(MouseEventKind::Up(MouseButton::Left)),
            1,
            1,
            MouseProtocolMode::Press,
            MouseProtocolEncoding::Default,
        );
        assert_eq!(release, None);
        let wheel = encode(
            &ev(MouseEventKind::ScrollUp),
            1,
            1,
            MouseProtocolMode::Press,
            MouseProtocolEncoding::Default,
        );
        assert!(wheel.is_some());
    }

    #[test]
    fn default_encoding_offsets_by_32() {
        // left press at col 1, row 1 → button 0+32, coords 1+32.
        let out = encode(
            &ev(MouseEventKind::Down(MouseButton::Left)),
            1,
            1,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
        );
        assert_eq!(out, Some(vec![0x1b, b'[', b'M', 32, 33, 33]));
    }

    #[test]
    fn default_encoding_release_collapses_button_bits() {
        let out = encode(
            &ev(MouseEventKind::Up(MouseButton::Left)),
            1,
            1,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
        );
        // release low bits → 3, +32 = 35.
        assert_eq!(out, Some(vec![0x1b, b'[', b'M', 35, 33, 33]));
    }

    #[test]
    fn default_encoding_saturates_large_coords() {
        let out = encode(
            &ev(MouseEventKind::Down(MouseButton::Left)),
            500,
            500,
            MouseProtocolMode::PressRelease,
            MouseProtocolEncoding::Default,
        )
        .unwrap();
        // 223 + 32 = 255 ceiling for both coordinate bytes.
        assert_eq!(out[4], 255);
        assert_eq!(out[5], 255);
    }
}
