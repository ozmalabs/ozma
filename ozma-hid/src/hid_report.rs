//! HID report parsing.
//!
//! Matches the UDP wire format used by the Ozma node/controller:
//!
//!   Keyboard (pkt_type = 0x01):
//!     [modifier, reserved, key1, key2, key3, key4, key5, key6]  (8 bytes)
//!
//!   Mouse (pkt_type = 0x02):
//!     [buttons, x_lo, x_hi, y_lo, y_hi, scroll]  (6 bytes)
//!     x/y are 0-32767 absolute coordinates.

use thiserror::Error;

/// Errors from report parsing.
#[derive(Debug, Error)]
pub enum ReportError {
    #[error("packet too short: need {need} bytes, got {got}")]
    TooShort { need: usize, got: usize },
    #[error("unknown packet type: 0x{0:02X}")]
    UnknownType(u8),
}

/// A parsed keyboard HID boot report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyboardReport {
    /// Modifier byte (bit 0=LCtrl, 1=LShift, 2=LAlt, 3=LMeta,
    ///                  4=RCtrl, 5=RShift, 6=RAlt/AltGr, 7=RMeta)
    pub modifier: u8,
    /// Up to 6 simultaneously pressed HID usage IDs (0 = empty slot).
    pub keys: [u8; 6],
}

impl KeyboardReport {
    /// Parse from an 8-byte HID boot keyboard report.
    pub fn from_bytes(b: &[u8]) -> Result<Self, ReportError> {
        if b.len() < 8 {
            return Err(ReportError::TooShort { need: 8, got: b.len() });
        }
        let mut keys = [0u8; 6];
        keys.copy_from_slice(&b[2..8]);
        Ok(Self { modifier: b[0], keys })
    }

    /// Active (non-zero) key usage IDs.
    pub fn active_keys(&self) -> impl Iterator<Item = u8> + '_ {
        self.keys.iter().copied().filter(|&k| k != 0)
    }
}

/// A parsed mouse HID report.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MouseReport {
    /// Button bitmask: bit 0=left, 1=right, 2=middle.
    pub buttons: u8,
    /// Absolute X position, 0-32767.
    pub x: u16,
    /// Absolute Y position, 0-32767.
    pub y: u16,
    /// Scroll wheel delta (signed, positive = up).
    pub scroll: i8,
}

impl MouseReport {
    /// Parse from a 6-byte HID mouse report.
    pub fn from_bytes(b: &[u8]) -> Result<Self, ReportError> {
        if b.len() < 6 {
            return Err(ReportError::TooShort { need: 6, got: b.len() });
        }
        let x = u16::from_le_bytes([b[1], b[2]]);
        let y = u16::from_le_bytes([b[3], b[4]]);
        let scroll = b[5] as i8;
        Ok(Self { buttons: b[0], x, y, scroll })
    }
}

/// A framed Ozma UDP packet.
#[derive(Debug, Clone)]
pub enum OzmaPacket {
    Keyboard(KeyboardReport),
    Mouse(MouseReport),
}

impl OzmaPacket {
    /// Parse a raw UDP datagram (first byte = packet type).
    pub fn parse(data: &[u8]) -> Result<Self, ReportError> {
        if data.is_empty() {
            return Err(ReportError::TooShort { need: 1, got: 0 });
        }
        match data[0] {
            0x01 => Ok(Self::Keyboard(KeyboardReport::from_bytes(&data[1..])?)),
            0x02 => Ok(Self::Mouse(MouseReport::from_bytes(&data[1..])?)),
            t    => Err(ReportError::UnknownType(t)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_keyboard_report() {
        // modifier=LShift, key=0x04 ('a')
        let pkt = [0x01u8, 0x02, 0x00, 0x04, 0, 0, 0, 0, 0];
        let p = OzmaPacket::parse(&pkt).unwrap();
        match p {
            OzmaPacket::Keyboard(r) => {
                assert_eq!(r.modifier, 0x02);
                assert_eq!(r.keys[0], 0x04);
            }
            _ => panic!("expected keyboard"),
        }
    }

    #[test]
    fn parse_mouse_report() {
        let x: u16 = 16383;
        let y: u16 = 8191;
        let pkt = [0x02u8, 0x01, x as u8, (x >> 8) as u8, y as u8, (y >> 8) as u8, 0xFE];
        let p = OzmaPacket::parse(&pkt).unwrap();
        match p {
            OzmaPacket::Mouse(r) => {
                assert_eq!(r.buttons, 0x01);
                assert_eq!(r.x, x);
                assert_eq!(r.y, y);
                assert_eq!(r.scroll, -2);
            }
            _ => panic!("expected mouse"),
        }
    }
}
