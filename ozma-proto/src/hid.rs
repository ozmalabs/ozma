//! HID report structs shared across crates.

use serde::{Deserialize, Serialize};

/// 8-byte HID keyboard boot report.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct KeyboardReport {
    /// Modifier byte (Ctrl, Shift, Alt, GUI).
    pub modifiers: u8,
    /// Reserved byte (always 0).
    pub reserved: u8,
    /// Up to 6 simultaneous key codes.
    pub keycodes: [u8; 6],
}

impl Default for KeyboardReport {
    fn default() -> Self {
        Self {
            modifiers: 0,
            reserved: 0,
            keycodes: [0u8; 6],
        }
    }
}

/// 6-byte absolute mouse report.
///
/// Layout:
/// - `[0]`    buttons bitmask (bit 0 = left, 1 = right, 2 = middle)
/// - `[1..2]` X little-endian 0–32767
/// - `[3..4]` Y little-endian 0–32767
/// - `[5]`    scroll (signed byte)
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MouseReport {
    pub buttons: u8,
    pub x: u16,
    pub y: u16,
    pub scroll: i8,
}

impl Default for MouseReport {
    fn default() -> Self {
        Self {
            buttons: 0,
            x: 0,
            y: 0,
            scroll: 0,
        }
    }
}
