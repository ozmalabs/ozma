//! USB HID boot-protocol wire format.
//!
//! # Keyboard report (8 bytes)
//! ```text
//! [0]    modifier bitmask  (see [`modifier`])
//! [1]    reserved — always 0x00
//! [2..7] up to 6 simultaneous key Usage IDs (0x00 = empty slot)
//! ```
//!
//! # Mouse report (4 bytes)
//! ```text
//! [0]    button bitmask  (bit 0 = left, 1 = right, 2 = middle)
//! [1]    X delta  (signed i8)
//! [2]    Y delta  (signed i8)
//! [3]    scroll   (signed i8)
//! ```

use serde::{Deserialize, Serialize};
use zerocopy::{AsBytes, FromBytes, FromZeroes};

// ---------------------------------------------------------------------------
// Modifier bitmask constants (USB HID spec, boot-protocol keyboard)
// ---------------------------------------------------------------------------

/// Modifier key bitmask values for byte 0 of [`HidKeyboardReport`].
///
/// Ported from `controller/keycodes.py` `MODIFIER_BITS`.
pub mod modifier {
    pub const LEFT_CTRL:   u8 = 0x01;
    pub const LEFT_SHIFT:  u8 = 0x02;
    pub const LEFT_ALT:    u8 = 0x04;
    pub const LEFT_GUI:    u8 = 0x08;
    pub const RIGHT_CTRL:  u8 = 0x10;
    pub const RIGHT_SHIFT: u8 = 0x20;
    pub const RIGHT_ALT:   u8 = 0x40;
    pub const RIGHT_GUI:   u8 = 0x80;
}

// ---------------------------------------------------------------------------
// HID Usage IDs (keyboard page 0x07) — ported from controller/keycodes.py
// ---------------------------------------------------------------------------

/// HID Usage ID constants for the keyboard page (0x07).
///
/// Names mirror the evdev `KEY_*` names used in `controller/keycodes.py`.
pub mod usage {
    pub const KEY_A: u8 = 0x04;
    pub const KEY_B: u8 = 0x05;
    pub const KEY_C: u8 = 0x06;
    pub const KEY_D: u8 = 0x07;
    pub const KEY_E: u8 = 0x08;
    pub const KEY_F: u8 = 0x09;
    pub const KEY_G: u8 = 0x0A;
    pub const KEY_H: u8 = 0x0B;
    pub const KEY_I: u8 = 0x0C;
    pub const KEY_J: u8 = 0x0D;
    pub const KEY_K: u8 = 0x0E;
    pub const KEY_L: u8 = 0x0F;
    pub const KEY_M: u8 = 0x10;
    pub const KEY_N: u8 = 0x11;
    pub const KEY_O: u8 = 0x12;
    pub const KEY_P: u8 = 0x13;
    pub const KEY_Q: u8 = 0x14;
    pub const KEY_R: u8 = 0x15;
    pub const KEY_S: u8 = 0x16;
    pub const KEY_T: u8 = 0x17;
    pub const KEY_U: u8 = 0x18;
    pub const KEY_V: u8 = 0x19;
    pub const KEY_W: u8 = 0x1A;
    pub const KEY_X: u8 = 0x1B;
    pub const KEY_Y: u8 = 0x1C;
    pub const KEY_Z: u8 = 0x1D;
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
    pub const KEY_ENTER:     u8 = 0x28;
    pub const KEY_ESC:       u8 = 0x29;
    pub const KEY_BACKSPACE: u8 = 0x2A;
    pub const KEY_TAB:       u8 = 0x2B;
    pub const KEY_SPACE:     u8 = 0x2C;
    pub const KEY_MINUS:     u8 = 0x2D;
    pub const KEY_EQUAL:     u8 = 0x2E;
    pub const KEY_LEFTBRACE:  u8 = 0x2F;
    pub const KEY_RIGHTBRACE: u8 = 0x30;
    pub const KEY_BACKSLASH:  u8 = 0x31;
    pub const KEY_SEMICOLON:  u8 = 0x33;
    pub const KEY_APOSTROPHE: u8 = 0x34;
    pub const KEY_GRAVE:      u8 = 0x35;
    pub const KEY_COMMA:      u8 = 0x36;
    pub const KEY_DOT:        u8 = 0x37;
    pub const KEY_SLASH:      u8 = 0x38;
    pub const KEY_CAPSLOCK:   u8 = 0x39;
    pub const KEY_F1:  u8 = 0x3A;
    pub const KEY_F2:  u8 = 0x3B;
    pub const KEY_F3:  u8 = 0x3C;
    pub const KEY_F4:  u8 = 0x3D;
    pub const KEY_F5:  u8 = 0x3E;
    pub const KEY_F6:  u8 = 0x3F;
    pub const KEY_F7:  u8 = 0x40;
    pub const KEY_F8:  u8 = 0x41;
    pub const KEY_F9:  u8 = 0x42;
    pub const KEY_F10: u8 = 0x43;
    pub const KEY_F11: u8 = 0x44;
    pub const KEY_F12: u8 = 0x45;
    pub const KEY_SYSRQ:      u8 = 0x46; // Print Screen
    pub const KEY_SCROLLLOCK: u8 = 0x47;
    pub const KEY_PAUSE:      u8 = 0x48;
    pub const KEY_INSERT:     u8 = 0x49;
    pub const KEY_HOME:       u8 = 0x4A;
    pub const KEY_PAGEUP:     u8 = 0x4B;
    pub const KEY_DELETE:     u8 = 0x4C;
    pub const KEY_END:        u8 = 0x4D;
    pub const KEY_PAGEDOWN:   u8 = 0x4E;
    pub const KEY_RIGHT: u8 = 0x4F;
    pub const KEY_LEFT:  u8 = 0x50;
    pub const KEY_DOWN:  u8 = 0x51;
    pub const KEY_UP:    u8 = 0x52;
    pub const KEY_NUMLOCK:    u8 = 0x53;
    pub const KEY_KPSLASH:    u8 = 0x54;
    pub const KEY_KPASTERISK: u8 = 0x55;
    pub const KEY_KPMINUS:    u8 = 0x56;
    pub const KEY_KPPLUS:     u8 = 0x57;
    pub const KEY_KPENTER:    u8 = 0x58;
    pub const KEY_KP1: u8 = 0x59;
    pub const KEY_KP2: u8 = 0x5A;
    pub const KEY_KP3: u8 = 0x5B;
    pub const KEY_KP4: u8 = 0x5C;
    pub const KEY_KP5: u8 = 0x5D;
    pub const KEY_KP6: u8 = 0x5E;
    pub const KEY_KP7: u8 = 0x5F;
    pub const KEY_KP8: u8 = 0x60;
    pub const KEY_KP9: u8 = 0x61;
    pub const KEY_KP0: u8 = 0x62;
    pub const KEY_KPDOT:    u8 = 0x63;
    pub const KEY_102ND:    u8 = 0x64; // Non-US backslash
    pub const KEY_COMPOSE:  u8 = 0x65;
    pub const KEY_POWER:    u8 = 0x66;
    pub const KEY_KPEQUAL:  u8 = 0x67;
    pub const KEY_F13: u8 = 0x68;
    pub const KEY_F14: u8 = 0x69;
    pub const KEY_F15: u8 = 0x6A;
    pub const KEY_F16: u8 = 0x6B;
    pub const KEY_F17: u8 = 0x6C;
    pub const KEY_F18: u8 = 0x6D;
    pub const KEY_F19: u8 = 0x6E;
    pub const KEY_F20: u8 = 0x6F;
    pub const KEY_F21: u8 = 0x70;
    pub const KEY_F22: u8 = 0x71;
    pub const KEY_F23: u8 = 0x72;
    pub const KEY_F24: u8 = 0x73;
    pub const KEY_MUTE:       u8 = 0x7F;
    pub const KEY_VOLUMEUP:   u8 = 0x80;
    pub const KEY_VOLUMEDOWN: u8 = 0x81;
    // Modifier keys also have Usage IDs (used in key-slot bytes, not modifier byte)
    pub const KEY_LEFTCTRL:   u8 = 0xE0;
    pub const KEY_LEFTSHIFT:  u8 = 0xE1;
    pub const KEY_LEFTALT:    u8 = 0xE2;
    pub const KEY_LEFTMETA:   u8 = 0xE3;
    pub const KEY_RIGHTCTRL:  u8 = 0xE4;
    pub const KEY_RIGHTSHIFT: u8 = 0xE5;
    pub const KEY_RIGHTALT:   u8 = 0xE6;
    pub const KEY_RIGHTMETA:  u8 = 0xE7;
}

// ---------------------------------------------------------------------------
// HidKeyboardReport
// ---------------------------------------------------------------------------

/// 8-byte USB HID boot-protocol keyboard report.
///
/// Implements [`zerocopy::AsBytes`] / [`zerocopy::FromBytes`] so it can be
/// cast directly to/from a `[u8; 8]` without copying.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize,
    AsBytes, FromBytes, FromZeroes,
)]
#[repr(C)]
pub struct HidKeyboardReport {
    /// Modifier bitmask — see [`modifier`] constants.
    pub modifiers: u8,
    /// Reserved — must be 0x00.
    pub reserved: u8,
    /// Up to 6 simultaneous key Usage IDs; 0x00 = empty slot.
    pub keycodes: [u8; 6],
}

impl HidKeyboardReport {
    /// Construct a report with no keys pressed.
    pub const fn empty() -> Self {
        Self { modifiers: 0, reserved: 0, keycodes: [0u8; 6] }
    }

    /// Encode to the 8-byte on-wire representation.
    #[inline]
    pub fn to_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = self.modifiers;
        out[1] = self.reserved;
        out[2..8].copy_from_slice(&self.keycodes);
        out
    }

    /// Decode from the 8-byte on-wire representation.
    #[inline]
    pub fn from_bytes(b: [u8; 8]) -> Self {
        let mut keycodes = [0u8; 6];
        keycodes.copy_from_slice(&b[2..8]);
        Self { modifiers: b[0], reserved: b[1], keycodes }
    }
}

impl Default for HidKeyboardReport {
    fn default() -> Self { Self::empty() }
}

// ---------------------------------------------------------------------------
// HidMouseReport
// ---------------------------------------------------------------------------

/// 4-byte USB HID boot-protocol mouse report.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize,
    AsBytes, FromBytes, FromZeroes,
)]
#[repr(C)]
pub struct HidMouseReport {
    /// Button bitmask: bit 0 = left, bit 1 = right, bit 2 = middle.
    pub buttons: u8,
    /// Relative X movement (signed).
    pub x: i8,
    /// Relative Y movement (signed).
    pub y: i8,
    /// Scroll wheel delta (signed).
    pub scroll: i8,
}

impl HidMouseReport {
    /// Construct a report with no buttons pressed and no movement.
    pub const fn empty() -> Self {
        Self { buttons: 0, x: 0, y: 0, scroll: 0 }
    }

    /// Encode to the 4-byte on-wire representation.
    #[inline]
    pub fn to_bytes(self) -> [u8; 4] {
        [self.buttons, self.x as u8, self.y as u8, self.scroll as u8]
    }

    /// Decode from the 4-byte on-wire representation.
    #[inline]
    pub fn from_bytes(b: [u8; 4]) -> Self {
        Self {
            buttons: b[0],
            x:       b[1] as i8,
            y:       b[2] as i8,
            scroll:  b[3] as i8,
        }
    }
}

impl Default for HidMouseReport {
    fn default() -> Self { Self::empty() }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // --- HidKeyboardReport ---

    #[test]
    fn keyboard_empty_round_trip() {
        let report = HidKeyboardReport::empty();
        let bytes = report.to_bytes();
        assert_eq!(bytes, [0u8; 8]);
        assert_eq!(HidKeyboardReport::from_bytes(bytes), report);
    }

    #[test]
    fn keyboard_round_trip_with_keys() {
        let report = HidKeyboardReport {
            modifiers: modifier::LEFT_CTRL | modifier::LEFT_SHIFT,
            reserved:  0,
            keycodes:  [usage::KEY_A, usage::KEY_ENTER, 0, 0, 0, 0],
        };
        let bytes = report.to_bytes();
        assert_eq!(bytes[0], modifier::LEFT_CTRL | modifier::LEFT_SHIFT);
        assert_eq!(bytes[1], 0x00);
        assert_eq!(bytes[2], usage::KEY_A);
        assert_eq!(bytes[3], usage::KEY_ENTER);
        assert_eq!(HidKeyboardReport::from_bytes(bytes), report);
    }

    #[test]
    fn keyboard_six_keys() {
        let report = HidKeyboardReport {
            modifiers: 0,
            reserved:  0,
            keycodes:  [
                usage::KEY_A, usage::KEY_B, usage::KEY_C,
                usage::KEY_D, usage::KEY_E, usage::KEY_F,
            ],
        };
        let bytes = report.to_bytes();
        assert_eq!(&bytes[2..8], &[
            usage::KEY_A, usage::KEY_B, usage::KEY_C,
            usage::KEY_D, usage::KEY_E, usage::KEY_F,
        ]);
        assert_eq!(HidKeyboardReport::from_bytes(bytes), report);
    }

    #[test]
    fn keyboard_all_modifier_bits() {
        let all_mods: u8 =
            modifier::LEFT_CTRL  | modifier::LEFT_SHIFT |
            modifier::LEFT_ALT   | modifier::LEFT_GUI   |
            modifier::RIGHT_CTRL | modifier::RIGHT_SHIFT |
            modifier::RIGHT_ALT  | modifier::RIGHT_GUI;
        let report = HidKeyboardReport { modifiers: all_mods, reserved: 0, keycodes: [0u8; 6] };
        assert_eq!(HidKeyboardReport::from_bytes(report.to_bytes()), report);
    }

    #[test]
    fn keyboard_size_is_8_bytes() {
        assert_eq!(std::mem::size_of::<HidKeyboardReport>(), 8);
    }

    // --- HidMouseReport ---

    #[test]
    fn mouse_empty_round_trip() {
        let report = HidMouseReport::empty();
        let bytes = report.to_bytes();
        assert_eq!(bytes, [0u8; 4]);
        assert_eq!(HidMouseReport::from_bytes(bytes), report);
    }

    #[test]
    fn mouse_round_trip_with_movement() {
        let report = HidMouseReport { buttons: 0b001, x: -10, y: 42, scroll: -1 };
        let bytes = report.to_bytes();
        assert_eq!(bytes[0], 0b001);
        assert_eq!(bytes[1] as i8, -10);
        assert_eq!(bytes[2] as i8, 42);
        assert_eq!(bytes[3] as i8, -1);
        assert_eq!(HidMouseReport::from_bytes(bytes), report);
    }

    #[test]
    fn mouse_all_buttons() {
        let report = HidMouseReport { buttons: 0b111, x: 0, y: 0, scroll: 0 };
        assert_eq!(HidMouseReport::from_bytes(report.to_bytes()), report);
    }

    #[test]
    fn mouse_extreme_deltas() {
        let report = HidMouseReport { buttons: 0, x: i8::MAX, y: i8::MIN, scroll: i8::MAX };
        assert_eq!(HidMouseReport::from_bytes(report.to_bytes()), report);
    }

    #[test]
    fn mouse_size_is_4_bytes() {
        assert_eq!(std::mem::size_of::<HidMouseReport>(), 4);
    }

    // --- Usage ID spot-checks (ported from keycodes.py) ---

    #[test]
    fn usage_ids_spot_check() {
        assert_eq!(usage::KEY_A,           0x04);
        assert_eq!(usage::KEY_Z,           0x1D);
        assert_eq!(usage::KEY_1,           0x1E);
        assert_eq!(usage::KEY_0,           0x27);
        assert_eq!(usage::KEY_ENTER,       0x28);
        assert_eq!(usage::KEY_ESC,         0x29);
        assert_eq!(usage::KEY_BACKSPACE,   0x2A);
        assert_eq!(usage::KEY_TAB,         0x2B);
        assert_eq!(usage::KEY_SPACE,       0x2C);
        assert_eq!(usage::KEY_F1,          0x3A);
        assert_eq!(usage::KEY_F12,         0x45);
        assert_eq!(usage::KEY_F13,         0x68);
        assert_eq!(usage::KEY_F24,         0x73);
        assert_eq!(usage::KEY_SYSRQ,       0x46);
        assert_eq!(usage::KEY_SCROLLLOCK,  0x47);
        assert_eq!(usage::KEY_PAUSE,       0x48);
        assert_eq!(usage::KEY_INSERT,      0x49);
        assert_eq!(usage::KEY_HOME,        0x4A);
        assert_eq!(usage::KEY_PAGEUP,      0x4B);
        assert_eq!(usage::KEY_DELETE,      0x4C);
        assert_eq!(usage::KEY_END,         0x4D);
        assert_eq!(usage::KEY_PAGEDOWN,    0x4E);
        assert_eq!(usage::KEY_RIGHT,       0x4F);
        assert_eq!(usage::KEY_LEFT,        0x50);
        assert_eq!(usage::KEY_DOWN,        0x51);
        assert_eq!(usage::KEY_UP,          0x52);
        assert_eq!(usage::KEY_NUMLOCK,     0x53);
        assert_eq!(usage::KEY_KPSLASH,     0x54);
        assert_eq!(usage::KEY_KPENTER,     0x58);
        assert_eq!(usage::KEY_KP0,         0x62);
        assert_eq!(usage::KEY_KPDOT,       0x63);
        assert_eq!(usage::KEY_102ND,       0x64);
        assert_eq!(usage::KEY_COMPOSE,     0x65);
        assert_eq!(usage::KEY_POWER,       0x66);
        assert_eq!(usage::KEY_KPEQUAL,     0x67);
        assert_eq!(usage::KEY_MUTE,        0x7F);
        assert_eq!(usage::KEY_VOLUMEUP,    0x80);
        assert_eq!(usage::KEY_VOLUMEDOWN,  0x81);
        assert_eq!(usage::KEY_LEFTCTRL,    0xE0);
        assert_eq!(usage::KEY_LEFTSHIFT,   0xE1);
        assert_eq!(usage::KEY_LEFTALT,     0xE2);
        assert_eq!(usage::KEY_LEFTMETA,    0xE3);
        assert_eq!(usage::KEY_RIGHTCTRL,   0xE4);
        assert_eq!(usage::KEY_RIGHTSHIFT,  0xE5);
        assert_eq!(usage::KEY_RIGHTALT,    0xE6);
        assert_eq!(usage::KEY_RIGHTMETA,   0xE7);
    }

    #[test]
    fn modifier_constants_match_hid_spec() {
        assert_eq!(modifier::LEFT_CTRL,   0x01);
        assert_eq!(modifier::LEFT_SHIFT,  0x02);
        assert_eq!(modifier::LEFT_ALT,    0x04);
        assert_eq!(modifier::LEFT_GUI,    0x08);
        assert_eq!(modifier::RIGHT_CTRL,  0x10);
        assert_eq!(modifier::RIGHT_SHIFT, 0x20);
        assert_eq!(modifier::RIGHT_ALT,   0x40);
        assert_eq!(modifier::RIGHT_GUI,   0x80);
    }

    // --- Serde round-trip ---

    #[test]
    fn keyboard_serde_round_trip() {
        let report = HidKeyboardReport {
            modifiers: modifier::LEFT_ALT,
            reserved:  0,
            keycodes:  [usage::KEY_TAB, 0, 0, 0, 0, 0],
        };
        let json = serde_json::to_string(&report).unwrap();
        let decoded: HidKeyboardReport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, report);
    }

    #[test]
    fn mouse_serde_round_trip() {
        let report = HidMouseReport { buttons: 0b010, x: 5, y: -3, scroll: 0 };
        let json = serde_json::to_string(&report).unwrap();
        let decoded: HidMouseReport = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded, report);
    }
}
