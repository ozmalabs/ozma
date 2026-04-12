//! HID report types and byte-encoding.
//!
//! Byte layouts must match the HID descriptors in
//! `tinynode/gadget/setup_gadget.sh` **and** the Python reference in
//! `node/usb_hid.py`.
//!
//! Keyboard  (8 bytes): [modifier, 0x00, key1, key2, key3, key4, key5, key6]
//! Mouse     (6 bytes): [buttons, x_lo, x_hi, y_lo, y_hi, scroll_byte]

use serde::Deserialize;

pub const KBD_REPORT_LEN: usize = 8;
pub const MOUSE_REPORT_LEN: usize = 6;

// ── wire format ──────────────────────────────────────────────────────────────

/// Top-level frame received over UDP (JSON).
///
/// Exactly one of `keyboard` / `mouse` must be present.
#[derive(Debug, Deserialize)]
pub struct HidFrame {
    pub keyboard: Option<KeyboardReport>,
    pub mouse:    Option<MouseReport>,
}

// ── keyboard ─────────────────────────────────────────────────────────────────

/// Boot-protocol keyboard report.
///
/// Matches `USBHIDGadget.keyboard_report()` in `node/usb_hid.py`.
#[derive(Debug, Deserialize, Default)]
pub struct KeyboardReport {
    /// Modifier byte (Shift, Ctrl, Alt, …).
    #[serde(default)]
    pub modifiers: u8,
    /// Up to 6 key-codes; missing slots are filled with 0x00.
    #[serde(default)]
    pub keys: Vec<u8>,
}

impl KeyboardReport {
    /// Encode to the 8-byte boot-protocol keyboard report.
    ///
    /// ```
    /// use usb_hid_udp::report::KeyboardReport;
    /// let r = KeyboardReport { modifiers: 0x02, keys: vec![0x04, 0x05] };
    /// assert_eq!(r.to_bytes(), [0x02, 0x00, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00]);
    /// ```
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

// ── mouse ─────────────────────────────────────────────────────────────────────

/// Absolute mouse HID report.
///
/// Matches `USBHIDGadget.mouse_report()` in `node/usb_hid.py`.
#[derive(Debug, Deserialize, Default)]
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
    /// Signed scroll delta −127–+127.
    #[serde(default)]
    pub scroll: i8,
}

impl MouseReport {
    /// Encode to the 6-byte absolute mouse report.
    ///
    /// ```
    /// use usb_hid_udp::report::MouseReport;
    /// let r = MouseReport { buttons: 0x01, x: 0x1234, y: 0x0056, scroll: -1 };
    /// assert_eq!(r.to_bytes(), [0x01, 0x34, 0x12, 0x56, 0x00, 0xFF]);
    /// ```
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

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── keyboard ──────────────────────────────────────────────────────────────

    /// Python: USBHIDGadget.keyboard_report() → [0,0,0,0,0,0,0,0]
    #[test]
    fn kbd_empty() {
        let r = KeyboardReport::default();
        assert_eq!(r.to_bytes(), [0x00; KBD_REPORT_LEN]);
    }

    /// Python: USBHIDGadget.keyboard_report(modifiers=0x02, keys=[0x04, 0x05])
    ///       → [0x02, 0x00, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00]
    #[test]
    fn kbd_modifier_and_two_keys() {
        let r = KeyboardReport { modifiers: 0x02, keys: vec![0x04, 0x05] };
        assert_eq!(r.to_bytes(), [0x02, 0x00, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00]);
    }

    /// Keys beyond 6 are silently dropped (matches Python `slots[:6]`).
    #[test]
    fn kbd_truncates_at_six_keys() {
        let r = KeyboardReport {
            modifiers: 0,
            keys: vec![1, 2, 3, 4, 5, 6, 7, 8],
        };
        assert_eq!(r.to_bytes(), [0x00, 0x00, 1, 2, 3, 4, 5, 6]);
    }

    /// Python: USBHIDGadget.keyboard_report(modifiers=0xFF, keys=[0x28])
    ///       → [0xFF, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00]
    #[test]
    fn kbd_all_modifiers() {
        let r = KeyboardReport { modifiers: 0xFF, keys: vec![0x28] };
        assert_eq!(r.to_bytes(), [0xFF, 0x00, 0x28, 0x00, 0x00, 0x00, 0x00, 0x00]);
    }

    // ── mouse ─────────────────────────────────────────────────────────────────

    /// Python: USBHIDGadget.mouse_report() → [0,0,0,0,0,0]
    #[test]
    fn mouse_empty() {
        let r = MouseReport::default();
        assert_eq!(r.to_bytes(), [0x00; MOUSE_REPORT_LEN]);
    }

    /// Python: USBHIDGadget.mouse_report(buttons=1, x=0x1234, y=0x0056, scroll=-1)
    ///       → [0x01, 0x34, 0x12, 0x56, 0x00, 0xFF]
    #[test]
    fn mouse_buttons_xy_scroll() {
        let r = MouseReport { buttons: 0x01, x: 0x1234, y: 0x0056, scroll: -1 };
        assert_eq!(r.to_bytes(), [0x01, 0x34, 0x12, 0x56, 0x00, 0xFF]);
    }

    /// x/y are clamped to 0x7FFF (matches Python `max(0, min(0x7FFF, x))`).
    #[test]
    fn mouse_clamps_xy() {
        let r = MouseReport { buttons: 0, x: 0xFFFF, y: 0x8000, scroll: 0 };
        // 0x7FFF little-endian = [0xFF, 0x7F]
        assert_eq!(r.to_bytes(), [0x00, 0xFF, 0x7F, 0xFF, 0x7F, 0x00]);
    }

    /// scroll=127 → 0x7F, scroll=-128 → 0x80
    #[test]
    fn mouse_scroll_extremes() {
        let pos = MouseReport { scroll: 127, ..Default::default() };
        assert_eq!(pos.to_bytes()[5], 0x7F);

        let neg = MouseReport { scroll: -128, ..Default::default() };
        assert_eq!(neg.to_bytes()[5], 0x80);
    }

    /// Python: mouse_report(buttons=0x07, x=100, y=200, scroll=5)
    ///       → [7, 100, 0, 200, 0, 5]
    #[test]
    fn mouse_all_buttons_small_coords() {
        let r = MouseReport { buttons: 0x07, x: 100, y: 200, scroll: 5 };
        assert_eq!(r.to_bytes(), [0x07, 100, 0, 200, 0, 5]);
    }

    // ── JSON round-trip ───────────────────────────────────────────────────────

    #[test]
    fn kbd_json_roundtrip() {
        let json = r#"{"keyboard": {"modifiers": 2, "keys": [4, 5]}}"#;
        let frame: super::super::report::HidFrame = serde_json::from_str(json).unwrap();
        let kbd = frame.keyboard.unwrap();
        assert_eq!(kbd.to_bytes(), [0x02, 0x00, 0x04, 0x05, 0x00, 0x00, 0x00, 0x00]);
    }

    #[test]
    fn mouse_json_roundtrip() {
        let json = r#"{"mouse": {"buttons": 1, "x": 100, "y": 200, "scroll": -1}}"#;
        let frame: super::super::report::HidFrame = serde_json::from_str(json).unwrap();
        let mouse = frame.mouse.unwrap();
        assert_eq!(mouse.to_bytes(), [0x01, 100, 0, 200, 0, 0xFF]);
    }
}
