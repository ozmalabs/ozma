// SPDX-License-Identifier: AGPL-3.0-only
//! Report encoders and absolute-coordinate mapping.
//!
//! [`ozma_proto::HidKeyboardReport`] and [`ozma_proto::HidMouseReport`] model
//! the boot-protocol keyboard and the four-byte *relative* mouse report. The
//! Ozma USB gadget descriptor (`/dev/hidg1`) instead exposes a six-byte
//! **absolute** pointer report:
//!
//! ```text
//! [0] buttons   bitmask, same layout as HidMouseReport
//! [1] x low     absolute 0..=32767, little-endian
//! [2] x high
//! [3] y low
//! [4] y high
//! [5] scroll    signed i8
//! ```
//!
//! [`AbsolutePointerReport`] encodes that format. A sink is configured with
//! [`MouseProtocol::Absolute`] (the default, matching the gadget descriptor)
//! or [`MouseProtocol::Relative`] (the portable four-byte boot report).

use serde::{Deserialize, Serialize};

use ozma_protocol::input::PointerButton;

/// Largest coordinate value in an absolute HID pointer report.
pub const HID_ABSOLUTE_MAX: u16 = 32767;

/// Which mouse report layout a sink writes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MouseProtocol {
    /// Six-byte absolute pointer report used by the Ozma gadget descriptor.
    ///
    /// `PointerAbsolute`, `Touch`, and `Pen` events are supported. Relative
    /// pointer motion is rejected because an absolute device cannot express
    /// raw deltas without emulating a pointer position.
    #[default]
    Absolute,
    /// Four-byte boot-protocol relative report from [`ozma_proto::HidMouseReport`].
    ///
    /// `PointerRelative`, buttons, and wheel are supported. Absolute, touch,
    /// and pen events are rejected.
    Relative,
}

/// An absolute pointer position in HID report space (`0..=32767`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsolutePoint {
    pub x: u16,
    pub y: u16,
}

impl AbsolutePoint {
    /// Centre of the absolute report range, used before the first absolute
    /// event so an early button press does not jump the cursor to a corner.
    pub const CENTER: Self = Self { x: 16_383, y: 16_383 };

    /// Construct a point, clamping both axes to the report range.
    pub const fn new(x: u16, y: u16) -> Self {
        Self {
            x: if x > HID_ABSOLUTE_MAX { HID_ABSOLUTE_MAX } else { x },
            y: if y > HID_ABSOLUTE_MAX { HID_ABSOLUTE_MAX } else { y },
        }
    }
}

impl Default for AbsolutePoint {
    fn default() -> Self {
        Self::CENTER
    }
}

/// Maps display-local pixel coordinates into `0..=32767` report space.
///
/// The caller configures the extent of the surface the event coordinates are
/// expressed in. The default is [`AbsoluteMapping::hid_space`], which treats
/// incoming coordinates as already being in report space.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsoluteMapping {
    pub width: u32,
    pub height: u32,
}

impl AbsoluteMapping {
    /// Construct a mapping for a surface of `width` × `height` pixels.
    ///
    /// Zero extents are treated as one pixel so the mapping is always usable.
    pub const fn new(width: u32, height: u32) -> Self {
        Self {
            width: if width == 0 { 1 } else { width },
            height: if height == 0 { 1 } else { height },
        }
    }

    /// Identity mapping for callers that already produce `0..=32767`.
    pub const fn hid_space() -> Self {
        Self { width: 32_768, height: 32_768 }
    }

    /// Map a pixel position to the nearest report coordinate, clamping
    /// out-of-surface positions to the edges.
    pub fn map(&self, x: i32, y: i32) -> AbsolutePoint {
        AbsolutePoint {
            x: map_axis(x, self.width),
            y: map_axis(y, self.height),
        }
    }
}

impl Default for AbsoluteMapping {
    fn default() -> Self {
        Self::hid_space()
    }
}

fn map_axis(value: i32, extent: u32) -> u16 {
    let last = i64::from(extent.max(1)) - 1;
    if last <= 0 {
        return 0;
    }
    let clamped = i64::from(value).clamp(0, last);
    // Round to nearest rather than truncating so the midpoint of a surface
    // lands on the midpoint of the report range.
    let scaled = (clamped * i64::from(HID_ABSOLUTE_MAX) + last / 2) / last;
    scaled as u16
}

/// Six-byte absolute pointer report used by the Ozma USB gadget.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AbsolutePointerReport {
    /// Button bitmask; see [`pointer_button_bit`].
    pub buttons: u8,
    /// Absolute X in `0..=32767`.
    pub x: u16,
    /// Absolute Y in `0..=32767`.
    pub y: u16,
    /// Vertical wheel delta.
    pub scroll: i8,
}

impl AbsolutePointerReport {
    /// A neutral report at the centre of the surface.
    pub const fn empty() -> Self {
        Self { buttons: 0, x: AbsolutePoint::CENTER.x, y: AbsolutePoint::CENTER.y, scroll: 0 }
    }

    /// Construct a report at `point`.
    pub const fn new(buttons: u8, point: AbsolutePoint, scroll: i8) -> Self {
        Self { buttons, x: point.x, y: point.y, scroll }
    }

    /// Encode the six-byte on-wire report.
    pub fn to_bytes(self) -> [u8; 6] {
        let x = self.x.min(HID_ABSOLUTE_MAX);
        let y = self.y.min(HID_ABSOLUTE_MAX);
        [
            self.buttons,
            (x & 0xFF) as u8,
            ((x >> 8) & 0xFF) as u8,
            (y & 0xFF) as u8,
            ((y >> 8) & 0xFF) as u8,
            self.scroll as u8,
        ]
    }

    /// Decode a six-byte on-wire report.
    pub fn from_bytes(bytes: [u8; 6]) -> Self {
        let x = u16::from_le_bytes([bytes[1], bytes[2]]) & HID_ABSOLUTE_MAX;
        let y = u16::from_le_bytes([bytes[3], bytes[4]]) & HID_ABSOLUTE_MAX;
        Self { buttons: bytes[0], x, y, scroll: bytes[5] as i8 }
    }
}

impl Default for AbsolutePointerReport {
    fn default() -> Self {
        Self::empty()
    }
}

/// HID button bit for a protocol pointer button.
///
/// Bits 0/1/2 are left/right/middle. Bits 3/4 carry back/forward, which many
/// desktop HID stacks expose as buttons 8 and 9.
pub const fn pointer_button_bit(button: PointerButton) -> u8 {
    match button {
        PointerButton::Left => 0b0000_0001,
        PointerButton::Right => 0b0000_0010,
        PointerButton::Middle => 0b0000_0100,
        PointerButton::Back => 0b0000_1000,
        PointerButton::Forward => 0b0001_0000,
    }
}

/// Clamp an i32 delta to the signed byte range of a relative report.
pub const fn clamp_i8(value: i32) -> i8 {
    if value > i8::MAX as i32 {
        i8::MAX
    } else if value < i8::MIN as i32 {
        i8::MIN
    } else {
        value as i8
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hid_space_mapping_is_identity_for_report_coordinates() {
        let mapping = AbsoluteMapping::hid_space();
        assert_eq!(mapping.map(0, 0), AbsolutePoint { x: 0, y: 0 });
        assert_eq!(
            mapping.map(32_767, 32_767),
            AbsolutePoint { x: 32_767, y: 32_767 }
        );
        assert_eq!(mapping.map(16_383, 16_383), AbsolutePoint { x: 16_383, y: 16_383 });
    }

    #[test]
    fn pixel_mapping_clamps_and_rounds() {
        let mapping = AbsoluteMapping::new(1920, 1080);
        assert_eq!(mapping.map(-10, -10), AbsolutePoint { x: 0, y: 0 });
        assert_eq!(
            mapping.map(1919, 1079),
            AbsolutePoint { x: 32_767, y: 32_767 }
        );
        let centre = mapping.map(960, 540);
        assert!((16_000..=16_700).contains(&centre.x), "centre x was {centre:?}");
        assert!((16_000..=16_700).contains(&centre.y), "centre y was {centre:?}");
    }

    #[test]
    fn zero_extent_mapping_does_not_divide_by_zero() {
        let mapping = AbsoluteMapping::new(0, 0);
        assert_eq!(mapping.map(100, 100), AbsolutePoint { x: 0, y: 0 });
    }

    #[test]
    fn absolute_report_round_trip_and_clamping() {
        let report = AbsolutePointerReport::new(0b101, AbsolutePoint::new(1, 32_767), -1);
        let bytes = report.to_bytes();
        assert_eq!(bytes, [0b101, 0x01, 0x00, 0xFF, 0x7F, 0xFF]);
        assert_eq!(AbsolutePointerReport::from_bytes(bytes), report);

        let clamped = AbsolutePointerReport::new(0, AbsolutePoint::CENTER, 0);
        assert_eq!(AbsolutePointerReport::from_bytes(clamped.to_bytes()), clamped);
    }

    #[test]
    fn button_bits_match_hid_boot_layout() {
        assert_eq!(pointer_button_bit(PointerButton::Left), 0x01);
        assert_eq!(pointer_button_bit(PointerButton::Right), 0x02);
        assert_eq!(pointer_button_bit(PointerButton::Middle), 0x04);
        assert_eq!(pointer_button_bit(PointerButton::Back), 0x08);
        assert_eq!(pointer_button_bit(PointerButton::Forward), 0x10);
    }

    #[test]
    fn relative_deltas_clamp_to_i8() {
        assert_eq!(clamp_i8(0), 0);
        assert_eq!(clamp_i8(127), 127);
        assert_eq!(clamp_i8(128), 127);
        assert_eq!(clamp_i8(-128), -128);
        assert_eq!(clamp_i8(-129), -128);
    }
}
