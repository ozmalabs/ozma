//! USB HID boot-protocol keyboard and mouse reports.
//!
//! # Keyboard report (8 bytes)
//! ```text
//! [0]    modifier bitmask  (see [`ModifierBits`])
//! [1]    reserved 0x00
//! [2..7] up to 6 simultaneous key Usage IDs (0x00 = no key)
//! ```
//!
//! # Mouse report (4 bytes)
//! ```text
//! [0]    button bitmask  (bit 0=left, 1=right, 2=middle)
//! [1]    X delta (signed i8)
//! [2]    Y delta (signed i8)
//! [3]    scroll wheel delta (signed i8)
//! ```

use bytemuck::{Pod, Zeroable};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use zerocopy::{AsBytes, FromBytes, FromZeroes};

// ── Modifier bitmask constants ────────────────────────────────────────────────

/// Modifier byte bit positions (USB HID spec, boot-protocol keyboard).
pub mod modifier {
    pub const LEFT_CTRL: u8  = 0x01;
    pub const LEFT_SHIFT: u8 = 0x02;
    pub const LEFT_ALT: u8   = 0x04;
    pub const LEFT_GUI: u8   = 0x08;
    pub const RIGHT_CTRL: u8  = 0x10;
    pub const RIGHT_SHIFT: u8 = 0x20;
    pub const RIGHT_ALT: u8   = 0x40;
    pub const RIGHT_GUI: u8   = 0x80;
}

// ── Errors ────────────────────────────────────────────────────────────────────

#[derive(Debug, Error)]
pub enum HidReportError {
    #[error("buffer must be exactly {expected} bytes, got {got}")]
    BadLength { expected: usize, got: usize },
    #[error("keyboard report already has 6 keys pressed (rollover)")]
    KeyRollover,
}

// ── Keyboard report ───────────────────────────────────────────────────────────

/// 8-byte USB HID boot-protocol keyboard report.
///
/// Implements [`zerocopy::AsBytes`] / [`zerocopy::FromBytes`] for zero-copy
/// casting to/from `&[u8]`, and [`bytemuck::Pod`] for safe transmutation.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize,
    AsBytes, FromBytes, FromZeroes,
    Pod, Zeroable,
)]
#[repr(C)]
pub struct HidKeyboardReport {
    /// Modifier bitmask (see [`modifier`]).
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

    /// Deserialise from a 8-byte slice.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, HidReportError> {
        if buf.len() != 8 {
            return Err(HidReportError::BadLength { expected: 8, got: buf.len() });
        }
        let mut keycodes = [0u8; 6];
        keycodes.copy_from_slice(&buf[2..8]);
        Ok(Self { modifiers: buf[0], reserved: buf[1], keycodes })
    }

    /// Serialise to an 8-byte array.
    #[inline]
    pub fn to_bytes(self) -> [u8; 8] {
        let mut out = [0u8; 8];
        out[0] = self.modifiers;
        out[1] = self.reserved;
        out[2..8].copy_from_slice(&self.keycodes);
        out
    }

    /// Press a key Usage ID (up to 6 simultaneous keys).
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

impl Default for HidKeyboardReport {
    fn default() -> Self { Self::new() }
}

// ── Mouse report ──────────────────────────────────────────────────────────────

/// Button bitmask constants for [`HidMouseReport`].
pub mod mouse_button {
    pub const LEFT: u8   = 0x01;
    pub const RIGHT: u8  = 0x02;
    pub const MIDDLE: u8 = 0x04;
}

/// 4-byte USB HID boot-protocol mouse report.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq,
    Serialize, Deserialize,
    AsBytes, FromBytes, FromZeroes,
    Pod, Zeroable,
)]
#[repr(C)]
pub struct HidMouseReport {
    /// Button bitmask (see [`mouse_button`]).
    pub buttons: u8,
    /// X-axis relative movement (signed).
    pub x: i8,
    /// Y-axis relative movement (signed).
    pub y: i8,
    /// Scroll-wheel delta (signed, positive = up).
    pub wheel: i8,
}

impl HidMouseReport {
    /// Create an empty (all-zeros) report.
    #[inline]
    pub const fn new() -> Self {
        Self { buttons: 0, x: 0, y: 0, wheel: 0 }
    }

    /// Deserialise from a 4-byte slice.
    pub fn from_bytes(buf: &[u8]) -> Result<Self, HidReportError> {
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
    pub fn to_bytes(self) -> [u8; 4] {
        [self.buttons, self.x as u8, self.y as u8, self.wheel as u8]
    }
}

impl Default for HidMouseReport {
    fn default() -> Self { Self::new() }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── Keyboard ──────────────────────────────────────────────────────────────

    #[test]
    fn keyboard_round_trip_empty() {
        let report = HidKeyboardReport::new();
        let bytes = report.to_bytes();
        assert_eq!(bytes, [0u8; 8]);
        let decoded = HidKeyboardReport::from_bytes(&bytes).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn keyboard_round_trip_with_keys() {
        let mut report = HidKeyboardReport::new();
        report.modifiers = modifier::LEFT_CTRL | modifier::LEFT_SHIFT;
        report.press(0x04).unwrap(); // KEY_A
        report.press(0x28).unwrap(); // KEY_ENTER
        let bytes = report.to_bytes();
        assert_eq!(bytes[0], modifier::LEFT_CTRL | modifier::LEFT_SHIFT);
        assert_eq!(bytes[1], 0x00);
        assert_eq!(bytes[2], 0x04);
        assert_eq!(bytes[3], 0x28);
        let decoded = HidKeyboardReport::from_bytes(&bytes).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn keyboard_rollover_error() {
        let mut report = HidKeyboardReport::new();
        for i in 0x04..=0x09 {
            report.press(i).unwrap();
        }
        assert!(matches!(report.press(0x0A), Err(HidReportError::KeyRollover)));
    }

    #[test]
    fn keyboard_release() {
        let mut report = HidKeyboardReport::new();
        report.press(0x04).unwrap();
        report.press(0x05).unwrap();
        report.release(0x04);
        assert_eq!(report.keycodes[0], 0x00);
        assert_eq!(report.keycodes[1], 0x05);
    }

    #[test]
    fn keyboard_bad_length() {
        assert!(matches!(
            HidKeyboardReport::from_bytes(&[0u8; 7]),
            Err(HidReportError::BadLength { expected: 8, got: 7 })
        ));
    }

    #[test]
    fn keyboard_zerocopy_size() {
        assert_eq!(std::mem::size_of::<HidKeyboardReport>(), 8);
    }

    // ── Mouse ─────────────────────────────────────────────────────────────────

    #[test]
    fn mouse_round_trip_empty() {
        let report = HidMouseReport::new();
        let bytes = report.to_bytes();
        assert_eq!(bytes, [0u8; 4]);
        let decoded = HidMouseReport::from_bytes(&bytes).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn mouse_round_trip_with_values() {
        let report = HidMouseReport { buttons: mouse_button::LEFT, x: -10, y: 42, wheel: -1 };
        let bytes = report.to_bytes();
        assert_eq!(bytes[0], mouse_button::LEFT);
        assert_eq!(bytes[1] as i8, -10i8);
        assert_eq!(bytes[2] as i8, 42i8);
        assert_eq!(bytes[3] as i8, -1i8);
        let decoded = HidMouseReport::from_bytes(&bytes).unwrap();
        assert_eq!(report, decoded);
    }

    #[test]
    fn mouse_bad_length() {
        assert!(matches!(
            HidMouseReport::from_bytes(&[0u8; 3]),
            Err(HidReportError::BadLength { expected: 4, got: 3 })
        ));
    }

    #[test]
    fn mouse_zerocopy_size() {
        assert_eq!(std::mem::size_of::<HidMouseReport>(), 4);
    }

    #[test]
    fn mouse_negative_deltas() {
        let report = HidMouseReport { buttons: 0, x: i8::MIN, y: i8::MAX, wheel: -1 };
        let decoded = HidMouseReport::from_bytes(&report.to_bytes()).unwrap();
        assert_eq!(decoded.x, i8::MIN);
        assert_eq!(decoded.y, i8::MAX);
        assert_eq!(decoded.wheel, -1);
    }
}
