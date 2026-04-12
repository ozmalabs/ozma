//! USB HID boot-protocol keyboard and mouse report types.
//!
//! # Keyboard report (8 bytes)
//! ```text
//! [0] modifier bitmask  (see [`ModifierBits`])
//! [1] reserved — always 0x00
//! [2..7] up to 6 simultaneous HID Usage IDs (0x00 = no key)
//! ```
//!
//! # Mouse report (4 bytes)
//! ```text
//! [0] button bitmask (bit 0=left, bit 1=right, bit 2=middle)
//! [1] X delta (signed i8, positive = right)
//! [2] Y delta (signed i8, positive = down)
//! [3] scroll wheel (signed i8, positive = up)
//! ```
//!
//! # Modifier byte bits (USB HID spec, boot-protocol keyboard)
//! ```text
//! bit 0  Left Ctrl    bit 4  Right Ctrl
//! bit 1  Left Shift   bit 5  Right Shift
//! bit 2  Left Alt     bit 6  Right Alt
//! bit 3  Left GUI     bit 7  Right GUI
//! ```
//!
//! HID Usage IDs are ported from `controller/keycodes.py` (`KEYCODE_TO_HID`).

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error, PartialEq, Eq)]
pub enum HidReportError {
    #[error("buffer must be exactly {expected} bytes, got {got}")]
    BadLength { expected: usize, got: usize },
    #[error("keyboard report already has 6 keys pressed (6KRO rollover)")]
    KeyRollover,
}

// ── Modifier bits ─────────────────────────────────────────────────────────────

/// Modifier key bitmask values for byte 0 of the HID keyboard boot report.
///
/// Ported from `controller/keycodes.py` `MODIFIER_BITS`.
pub struct ModifierBits;

impl ModifierBits {
    pub const LEFT_CTRL: u8   = 0x01;
    pub const LEFT_SHIFT: u8  = 0x02;
    pub const LEFT_ALT: u8    = 0x04;
    pub const LEFT_GUI: u8    = 0x08;
    pub const RIGHT_CTRL: u8  = 0x10;
    pub const RIGHT_SHIFT: u8 = 0x20;
    pub const RIGHT_ALT: u8   = 0x40;
    pub const RIGHT_GUI: u8   = 0x80;
}

// ── HID Usage IDs (keyboard page 0x07) ───────────────────────────────────────
//
// Ported from controller/keycodes.py KEYCODE_TO_HID.
// Named after Linux evdev KEY_* constants for easy cross-referencing.

/// USB HID Usage IDs for the keyboard page (0x07).
///
/// Values are identical to those in `controller/keycodes.py` `KEYCODE_TO_HID`.
#[allow(dead_code, non_upper_case_globals)]
pub mod KeyCode {
    pub const A: u8 = 0x04;
    pub const B: u8 = 0x05;
    pub const C: u8 = 0x06;
    pub const D: u8 = 0x07;
    pub const E: u8 = 0x08;
    pub const F: u8 = 0x09;
    pub const G: u8 = 0x0A;
    pub const H: u8 = 0x0B;
    pub const I: u8 = 0x0C;
    pub const J: u8 = 0x0D;
    pub const K: u8 = 0x0E;
    pub const L: u8 = 0x0F;
    pub const M: u8 = 0x10;
    pub const N: u8 = 0x11;
    pub const O: u8 = 0x12;
    pub const P: u8 = 0x13;
    pub const Q: u8 = 0x14;
    pub const R: u8 = 0x15;
    pub const S: u8 = 0x16;
    pub const T: u8 = 0x17;
    pub const U: u8 = 0x18;
    pub const V: u8 = 0x19;
    pub const W: u8 = 0x1A;
    pub const X: u8 = 0x1B;
    pub const Y: u8 = 0x1C;
    pub const Z: u8 = 0x1D;

    pub const KEY_1: u8 = 0x1E;
    pub const KEY_2: u8 = 0x1F;
    pub const KEY_3: u8 = 0x20;
    pub const KEY_4: u8 = 0x21;
    pub const KEY_5: u8 = 0x22;
    pub const KEY_6: u8 = 0x23;
    pub const KEY_7: u8 = 0x24;
    pub const KEY_8: u8 = 0x25;
    pub const KEY_9: u8 = 0x26;
    pub const KEY_0: u8 = 0x27;

    pub const ENTER: u8     = 0x28;
    pub const ESCAPE: u8    = 0x29;
    pub const BACKSPACE: u8 = 0x2A;
    pub const TAB: u8       = 0x2B;
    pub const SPACE: u8     = 0x2C;
    pub const MINUS: u8     = 0x2D;
    pub const EQUAL: u8     = 0x2E;
    pub const LEFT_BRACE: u8  = 0x2F;
    pub const RIGHT_BRACE: u8 = 0x30;
    pub const BACKSLASH: u8   = 0x31;
    pub const SEMICOLON: u8   = 0x33;
    pub const APOSTROPHE: u8  = 0x34;
    pub const GRAVE: u8       = 0x35;
    pub const COMMA: u8       = 0x36;
    pub const DOT: u8         = 0x37;
    pub const SLASH: u8       = 0x38;
    pub const CAPS_LOCK: u8   = 0x39;

    pub const F1: u8  = 0x3A;
    pub const F2: u8  = 0x3B;
    pub const F3: u8  = 0x3C;
    pub const F4: u8  = 0x3D;
    pub const F5: u8  = 0x3E;
    pub const F6: u8  = 0x3F;
    pub const F7: u8  = 0x40;
    pub const F8: u8  = 0x41;
    pub const F9: u8  = 0x42;
    pub const F10: u8 = 0x43;
    pub const F11: u8 = 0x44;
    pub const F12: u8 = 0x45;
    pub const F13: u8 = 0x68;
    pub const F14: u8 = 0x69;
    pub const F15: u8 = 0x6A;
    pub const F16: u8 = 0x6B;
    pub const F17: u8 = 0x6C;
    pub const F18: u8 = 0x6D;
    pub const F19: u8 = 0x6E;
    pub const F20: u8 = 0x6F;
    pub const F21: u8 = 0x70;
    pub const F22: u8 = 0x71;
    pub const F23: u8 = 0x72;
    pub const F24: u8 = 0x73;

    pub const PRINT_SCREEN: u8 = 0x46; // KEY_SYSRQ
    pub const SCROLL_LOCK: u8  = 0x47;
    pub const PAUSE: u8        = 0x48;
    pub const INSERT: u8       = 0x49;
    pub const HOME: u8         = 0x4A;
    pub const PAGE_UP: u8      = 0x4B;
    pub const DELETE: u8       = 0x4C;
    pub const END: u8          = 0x4D;
    pub const PAGE_DOWN: u8    = 0x4E;
    pub const RIGHT: u8        = 0x4F;
    pub const LEFT: u8         = 0x50;
    pub const DOWN: u8         = 0x51;
    pub const UP: u8           = 0x52;

    pub const NUM_LOCK: u8    = 0x53;
    pub const KP_SLASH: u8    = 0x54;
    pub const KP_ASTERISK: u8 = 0x55;
    pub const KP_MINUS: u8    = 0x56;
    pub const KP_PLUS: u8     = 0x57;
    pub const KP_ENTER: u8    = 0x58;
    pub const KP_1: u8        = 0x59;
    pub const KP_2: u8        = 0x5A;
    pub const KP_3: u8        = 0x5B;
    pub const KP_4: u8        = 0x5C;
    pub const KP_5: u8        = 0x5D;
    pub const KP_6: u8        = 0x5E;
    pub const KP_7: u8        = 0x5F;
    pub const KP_8: u8        = 0x60;
    pub const KP_9: u8        = 0x61;
    pub const KP_0: u8        = 0x62;
    pub const KP_DOT: u8      = 0x63;

    pub const NON_US_BACKSLASH: u8 = 0x64; // KEY_102ND
    pub const COMPOSE: u8          = 0x65;
    pub const POWER: u8            = 0x66;
    pub const KP_EQUAL: u8         = 0x67;

    pub const MUTE: u8        = 0x7F;
    pub const VOLUME_UP: u8   = 0x80;
    pub const VOLUME_DOWN: u8 = 0x81;

    // Modifier keys — Usage IDs; modifier byte bits are in ModifierBits
    pub const LEFT_CTRL: u8   = 0xE0;
    pub const LEFT_SHIFT: u8  = 0xE1;
    pub const LEFT_ALT: u8    = 0xE2;
    pub const LEFT_GUI: u8    = 0xE3;
    pub const RIGHT_CTRL: u8  = 0xE4;
    pub const RIGHT_SHIFT: u8 = 0xE5;
    pub const RIGHT_ALT: u8   = 0xE6;
    pub const RIGHT_GUI: u8   = 0xE7;
}

// ── HidKeyboardReport ─────────────────────────────────────────────────────────

/// 8-byte USB HID boot-protocol keyboard report.
///
/// Implements [`zerocopy::AsBytes`] / [`zerocopy::FromBytes`] for zero-copy
/// casting to/from `&[u8]`, and [`bytemuck::Pod`] for safe transmutation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default,
    Serialize, Deserialize,
    AsBytes, FromBytes, FromZeroes,
    Pod, Zeroable,
)]
#[repr(C)]
pub struct HidKeyboardReport {
    /// Modifier bitmask (see [`ModifierBits`]).
    pub modifiers: u8,
    /// Reserved — always 0x00.
    pub reserved: u8,
    /// Up to 6 simultaneous key Usage IDs; 0x00 = empty slot.
    pub keycodes: [u8; 6],
}

impl HidKeyboardReport {
    /// Create an empty (all-zeros) report.
    #[inline]
    pub const fn new() -> Self {
        Self { modifiers: 0, reserved: 0, keycodes: [0u8; 6] }
    }

    /// Deserialise from an 8-byte slice.
    pub fn from_slice(buf: &[u8]) -> Result<Self, HidReportError> {
        if buf.len() != 8 {
            return Err(HidReportError::BadLength { expected: 8, got: buf.len() });
        }
        let mut keycodes = [0u8; 6];
        keycodes.copy_from_slice(&buf[2..8]);
        Ok(Self { modifiers: buf[0], reserved: buf[1], keycodes })
    }

    /// Serialise to an 8-byte array.
    #[inline]
    pub fn to_wire(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = self.modifiers;
        out[1] = self.reserved;
        out[2..8].copy_from_slice(&self.keycodes);
        out
    }

    /// Press a key Usage ID (up to 6 simultaneous keys — 6KRO).
    pub fn press(&mut self, usage_id: u8) -> Result<(), HidReportError> {
        for slot in &mut self.keycodes {
            if *slot == 0 {
                *slot = usage_id;
                return Ok(());
            }
        }
        Err(HidReportError::KeyRollover)
    }

    /// Release a key Usage ID.
    pub fn release(&mut self, usage_id: u8) {
        for slot in &mut self.keycodes {
            if *slot == usage_id {
                *slot = 0;
            }
        }
    }
}

// ── HidMouseReport ────────────────────────────────────────────────────────────

/// 4-byte USB HID boot-protocol mouse report.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, Default,
    Serialize, Deserialize,
    AsBytes, FromBytes, FromZeroes,
    Pod, Zeroable,
)]
#[repr(C)]
pub struct HidMouseReport {
    /// Button bitmask: bit 0=left, bit 1=right, bit 2=middle.
    pub buttons: u8,
    /// X-axis relative movement (signed, positive = right).
    pub x: i8,
    /// Y-axis relative movement (signed, positive = down).
    pub y: i8,
    /// Scroll-wheel delta (signed, positive = up).
    pub wheel: i8,
}

impl HidMouseReport {
    pub const BUTTON_LEFT: u8   = 0x01;
    pub const BUTTON_RIGHT: u8  = 0x02;
    pub const BUTTON_MIDDLE: u8 = 0x04;

    /// Create an empty (all-zeros) report.
    #[inline]
    pub const fn new() -> Self {
        Self { buttons: 0, x: 0, y: 0, wheel: 0 }
    }

    /// Deserialise from a 4-byte slice.
    pub fn from_slice(buf: &[u8]) -> Result<Self, HidReportError> {
        if buf.len() != 4 {
            return Err(HidReportError::BadLength { expected: 4, got: buf.len() });
        }
        Ok(Self {
            buttons: buf[0],
            x:       buf[1] as i8,
            y:       buf[2] as i8,
            wheel:   buf[3] as i8,
        })
    }

    /// Serialise to a 4-byte array.
    #[inline]
    pub fn to_wire(self) -> [u8; 4] {
        [self.buttons, self.x as u8, self.y as u8, self.wheel as u8]
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use zerocopy::AsBytes;

    // ── HidKeyboardReport ─────────────────────────────────────────────────────

    #[test]
    fn keyboard_size_is_8_bytes() {
        assert_eq!(std::mem::size_of::<HidKeyboardReport>(), 8);
    }

    #[test]
    fn keyboard_round_trip_empty() {
        let report = HidKeyboardReport::new();
        assert_eq!(report.to_wire(), [0u8; 8]);
        let decoded = HidKeyboardReport::from_slice(&report.to_wire()).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn keyboard_round_trip_with_keys() {
        let mut report = HidKeyboardReport::new();
        report.modifiers = ModifierBits::LEFT_CTRL | ModifierBits::LEFT_SHIFT;
        report.press(KeyCode::A).unwrap();
        report.press(KeyCode::ENTER).unwrap();

        let bytes = report.to_wire();
        assert_eq!(bytes[0], ModifierBits::LEFT_CTRL | ModifierBits::LEFT_SHIFT);
        assert_eq!(bytes[1], 0x00);
        assert_eq!(bytes[2], KeyCode::A);
        assert_eq!(bytes[3], KeyCode::ENTER);
        assert_eq!(&bytes[4..], &[0u8; 4]);

        let decoded = HidKeyboardReport::from_slice(&bytes).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn keyboard_rollover_error() {
        let mut report = HidKeyboardReport::new();
        for i in 0x04u8..=0x09 {
            report.press(i).unwrap();
        }
        assert_eq!(report.press(0x0A), Err(HidReportError::KeyRollover));
    }

    #[test]
    fn keyboard_release() {
        let mut report = HidKeyboardReport::new();
        report.press(KeyCode::A).unwrap();
        report.press(KeyCode::B).unwrap();
        report.release(KeyCode::A);
        assert_eq!(report.keycodes[0], 0x00);
        assert_eq!(report.keycodes[1], KeyCode::B);
    }

    #[test]
    fn keyboard_bad_length() {
        assert_eq!(
            HidKeyboardReport::from_slice(&[0u8; 7]),
            Err(HidReportError::BadLength { expected: 8, got: 7 })
        );
    }

    #[test]
    fn keyboard_zerocopy_as_bytes_matches_to_wire() {
        let mut r = HidKeyboardReport::new();
        r.modifiers = ModifierBits::LEFT_SHIFT;
        r.press(KeyCode::S).unwrap();
        assert_eq!(r.as_bytes(), &r.to_wire());
    }

    #[test]
    fn keyboard_serde_round_trip() {
        let mut r = HidKeyboardReport::new();
        r.modifiers = ModifierBits::RIGHT_ALT;
        r.press(KeyCode::ENTER).unwrap();
        let json = serde_json::to_string(&r).unwrap();
        let decoded: HidKeyboardReport = serde_json::from_str(&json).unwrap();
        assert_eq!(r, decoded);
    }

    // ── HidMouseReport ────────────────────────────────────────────────────────

    #[test]
    fn mouse_size_is_4_bytes() {
        assert_eq!(std::mem::size_of::<HidMouseReport>(), 4);
    }

    #[test]
    fn mouse_round_trip_empty() {
        let report = HidMouseReport::new();
        assert_eq!(report.to_wire(), [0u8; 4]);
        let decoded = HidMouseReport::from_slice(&report.to_wire()).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn mouse_round_trip_with_values() {
        let report = HidMouseReport {
            buttons: HidMouseReport::BUTTON_LEFT | HidMouseReport::BUTTON_RIGHT,
            x: -10,
            y: 42,
            wheel: -1,
        };
        let bytes = report.to_wire();
        assert_eq!(bytes[0], HidMouseReport::BUTTON_LEFT | HidMouseReport::BUTTON_RIGHT);
        assert_eq!(bytes[1] as i8, -10i8);
        assert_eq!(bytes[2] as i8, 42i8);
        assert_eq!(bytes[3] as i8, -1i8);
        let decoded = HidMouseReport::from_slice(&bytes).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn mouse_signed_extremes() {
        let report = HidMouseReport { buttons: 0, x: i8::MIN, y: i8::MAX, wheel: -1 };
        let decoded = HidMouseReport::from_slice(&report.to_wire()).unwrap();
        assert_eq!(decoded.x, i8::MIN);
        assert_eq!(decoded.y, i8::MAX);
        assert_eq!(decoded.wheel, -1);
    }

    #[test]
    fn mouse_bad_length() {
        assert_eq!(
            HidMouseReport::from_slice(&[0u8; 3]),
            Err(HidReportError::BadLength { expected: 4, got: 3 })
        );
    }

    #[test]
    fn mouse_zerocopy_as_bytes_matches_to_wire() {
        let r = HidMouseReport { buttons: HidMouseReport::BUTTON_LEFT, x: 1, y: 2, wheel: 0 };
        assert_eq!(r.as_bytes(), &r.to_wire());
    }

    #[test]
    fn mouse_serde_round_trip() {
        let r = HidMouseReport { buttons: HidMouseReport::BUTTON_RIGHT, x: -5, y: 10, wheel: 2 };
        let json = serde_json::to_string(&r).unwrap();
        let decoded: HidMouseReport = serde_json::from_str(&json).unwrap();
        assert_eq!(r, decoded);
    }

    // ── KeyCode table spot-checks (ported from controller/keycodes.py) ────────

    #[test]
    fn keycode_values_match_python_source() {
        assert_eq!(KeyCode::A,                0x04);
        assert_eq!(KeyCode::Z,                0x1D);
        assert_eq!(KeyCode::KEY_1,            0x1E);
        assert_eq!(KeyCode::KEY_0,            0x27);
        assert_eq!(KeyCode::ENTER,            0x28);
        assert_eq!(KeyCode::ESCAPE,           0x29);
        assert_eq!(KeyCode::BACKSPACE,        0x2A);
        assert_eq!(KeyCode::TAB,              0x2B);
        assert_eq!(KeyCode::SPACE,            0x2C);
        assert_eq!(KeyCode::F1,               0x3A);
        assert_eq!(KeyCode::F12,              0x45);
        assert_eq!(KeyCode::F13,              0x68);
        assert_eq!(KeyCode::F24,              0x73);
        assert_eq!(KeyCode::PRINT_SCREEN,     0x46);
        assert_eq!(KeyCode::SCROLL_LOCK,      0x47);
        assert_eq!(KeyCode::PAUSE,            0x48);
        assert_eq!(KeyCode::INSERT,           0x49);
        assert_eq!(KeyCode::HOME,             0x4A);
        assert_eq!(KeyCode::PAGE_UP,          0x4B);
        assert_eq!(KeyCode::DELETE,           0x4C);
        assert_eq!(KeyCode::END,              0x4D);
        assert_eq!(KeyCode::PAGE_DOWN,        0x4E);
        assert_eq!(KeyCode::RIGHT,            0x4F);
        assert_eq!(KeyCode::LEFT,             0x50);
        assert_eq!(KeyCode::DOWN,             0x51);
        assert_eq!(KeyCode::UP,               0x52);
        assert_eq!(KeyCode::NUM_LOCK,         0x53);
        assert_eq!(KeyCode::KP_SLASH,         0x54);
        assert_eq!(KeyCode::KP_ENTER,         0x58);
        assert_eq!(KeyCode::KP_0,             0x62);
        assert_eq!(KeyCode::KP_DOT,           0x63);
        assert_eq!(KeyCode::NON_US_BACKSLASH, 0x64);
        assert_eq!(KeyCode::COMPOSE,          0x65);
        assert_eq!(KeyCode::POWER,            0x66);
        assert_eq!(KeyCode::KP_EQUAL,         0x67);
        assert_eq!(KeyCode::MUTE,             0x7F);
        assert_eq!(KeyCode::VOLUME_UP,        0x80);
        assert_eq!(KeyCode::VOLUME_DOWN,      0x81);
        assert_eq!(KeyCode::LEFT_CTRL,        0xE0);
        assert_eq!(KeyCode::LEFT_SHIFT,       0xE1);
        assert_eq!(KeyCode::LEFT_ALT,         0xE2);
        assert_eq!(KeyCode::LEFT_GUI,         0xE3);
        assert_eq!(KeyCode::RIGHT_CTRL,       0xE4);
        assert_eq!(KeyCode::RIGHT_SHIFT,      0xE5);
        assert_eq!(KeyCode::RIGHT_ALT,        0xE6);
        assert_eq!(KeyCode::RIGHT_GUI,        0xE7);
    }

    #[test]
    fn modifier_bits_match_hid_spec() {
        assert_eq!(ModifierBits::LEFT_CTRL,   0x01);
        assert_eq!(ModifierBits::LEFT_SHIFT,  0x02);
        assert_eq!(ModifierBits::LEFT_ALT,    0x04);
        assert_eq!(ModifierBits::LEFT_GUI,    0x08);
        assert_eq!(ModifierBits::RIGHT_CTRL,  0x10);
        assert_eq!(ModifierBits::RIGHT_SHIFT, 0x20);
        assert_eq!(ModifierBits::RIGHT_ALT,   0x40);
        assert_eq!(ModifierBits::RIGHT_GUI,   0x80);
    }
}
