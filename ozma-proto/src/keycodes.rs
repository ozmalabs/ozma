//! evdev KEY_* ↔ HID Usage ID and X11 keysym lookup tables.
//!
//! Ported from `controller/keycodes.py`.  Linux evdev keycodes are defined in
//! `<linux/input-event-codes.h>`; the values used here match those constants.
//!
//! # Usage
//! ```rust
//! use ozma_proto::keycodes::{evdev_to_hid, evdev_to_x11, modifier_bit};
//!
//! let hid = evdev_to_hid(30); // KEY_A → 0x04
//! let x11 = evdev_to_x11(30); // KEY_A → "a"
//! let bit = modifier_bit(29);  // KEY_LEFTCTRL → Some(0x01)
//! ```

// ── evdev key constants (linux/input-event-codes.h) ──────────────────────────
// Only the subset used in the tables below.

pub mod key {
    pub const KEY_A: u16 = 30;
    pub const KEY_B: u16 = 48;
    pub const KEY_C: u16 = 46;
    pub const KEY_D: u16 = 32;
    pub const KEY_E: u16 = 18;
    pub const KEY_F: u16 = 33;
    pub const KEY_G: u16 = 34;
    pub const KEY_H: u16 = 35;
    pub const KEY_I: u16 = 23;
    pub const KEY_J: u16 = 36;
    pub const KEY_K: u16 = 37;
    pub const KEY_L: u16 = 38;
    pub const KEY_M: u16 = 50;
    pub const KEY_N: u16 = 49;
    pub const KEY_O: u16 = 24;
    pub const KEY_P: u16 = 25;
    pub const KEY_Q: u16 = 16;
    pub const KEY_R: u16 = 19;
    pub const KEY_S: u16 = 31;
    pub const KEY_T: u16 = 20;
    pub const KEY_U: u16 = 22;
    pub const KEY_V: u16 = 47;
    pub const KEY_W: u16 = 17;
    pub const KEY_X: u16 = 45;
    pub const KEY_Y: u16 = 21;
    pub const KEY_Z: u16 = 44;
    pub const KEY_1: u16 = 2;
    pub const KEY_2: u16 = 3;
    pub const KEY_3: u16 = 4;
    pub const KEY_4: u16 = 5;
    pub const KEY_5: u16 = 6;
    pub const KEY_6: u16 = 7;
    pub const KEY_7: u16 = 8;
    pub const KEY_8: u16 = 9;
    pub const KEY_9: u16 = 10;
    pub const KEY_0: u16 = 11;
    pub const KEY_ENTER: u16 = 28;
    pub const KEY_ESC: u16 = 1;
    pub const KEY_BACKSPACE: u16 = 14;
    pub const KEY_TAB: u16 = 15;
    pub const KEY_SPACE: u16 = 57;
    pub const KEY_MINUS: u16 = 12;
    pub const KEY_EQUAL: u16 = 13;
    pub const KEY_LEFTBRACE: u16 = 26;
    pub const KEY_RIGHTBRACE: u16 = 27;
    pub const KEY_BACKSLASH: u16 = 43;
    pub const KEY_SEMICOLON: u16 = 39;
    pub const KEY_APOSTROPHE: u16 = 40;
    pub const KEY_GRAVE: u16 = 41;
    pub const KEY_COMMA: u16 = 51;
    pub const KEY_DOT: u16 = 52;
    pub const KEY_SLASH: u16 = 53;
    pub const KEY_CAPSLOCK: u16 = 58;
    pub const KEY_F1: u16 = 59;
    pub const KEY_F2: u16 = 60;
    pub const KEY_F3: u16 = 61;
    pub const KEY_F4: u16 = 62;
    pub const KEY_F5: u16 = 63;
    pub const KEY_F6: u16 = 64;
    pub const KEY_F7: u16 = 65;
    pub const KEY_F8: u16 = 66;
    pub const KEY_F9: u16 = 67;
    pub const KEY_F10: u16 = 68;
    pub const KEY_F11: u16 = 87;
    pub const KEY_F12: u16 = 88;
    pub const KEY_F13: u16 = 183;
    pub const KEY_F14: u16 = 184;
    pub const KEY_F15: u16 = 185;
    pub const KEY_F16: u16 = 186;
    pub const KEY_F17: u16 = 187;
    pub const KEY_F18: u16 = 188;
    pub const KEY_F19: u16 = 189;
    pub const KEY_F20: u16 = 190;
    pub const KEY_F21: u16 = 191;
    pub const KEY_F22: u16 = 192;
    pub const KEY_F23: u16 = 193;
    pub const KEY_F24: u16 = 194;
    pub const KEY_SYSRQ: u16 = 99;
    pub const KEY_SCROLLLOCK: u16 = 70;
    pub const KEY_PAUSE: u16 = 119;
    pub const KEY_INSERT: u16 = 110;
    pub const KEY_HOME: u16 = 102;
    pub const KEY_PAGEUP: u16 = 104;
    pub const KEY_DELETE: u16 = 111;
    pub const KEY_END: u16 = 107;
    pub const KEY_PAGEDOWN: u16 = 109;
    pub const KEY_RIGHT: u16 = 106;
    pub const KEY_LEFT: u16 = 105;
    pub const KEY_DOWN: u16 = 108;
    pub const KEY_UP: u16 = 103;
    pub const KEY_NUMLOCK: u16 = 69;
    pub const KEY_KPSLASH: u16 = 98;
    pub const KEY_KPASTERISK: u16 = 55;
    pub const KEY_KPMINUS: u16 = 74;
    pub const KEY_KPPLUS: u16 = 78;
    pub const KEY_KPENTER: u16 = 96;
    pub const KEY_KP1: u16 = 79;
    pub const KEY_KP2: u16 = 80;
    pub const KEY_KP3: u16 = 81;
    pub const KEY_KP4: u16 = 75;
    pub const KEY_KP5: u16 = 76;
    pub const KEY_KP6: u16 = 77;
    pub const KEY_KP7: u16 = 71;
    pub const KEY_KP8: u16 = 72;
    pub const KEY_KP9: u16 = 73;
    pub const KEY_KP0: u16 = 82;
    pub const KEY_KPDOT: u16 = 83;
    pub const KEY_102ND: u16 = 86;
    pub const KEY_COMPOSE: u16 = 127;
    pub const KEY_POWER: u16 = 116;
    pub const KEY_KPEQUAL: u16 = 117;
    pub const KEY_MUTE: u16 = 113;
    pub const KEY_VOLUMEUP: u16 = 115;
    pub const KEY_VOLUMEDOWN: u16 = 114;
    pub const KEY_LEFTCTRL: u16 = 29;
    pub const KEY_LEFTSHIFT: u16 = 42;
    pub const KEY_LEFTALT: u16 = 56;
    pub const KEY_LEFTMETA: u16 = 125;
    pub const KEY_RIGHTCTRL: u16 = 97;
    pub const KEY_RIGHTSHIFT: u16 = 54;
    pub const KEY_RIGHTALT: u16 = 100;
    pub const KEY_RIGHTMETA: u16 = 126;
}

use key::*;
use phf::phf_map;

// ── evdev → HID Usage ID (keyboard page 0x07) ─────────────────────────────────

static EVDEV_TO_HID_MAP: phf::Map<u16, u8> = phf_map! {
    30u16  => 0x04, // KEY_A
    48u16  => 0x05, // KEY_B
    46u16  => 0x06, // KEY_C
    32u16  => 0x07, // KEY_D
    18u16  => 0x08, // KEY_E
    33u16  => 0x09, // KEY_F
    34u16  => 0x0A, // KEY_G
    35u16  => 0x0B, // KEY_H
    23u16  => 0x0C, // KEY_I
    36u16  => 0x0D, // KEY_J
    37u16  => 0x0E, // KEY_K
    38u16  => 0x0F, // KEY_L
    50u16  => 0x10, // KEY_M
    49u16  => 0x11, // KEY_N
    24u16  => 0x12, // KEY_O
    25u16  => 0x13, // KEY_P
    16u16  => 0x14, // KEY_Q
    19u16  => 0x15, // KEY_R
    31u16  => 0x16, // KEY_S
    20u16  => 0x17, // KEY_T
    22u16  => 0x18, // KEY_U
    47u16  => 0x19, // KEY_V
    17u16  => 0x1A, // KEY_W
    45u16  => 0x1B, // KEY_X
    21u16  => 0x1C, // KEY_Y
    44u16  => 0x1D, // KEY_Z
    2u16   => 0x1E, // KEY_1
    3u16   => 0x1F, // KEY_2
    4u16   => 0x20, // KEY_3
    5u16   => 0x21, // KEY_4
    6u16   => 0x22, // KEY_5
    7u16   => 0x23, // KEY_6
    8u16   => 0x24, // KEY_7
    9u16   => 0x25, // KEY_8
    10u16  => 0x26, // KEY_9
    11u16  => 0x27, // KEY_0
    28u16  => 0x28, // KEY_ENTER
    1u16   => 0x29, // KEY_ESC
    14u16  => 0x2A, // KEY_BACKSPACE
    15u16  => 0x2B, // KEY_TAB
    57u16  => 0x2C, // KEY_SPACE
    12u16  => 0x2D, // KEY_MINUS
    13u16  => 0x2E, // KEY_EQUAL
    26u16  => 0x2F, // KEY_LEFTBRACE
    27u16  => 0x30, // KEY_RIGHTBRACE
    43u16  => 0x31, // KEY_BACKSLASH
    39u16  => 0x33, // KEY_SEMICOLON
    40u16  => 0x34, // KEY_APOSTROPHE
    41u16  => 0x35, // KEY_GRAVE
    51u16  => 0x36, // KEY_COMMA
    52u16  => 0x37, // KEY_DOT
    53u16  => 0x38, // KEY_SLASH
    58u16  => 0x39, // KEY_CAPSLOCK
    59u16  => 0x3A, // KEY_F1
    60u16  => 0x3B, // KEY_F2
    61u16  => 0x3C, // KEY_F3
    62u16  => 0x3D, // KEY_F4
    63u16  => 0x3E, // KEY_F5
    64u16  => 0x3F, // KEY_F6
    65u16  => 0x40, // KEY_F7
    66u16  => 0x41, // KEY_F8
    67u16  => 0x42, // KEY_F9
    68u16  => 0x43, // KEY_F10
    87u16  => 0x44, // KEY_F11
    88u16  => 0x45, // KEY_F12
    99u16  => 0x46, // KEY_SYSRQ (Print Screen)
    70u16  => 0x47, // KEY_SCROLLLOCK
    119u16 => 0x48, // KEY_PAUSE
    110u16 => 0x49, // KEY_INSERT
    102u16 => 0x4A, // KEY_HOME
    104u16 => 0x4B, // KEY_PAGEUP
    111u16 => 0x4C, // KEY_DELETE
    107u16 => 0x4D, // KEY_END
    109u16 => 0x4E, // KEY_PAGEDOWN
    106u16 => 0x4F, // KEY_RIGHT
    105u16 => 0x50, // KEY_LEFT
    108u16 => 0x51, // KEY_DOWN
    103u16 => 0x52, // KEY_UP
    69u16  => 0x53, // KEY_NUMLOCK
    98u16  => 0x54, // KEY_KPSLASH
    55u16  => 0x55, // KEY_KPASTERISK
    74u16  => 0x56, // KEY_KPMINUS
    78u16  => 0x57, // KEY_KPPLUS
    96u16  => 0x58, // KEY_KPENTER
    79u16  => 0x59, // KEY_KP1
    80u16  => 0x5A, // KEY_KP2
    81u16  => 0x5B, // KEY_KP3
    75u16  => 0x5C, // KEY_KP4
    76u16  => 0x5D, // KEY_KP5
    77u16  => 0x5E, // KEY_KP6
    71u16  => 0x5F, // KEY_KP7
    72u16  => 0x60, // KEY_KP8
    73u16  => 0x61, // KEY_KP9
    82u16  => 0x62, // KEY_KP0
    83u16  => 0x63, // KEY_KPDOT
    86u16  => 0x64, // KEY_102ND
    127u16 => 0x65, // KEY_COMPOSE
    116u16 => 0x66, // KEY_POWER
    117u16 => 0x67, // KEY_KPEQUAL
    183u16 => 0x68, // KEY_F13
    184u16 => 0x69, // KEY_F14
    185u16 => 0x6A, // KEY_F15
    186u16 => 0x6B, // KEY_F16
    187u16 => 0x6C, // KEY_F17
    188u16 => 0x6D, // KEY_F18
    189u16 => 0x6E, // KEY_F19
    190u16 => 0x6F, // KEY_F20
    191u16 => 0x70, // KEY_F21
    192u16 => 0x71, // KEY_F22
    193u16 => 0x72, // KEY_F23
    194u16 => 0x73, // KEY_F24
    113u16 => 0x7F, // KEY_MUTE
    115u16 => 0x80, // KEY_VOLUMEUP
    114u16 => 0x81, // KEY_VOLUMEDOWN
    29u16  => 0xE0, // KEY_LEFTCTRL
    42u16  => 0xE1, // KEY_LEFTSHIFT
    56u16  => 0xE2, // KEY_LEFTALT
    125u16 => 0xE3, // KEY_LEFTMETA
    97u16  => 0xE4, // KEY_RIGHTCTRL
    54u16  => 0xE5, // KEY_RIGHTSHIFT
    100u16 => 0xE6, // KEY_RIGHTALT
    126u16 => 0xE7, // KEY_RIGHTMETA
};

// ── evdev → X11 keysym name ───────────────────────────────────────────────────

static EVDEV_TO_X11_MAP: phf::Map<u16, &'static str> = phf_map! {
    30u16  => "a", 48u16  => "b", 46u16  => "c", 32u16  => "d",
    18u16  => "e", 33u16  => "f", 34u16  => "g", 35u16  => "h",
    23u16  => "i", 36u16  => "j", 37u16  => "k", 38u16  => "l",
    50u16  => "m", 49u16  => "n", 24u16  => "o", 25u16  => "p",
    16u16  => "q", 19u16  => "r", 31u16  => "s", 20u16  => "t",
    22u16  => "u", 47u16  => "v", 17u16  => "w", 45u16  => "x",
    21u16  => "y", 44u16  => "z",
    2u16   => "1", 3u16   => "2", 4u16   => "3", 5u16   => "4",
    6u16   => "5", 7u16   => "6", 8u16   => "7", 9u16   => "8",
    10u16  => "9", 11u16  => "0",
    28u16  => "Return",
    1u16   => "Escape",
    14u16  => "BackSpace",
    15u16  => "Tab",
    57u16  => "space",
    12u16  => "minus",
    13u16  => "equal",
    26u16  => "bracketleft",
    27u16  => "bracketright",
    43u16  => "backslash",
    39u16  => "semicolon",
    40u16  => "apostrophe",
    41u16  => "grave",
    51u16  => "comma",
    52u16  => "period",
    53u16  => "slash",
    58u16  => "Caps_Lock",
    59u16  => "F1",  60u16  => "F2",  61u16  => "F3",
    62u16  => "F4",  63u16  => "F5",  64u16  => "F6",
    65u16  => "F7",  66u16  => "F8",  67u16  => "F9",
    68u16  => "F10", 87u16  => "F11", 88u16  => "F12",
    110u16 => "Insert",
    102u16 => "Home",
    104u16 => "Prior",
    111u16 => "Delete",
    107u16 => "End",
    109u16 => "Next",
    106u16 => "Right", 105u16 => "Left",
    108u16 => "Down",  103u16 => "Up",
    69u16  => "Num_Lock",
    98u16  => "KP_Divide",
    55u16  => "KP_Multiply",
    74u16  => "KP_Subtract",
    78u16  => "KP_Add",
    96u16  => "KP_Enter",
    79u16  => "KP_End",       80u16  => "KP_Down",
    81u16  => "KP_Page_Down", 75u16  => "KP_Left",
    76u16  => "KP_Begin",     77u16  => "KP_Right",
    71u16  => "KP_Home",      72u16  => "KP_Up",
    73u16  => "KP_Page_Up",   82u16  => "KP_Insert",
    83u16  => "KP_Delete",
    99u16  => "Print",
    70u16  => "Scroll_Lock",
    119u16 => "Pause",
    29u16  => "Control_L", 97u16  => "Control_R",
    42u16  => "Shift_L",   54u16  => "Shift_R",
    56u16  => "Alt_L",     100u16 => "Alt_R",
    125u16 => "Super_L",   126u16 => "Super_R",
    113u16 => "XF86AudioMute",
    115u16 => "XF86AudioRaiseVolume",
    114u16 => "XF86AudioLowerVolume",
};

// ── Modifier bit table ────────────────────────────────────────────────────────

static MODIFIER_BIT_MAP: phf::Map<u16, u8> = phf_map! {
    29u16  => 0x01, // KEY_LEFTCTRL
    42u16  => 0x02, // KEY_LEFTSHIFT
    56u16  => 0x04, // KEY_LEFTALT
    125u16 => 0x08, // KEY_LEFTMETA
    97u16  => 0x10, // KEY_RIGHTCTRL
    54u16  => 0x20, // KEY_RIGHTSHIFT
    100u16 => 0x40, // KEY_RIGHTALT
    126u16 => 0x80, // KEY_RIGHTMETA
};

// ── Public API ────────────────────────────────────────────────────────────────

/// Look up the HID Usage ID for an evdev keycode.
///
/// Returns `None` if the key is not in the translation table.
#[inline]
pub fn evdev_to_hid(evdev_code: u16) -> Option<u8> {
    EVDEV_TO_HID_MAP.get(&evdev_code).copied()
}

/// Look up the X11 keysym name for an evdev keycode.
///
/// Returns `None` if the key is not in the translation table.
#[inline]
pub fn evdev_to_x11(evdev_code: u16) -> Option<&'static str> {
    EVDEV_TO_X11_MAP.get(&evdev_code).copied()
}

/// Return the modifier bitmask bit for an evdev keycode, if it is a modifier.
///
/// Returns `None` for non-modifier keys.
#[inline]
pub fn modifier_bit(evdev_code: u16) -> Option<u8> {
    MODIFIER_BIT_MAP.get(&evdev_code).copied()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_key_a() {
        assert_eq!(evdev_to_hid(KEY_A), Some(0x04));
    }

    #[test]
    fn hid_key_z() {
        assert_eq!(evdev_to_hid(KEY_Z), Some(0x1D));
    }

    #[test]
    fn hid_digits() {
        assert_eq!(evdev_to_hid(KEY_1), Some(0x1E));
        assert_eq!(evdev_to_hid(KEY_0), Some(0x27));
    }

    #[test]
    fn hid_modifiers() {
        assert_eq!(evdev_to_hid(KEY_LEFTCTRL),  Some(0xE0));
        assert_eq!(evdev_to_hid(KEY_RIGHTMETA), Some(0xE7));
    }

    #[test]
    fn hid_function_keys() {
        assert_eq!(evdev_to_hid(KEY_F1),  Some(0x3A));
        assert_eq!(evdev_to_hid(KEY_F12), Some(0x45));
        assert_eq!(evdev_to_hid(KEY_F13), Some(0x68));
        assert_eq!(evdev_to_hid(KEY_F24), Some(0x73));
    }

    #[test]
    fn hid_numpad() {
        assert_eq!(evdev_to_hid(KEY_KP0), Some(0x62));
        assert_eq!(evdev_to_hid(KEY_KP9), Some(0x61));
        assert_eq!(evdev_to_hid(KEY_KPENTER), Some(0x58));
    }

    #[test]
    fn hid_media_keys() {
        assert_eq!(evdev_to_hid(KEY_MUTE),       Some(0x7F));
        assert_eq!(evdev_to_hid(KEY_VOLUMEUP),   Some(0x80));
        assert_eq!(evdev_to_hid(KEY_VOLUMEDOWN), Some(0x81));
    }

    #[test]
    fn hid_unknown_key() {
        assert_eq!(evdev_to_hid(0xFFFF), None);
    }

    #[test]
    fn x11_letters() {
        assert_eq!(evdev_to_x11(KEY_A), Some("a"));
        assert_eq!(evdev_to_x11(KEY_Z), Some("z"));
    }

    #[test]
    fn x11_special_keys() {
        assert_eq!(evdev_to_x11(KEY_ENTER),     Some("Return"));
        assert_eq!(evdev_to_x11(KEY_ESC),       Some("Escape"));
        assert_eq!(evdev_to_x11(KEY_BACKSPACE),  Some("BackSpace"));
        assert_eq!(evdev_to_x11(KEY_PAGEUP),    Some("Prior"));
        assert_eq!(evdev_to_x11(KEY_PAGEDOWN),  Some("Next"));
    }

    #[test]
    fn x11_modifiers() {
        assert_eq!(evdev_to_x11(KEY_LEFTCTRL),  Some("Control_L"));
        assert_eq!(evdev_to_x11(KEY_RIGHTALT),  Some("Alt_R"));
        assert_eq!(evdev_to_x11(KEY_LEFTMETA),  Some("Super_L"));
    }

    #[test]
    fn x11_media() {
        assert_eq!(evdev_to_x11(KEY_MUTE),       Some("XF86AudioMute"));
        assert_eq!(evdev_to_x11(KEY_VOLUMEUP),   Some("XF86AudioRaiseVolume"));
        assert_eq!(evdev_to_x11(KEY_VOLUMEDOWN), Some("XF86AudioLowerVolume"));
    }

    #[test]
    fn modifier_bits_all() {
        assert_eq!(modifier_bit(KEY_LEFTCTRL),   Some(0x01));
        assert_eq!(modifier_bit(KEY_LEFTSHIFT),  Some(0x02));
        assert_eq!(modifier_bit(KEY_LEFTALT),    Some(0x04));
        assert_eq!(modifier_bit(KEY_LEFTMETA),   Some(0x08));
        assert_eq!(modifier_bit(KEY_RIGHTCTRL),  Some(0x10));
        assert_eq!(modifier_bit(KEY_RIGHTSHIFT), Some(0x20));
        assert_eq!(modifier_bit(KEY_RIGHTALT),   Some(0x40));
        assert_eq!(modifier_bit(KEY_RIGHTMETA),  Some(0x80));
    }

    #[test]
    fn modifier_bit_non_modifier() {
        assert_eq!(modifier_bit(KEY_A), None);
        assert_eq!(modifier_bit(KEY_SPACE), None);
    }
}
