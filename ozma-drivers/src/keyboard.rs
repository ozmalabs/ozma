// SPDX-License-Identifier: AGPL-3.0-only
//! Keyboard HID state and report building.
//!
//! Mirrors `KeyboardState` in `controller/hid.py` and the keymap in
//! `controller/keycodes.py`.

use evdev::Key;

/// Maximum simultaneous non-modifier keys in a boot-protocol HID report.
pub const MAX_KEYS: usize = 6;

/// Return the HID modifier bit for a modifier key, if applicable.
///
/// Mirrors `MODIFIER_BITS` in `controller/keycodes.py`.
pub fn modifier_bit(key: Key) -> Option<u8> {
    match key {
        Key::KEY_LEFTCTRL => Some(0x01),
        Key::KEY_LEFTSHIFT => Some(0x02),
        Key::KEY_LEFTALT => Some(0x04),
        Key::KEY_LEFTMETA => Some(0x08),
        Key::KEY_RIGHTCTRL => Some(0x10),
        Key::KEY_RIGHTSHIFT => Some(0x20),
        Key::KEY_RIGHTALT => Some(0x40),
        Key::KEY_RIGHTMETA => Some(0x80),
        _ => None,
    }
}

/// Map an evdev `Key` to a USB HID usage ID (page 0x07, keyboard/keypad).
///
/// Mirrors `KEYCODE_TO_HID` in `controller/keycodes.py`.
pub fn key_to_hid(key: Key) -> Option<u8> {
    let hid = match key {
        Key::KEY_A => 0x04,
        Key::KEY_B => 0x05,
        Key::KEY_C => 0x06,
        Key::KEY_D => 0x07,
        Key::KEY_E => 0x08,
        Key::KEY_F => 0x09,
        Key::KEY_G => 0x0A,
        Key::KEY_H => 0x0B,
        Key::KEY_I => 0x0C,
        Key::KEY_J => 0x0D,
        Key::KEY_K => 0x0E,
        Key::KEY_L => 0x0F,
        Key::KEY_M => 0x10,
        Key::KEY_N => 0x11,
        Key::KEY_O => 0x12,
        Key::KEY_P => 0x13,
        Key::KEY_Q => 0x14,
        Key::KEY_R => 0x15,
        Key::KEY_S => 0x16,
        Key::KEY_T => 0x17,
        Key::KEY_U => 0x18,
        Key::KEY_V => 0x19,
        Key::KEY_W => 0x1A,
        Key::KEY_X => 0x1B,
        Key::KEY_Y => 0x1C,
        Key::KEY_Z => 0x1D,
        Key::KEY_1 => 0x1E,
        Key::KEY_2 => 0x1F,
        Key::KEY_3 => 0x20,
        Key::KEY_4 => 0x21,
        Key::KEY_5 => 0x22,
        Key::KEY_6 => 0x23,
        Key::KEY_7 => 0x24,
        Key::KEY_8 => 0x25,
        Key::KEY_9 => 0x26,
        Key::KEY_0 => 0x27,
        Key::KEY_ENTER => 0x28,
        Key::KEY_ESC => 0x29,
        Key::KEY_BACKSPACE => 0x2A,
        Key::KEY_TAB => 0x2B,
        Key::KEY_SPACE => 0x2C,
        Key::KEY_MINUS => 0x2D,
        Key::KEY_EQUAL => 0x2E,
        Key::KEY_LEFTBRACE => 0x2F,
        Key::KEY_RIGHTBRACE => 0x30,
        Key::KEY_BACKSLASH => 0x31,
        Key::KEY_SEMICOLON => 0x33,
        Key::KEY_APOSTROPHE => 0x34,
        Key::KEY_GRAVE => 0x35,
        Key::KEY_COMMA => 0x36,
        Key::KEY_DOT => 0x37,
        Key::KEY_SLASH => 0x38,
        Key::KEY_CAPSLOCK => 0x39,
        Key::KEY_F1 => 0x3A,
        Key::KEY_F2 => 0x3B,
        Key::KEY_F3 => 0x3C,
        Key::KEY_F4 => 0x3D,
        Key::KEY_F5 => 0x3E,
        Key::KEY_F6 => 0x3F,
        Key::KEY_F7 => 0x40,
        Key::KEY_F8 => 0x41,
        Key::KEY_F9 => 0x42,
        Key::KEY_F10 => 0x43,
        Key::KEY_F11 => 0x44,
        Key::KEY_F12 => 0x45,
        Key::KEY_SYSRQ => 0x46,
        Key::KEY_SCROLLLOCK => 0x47,
        Key::KEY_PAUSE => 0x48,
        Key::KEY_INSERT => 0x49,
        Key::KEY_HOME => 0x4A,
        Key::KEY_PAGEUP => 0x4B,
        Key::KEY_DELETE => 0x4C,
        Key::KEY_END => 0x4D,
        Key::KEY_PAGEDOWN => 0x4E,
        Key::KEY_RIGHT => 0x4F,
        Key::KEY_LEFT => 0x50,
        Key::KEY_DOWN => 0x51,
        Key::KEY_UP => 0x52,
        Key::KEY_NUMLOCK => 0x53,
        Key::KEY_KPSLASH => 0x54,
        Key::KEY_KPASTERISK => 0x55,
        Key::KEY_KPMINUS => 0x56,
        Key::KEY_KPPLUS => 0x57,
        Key::KEY_KPENTER => 0x58,
        Key::KEY_KP1 => 0x59,
        Key::KEY_KP2 => 0x5A,
        Key::KEY_KP3 => 0x5B,
        Key::KEY_KP4 => 0x5C,
        Key::KEY_KP5 => 0x5D,
        Key::KEY_KP6 => 0x5E,
        Key::KEY_KP7 => 0x5F,
        Key::KEY_KP8 => 0x60,
        Key::KEY_KP9 => 0x61,
        Key::KEY_KP0 => 0x62,
        Key::KEY_KPDOT => 0x63,
        Key::KEY_MUTE => 0x7F,
        Key::KEY_VOLUMEUP => 0x80,
        Key::KEY_VOLUMEDOWN => 0x81,
        Key::KEY_LEFTCTRL => 0xE0,
        Key::KEY_LEFTSHIFT => 0xE1,
        Key::KEY_LEFTALT => 0xE2,
        Key::KEY_LEFTMETA => 0xE3,
        Key::KEY_RIGHTCTRL => 0xE4,
        Key::KEY_RIGHTSHIFT => 0xE5,
        Key::KEY_RIGHTALT => 0xE6,
        Key::KEY_RIGHTMETA => 0xE7,
        _ => return None,
    };
    Some(hid)
}

/// Tracks which keys are currently held and builds HID boot-protocol reports.
///
/// Mirrors `KeyboardState` in `controller/hid.py`.
#[derive(Debug, Default)]
pub struct KeyboardState {
    pub modifiers: u8,
    /// Up to 6 HID usage IDs for non-modifier keys.
    pub pressed: Vec<u8>,
}

impl KeyboardState {
    pub fn new() -> Self {
        Self::default()
    }

    /// Handle a key-down event.
    pub fn press(&mut self, key: Key) {
        if let Some(bit) = modifier_bit(key) {
            self.modifiers |= bit;
        } else if let Some(hid) = key_to_hid(key) {
            if !self.pressed.contains(&hid) {
                self.pressed.push(hid);
                if self.pressed.len() > MAX_KEYS {
                    self.pressed.remove(0);
                }
            }
        }
    }

    /// Handle a key-up event.
    pub fn release(&mut self, key: Key) {
        if let Some(bit) = modifier_bit(key) {
            self.modifiers &= !bit;
        } else if let Some(hid) = key_to_hid(key) {
            self.pressed.retain(|&k| k != hid);
        }
    }

    /// Build an 8-byte HID boot-protocol keyboard report.
    ///
    /// Layout: `[modifiers, 0x00, key1, key2, key3, key4, key5, key6]`
    pub fn build_report(&self) -> [u8; 8] {
        let mut report = [0u8; 8];
        report[0] = self.modifiers;
        // report[1] is reserved (0x00)
        for (i, &hid) in self.pressed.iter().take(MAX_KEYS).enumerate() {
            report[2 + i] = hid;
        }
        report
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_modifier_press_release() {
        let mut state = KeyboardState::new();
        state.press(Key::KEY_LEFTSHIFT);
        assert_eq!(state.modifiers, 0x02);
        state.release(Key::KEY_LEFTSHIFT);
        assert_eq!(state.modifiers, 0x00);
    }

    #[test]
    fn test_key_report() {
        let mut state = KeyboardState::new();
        state.press(Key::KEY_A);
        state.press(Key::KEY_B);
        let report = state.build_report();
        assert_eq!(report[0], 0x00); // no modifiers
        assert_eq!(report[2], 0x04); // KEY_A → HID 0x04
        assert_eq!(report[3], 0x05); // KEY_B → HID 0x05
    }

    #[test]
    fn test_max_keys_overflow() {
        let mut state = KeyboardState::new();
        for key in [
            Key::KEY_A,
            Key::KEY_B,
            Key::KEY_C,
            Key::KEY_D,
            Key::KEY_E,
            Key::KEY_F,
            Key::KEY_G, // 7th key — oldest should be dropped
        ] {
            state.press(key);
        }
        assert_eq!(state.pressed.len(), MAX_KEYS);
        // KEY_A (HID 0x04) should have been evicted
        assert!(!state.pressed.contains(&0x04));
    }

    #[test]
    fn test_release_clears_key() {
        let mut state = KeyboardState::new();
        state.press(Key::KEY_A);
        assert!(state.pressed.contains(&0x04));
        state.release(Key::KEY_A);
        assert!(!state.pressed.contains(&0x04));
    }

    #[test]
    fn test_duplicate_press_ignored() {
        let mut state = KeyboardState::new();
        state.press(Key::KEY_A);
        state.press(Key::KEY_A);
        assert_eq!(state.pressed.len(), 1);
    }
}
