//! HID report frame types.
//!
//! Wire format (UDP payload): newline-delimited JSON, one object per datagram.
//!
//! ```json
//! {"type":"keyboard","modifiers":0,"keys":[4,0,0,0,0,0]}
//! {"type":"mouse","buttons":0,"x":16383,"y":16383,"scroll":0}
//! ```
//!
//! Byte layouts match node/usb_hid.py exactly:
//!   Keyboard : [modifier, 0x00, key1..key6]          — 8 bytes
//!   Mouse    : [buttons, x_lo, x_hi, y_lo, y_hi, scroll] — 6 bytes

use serde::Deserialize;

pub const KBD_REPORT_LEN: usize = 8;
pub const MOUSE_REPORT_LEN: usize = 6;

/// A single HID report received over UDP.
#[derive(Debug, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum HidReport {
    Keyboard(KeyboardReport),
    Mouse(MouseReport),
}

#[derive(Debug, Deserialize)]
pub struct KeyboardReport {
    /// Modifier byte (Shift, Ctrl, Alt, …).
    #[serde(default)]
    pub modifiers: u8,
    /// Up to 6 key-codes; missing slots are filled with 0x00.
    #[serde(default)]
    pub keys: Vec<u8>,
}

#[derive(Debug, Deserialize)]
pub struct MouseReport {
    /// Button bitmask: bit 0 = left, 1 = right, 2 = middle.
    #[serde(default)]
    pub buttons: u8,
    /// Absolute X position 0–32767.
    #[serde(default)]
    pub x: u16,
    /// Absolute Y position 0–32767.
    #[serde(default)]
    pub y: u16,
    /// Scroll wheel, signed −127–+127.
    #[serde(default)]
    pub scroll: i8,
}

impl KeyboardReport {
    /// Encode to the 8-byte boot-protocol keyboard report.
    ///
    /// Matches `USBHIDGadget.keyboard_report()` in usb_hid.py:
    ///   [modifier, 0x00, key1, key2, key3, key4, key5, key6]
    pub fn to_bytes(&self) -> [u8; KBD_REPORT_LEN] {
        let mut buf = [0u8; KBD_REPORT_LEN];
        buf[0] = self.modifiers;
        buf[1] = 0x00; // reserved
        for (i, &k) in self.keys.iter().take(6).enumerate() {
            buf[2 + i] = k;
        }
        buf
    }
}

impl MouseReport {
    /// Encode to the 6-byte absolute mouse report.
    ///
    /// Matches `USBHIDGadget.mouse_report()` in usb_hid.py:
    ///   [buttons, x_lo, x_hi, y_lo, y_hi, scroll_byte]
    pub fn to_bytes(&self) -> [u8; MOUSE_REPORT_LEN] {
        let x = self.x.min(0x7FFF);
        let y = self.y.min(0x7FFF);
        [
            self.buttons,
            (x & 0xFF) as u8,
            ((x >> 8) & 0xFF) as u8,
            (y & 0xFF) as u8,
            ((y >> 8) & 0xFF) as u8,
            self.scroll as u8, // reinterpret signed as unsigned byte
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── keyboard ──────────────────────────────────────────────────────────────

    #[test]
    fn kbd_empty_report() {
        let r = KeyboardReport { modifiers: 0, keys: vec![] };
        assert_eq!(r.to_bytes(), [0, 0, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn kbd_modifier_only() {
        // Left-Shift = 0x02
        let r = KeyboardReport { modifiers: 0x02, keys: vec![] };
        assert_eq!(r.to_bytes(), [0x02, 0x00, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn kbd_keys_truncated_to_six() {
        let r = KeyboardReport {
            modifiers: 0,
            keys: vec![4, 5, 6, 7, 8, 9, 10, 11], // 8 keys — only first 6 used
        };
        assert_eq!(r.to_bytes(), [0, 0, 4, 5, 6, 7, 8, 9]);
    }

    #[test]
    fn kbd_reserved_byte_always_zero() {
        let r = KeyboardReport { modifiers: 0xFF, keys: vec![4] };
        assert_eq!(r.to_bytes()[1], 0x00, "reserved byte must be 0x00");
    }

    // ── mouse ─────────────────────────────────────────────────────────────────

    #[test]
    fn mouse_zero_report() {
        let r = MouseReport { buttons: 0, x: 0, y: 0, scroll: 0 };
        assert_eq!(r.to_bytes(), [0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn mouse_centre() {
        // Centre of 0–32767 range = 16383 (0x3FFF)
        let r = MouseReport { buttons: 0, x: 16383, y: 16383, scroll: 0 };
        let b = r.to_bytes();
        assert_eq!(b[1], 0xFF); // x_lo
        assert_eq!(b[2], 0x3F); // x_hi
        assert_eq!(b[3], 0xFF); // y_lo
        assert_eq!(b[4], 0x3F); // y_hi
    }

    #[test]
    fn mouse_max_position() {
        let r = MouseReport { buttons: 0, x: 0x7FFF, y: 0x7FFF, scroll: 0 };
        let b = r.to_bytes();
        assert_eq!(b[1], 0xFF);
        assert_eq!(b[2], 0x7F);
        assert_eq!(b[3], 0xFF);
        assert_eq!(b[4], 0x7F);
    }

    #[test]
    fn mouse_clamps_x_y_above_max() {
        // Python: x = max(0, min(0x7FFF, x))
        let r = MouseReport { buttons: 0, x: 0xFFFF, y: 0xFFFF, scroll: 0 };
        let b = r.to_bytes();
        assert_eq!(b[1], 0xFF);
        assert_eq!(b[2], 0x7F);
    }

    #[test]
    fn mouse_negative_scroll() {
        // scroll = -1  →  0xFF as unsigned byte
        let r = MouseReport { buttons: 0, x: 0, y: 0, scroll: -1 };
        assert_eq!(r.to_bytes()[5], 0xFF);
    }

    #[test]
    fn mouse_positive_scroll() {
        let r = MouseReport { buttons: 0, x: 0, y: 0, scroll: 3 };
        assert_eq!(r.to_bytes()[5], 0x03);
    }

    #[test]
    fn mouse_buttons() {
        // left=1, right=2, middle=4
        let r = MouseReport { buttons: 0b0000_0111, x: 0, y: 0, scroll: 0 };
        assert_eq!(r.to_bytes()[0], 7);
    }

    // ── JSON deserialisation ──────────────────────────────────────────────────

    #[test]
    fn parse_keyboard_json() {
        let json = r#"{"type":"keyboard","modifiers":2,"keys":[4,5]}"#;
        let report: HidReport = serde_json::from_str(json).unwrap();
        if let HidReport::Keyboard(k) = report {
            assert_eq!(k.modifiers, 2);
            assert_eq!(k.keys, vec![4, 5]);
        } else {
            panic!("expected Keyboard variant");
        }
    }

    #[test]
    fn parse_mouse_json() {
        let json = r#"{"type":"mouse","buttons":1,"x":100,"y":200,"scroll":-1}"#;
        let report: HidReport = serde_json::from_str(json).unwrap();
        if let HidReport::Mouse(m) = report {
            assert_eq!(m.buttons, 1);
            assert_eq!(m.x, 100);
            assert_eq!(m.y, 200);
            assert_eq!(m.scroll, -1);
        } else {
            panic!("expected Mouse variant");
        }
    }

    #[test]
    fn parse_keyboard_defaults() {
        // modifiers and keys are optional
        let json = r#"{"type":"keyboard"}"#;
        let report: HidReport = serde_json::from_str(json).unwrap();
        if let HidReport::Keyboard(k) = report {
            assert_eq!(k.modifiers, 0);
            assert!(k.keys.is_empty());
        } else {
            panic!("expected Keyboard variant");
        }
    }
}
