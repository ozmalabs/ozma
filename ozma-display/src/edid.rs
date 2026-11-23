// SPDX-License-Identifier: AGPL-3.0-only
//! A minimal, dependency-free EDID 1.4 builder.
//!
//! The output is a single 128-byte base block. It is intentionally modest:
//! one preferred detailed timing from [`VirtualDisplaySpec::preferred`], a few
//! classic established timings, up to eight standard timings, a monitor-name
//! descriptor, a serial-number descriptor, and a range-limits descriptor.
//! There is no CTA extension, so HDR/audio metadata is not advertised here;
//! providers that can publish extensions must add them.
//!
//! The builder is pure Rust, uses no `unsafe`, and does not shell out. It is a
//! data generator, not a driver: whether a mode is actually reachable is the
//! provider's and encoder probe's job.

use std::fmt;

use ozma_protocol::display::{DisplayMode, VirtualDisplaySpec};
use ozma_protocol::{ApiError, Result};

/// EDID base block length.
pub const EDID_LEN: usize = 128;

/// Classic established timings that are cheap to include and broadly
/// understood. They are not a claim that the capture/encode path can sustain
/// them; they are fallbacks for an OS that ignores the detailed timing.
const ESTABLISHED_640X480_60: usize = 35;
const ESTABLISHED_800X600_60: usize = 35;
const ESTABLISHED_1024X768_60: usize = 36;

/// A validated 128-byte EDID base block.
#[derive(Clone, PartialEq, Eq)]
pub struct Edid {
    bytes: [u8; EDID_LEN],
}

impl Edid {
    pub fn as_bytes(&self) -> &[u8; EDID_LEN] {
        &self.bytes
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes.to_vec()
    }

    /// The checksum byte (the last byte of the block).
    pub fn checksum(&self) -> u8 {
        self.bytes[EDID_LEN - 1]
    }

    /// True when the whole 128-byte block sums to zero modulo 256.
    pub fn checksum_valid(&self) -> bool {
        self.bytes.iter().fold(0u32, |sum, byte| sum + u32::from(*byte)) % 256 == 0
    }

    /// Decode the first detailed timing descriptor into a mode. Useful for
    /// tests and for confirming that an OS-selected mode round-trips.
    pub fn preferred_mode(&self) -> DisplayMode {
        let pixel_clock_10khz = u16::from_le_bytes([self.bytes[54], self.bytes[55]]);
        let h_active = u32::from(self.bytes[56]) | (u32::from(self.bytes[58] & 0xF0) << 4);
        let h_blank = u32::from(self.bytes[57]) | (u32::from(self.bytes[58] & 0x0F) << 8);
        let v_active = u32::from(self.bytes[59]) | (u32::from(self.bytes[61] & 0xF0) << 4);
        let v_blank = u32::from(self.bytes[60]) | (u32::from(self.bytes[61] & 0x0F) << 8);

        let h_total = h_active + h_blank;
        let v_total = v_active + v_blank;
        let pixel_clock_hz = u64::from(pixel_clock_10khz) * 10_000;
        let refresh = if h_total == 0 || v_total == 0 {
            0
        } else {
            let total = u64::from(h_total) * u64::from(v_total);
            ((pixel_clock_hz + total / 2) / total) as u32
        };

        DisplayMode::new(h_active, v_active, refresh)
    }
}

impl fmt::Debug for Edid {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Edid")
            .field("bytes", &self.bytes.len())
            .field("checksum", &self.checksum())
            .field("preferred", &self.preferred_mode())
            .finish()
    }
}

impl TryFrom<&VirtualDisplaySpec> for Edid {
    type Error = ApiError;

    fn try_from(spec: &VirtualDisplaySpec) -> Result<Self> {
        build(spec)
    }
}

/// Build a 128-byte EDID 1.4 base block for a virtual display.
///
/// Returns `invalid_request` for a spec that cannot be represented (for
/// example a detailed timing wider than 4095 pixels).
pub fn build(spec: &VirtualDisplaySpec) -> Result<Edid> {
    spec.validate()?;

    let preferred = &spec.preferred;
    if preferred.width > 4095 || preferred.height > 4095 {
        return Err(ApiError::invalid(format!(
            "EDID detailed timings cannot represent {}x{}; use a mode up to 4095 pixels",
            preferred.width, preferred.height
        )));
    }

    let mut bytes = [0u8; EDID_LEN];

    // 0-7: fixed header
    bytes[0..8].copy_from_slice(&[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]);

    // 8-9: manufacturer "OZM" (Ozma), 5 bits per letter, big-endian.
    let manufacturer: u16 = (15 << 10) | (26 << 5) | 13;
    bytes[8..10].copy_from_slice(&manufacturer.to_be_bytes());

    // 10-11: product code, stable per display id.
    let product = stable_u16(spec.id.as_str());
    bytes[10..12].copy_from_slice(&product.to_le_bytes());

    // 12-15: serial number, stable per EDID serial string.
    let serial_source = if spec.edid_serial.is_empty() {
        spec.id.as_str()
    } else {
        spec.edid_serial.as_str()
    };
    let serial = stable_u32(serial_source).max(1);
    bytes[12..16].copy_from_slice(&serial.to_le_bytes());

    // 16-17: week 1, year 2024 (EDID year is year - 1990).
    bytes[16] = 1;
    bytes[17] = 34;

    // 18-19: EDID 1.4
    bytes[18] = 1;
    bytes[19] = 4;

    // 20: digital input, 8 bits per colour, DisplayPort.
    bytes[20] = 0x80 | 0x20 | 0x05;

    // 21-22: physical size in centimetres, clamped to the byte range.
    bytes[21] = mm_to_cm(spec.physical_mm.0);
    bytes[22] = mm_to_cm(spec.physical_mm.1);

    // 23: gamma 2.20
    bytes[23] = 120;

    // 24: DPMS standby/suspend/active-off, RGB 4:4:4, preferred timing.
    bytes[24] = 0xEA;

    // 25-34: sRGB chromaticity coordinates (a standard, conservative set).
    bytes[25..35].copy_from_slice(&[
        0xEE, 0x91, 0xA3, 0x54, 0x4C, 0x99, 0x26, 0x0F, 0x50, 0x54,
    ]);

    // 35-37: established timings. 640x480@60, 800x600@60, 1024x768@60.
    bytes[ESTABLISHED_640X480_60] |= 0x20;
    bytes[ESTABLISHED_800X600_60] |= 0x01;
    bytes[ESTABLISHED_1024X768_60] |= 0x08;

    // 38-53: up to eight standard timings from the advertised modes.
    let mut slot = 0usize;
    for mode in &spec.modes {
        if slot == 8 {
            break;
        }
        if is_classic_established(mode) {
            continue;
        }
        if let Some(encoded) = standard_timing(mode) {
            let offset = 38 + slot * 2;
            bytes[offset] = encoded[0];
            bytes[offset + 1] = encoded[1];
            slot += 1;
        }
    }
    for remaining in slot..8 {
        let offset = 38 + remaining * 2;
        bytes[offset] = 0x01;
        bytes[offset + 1] = 0x01;
    }

    // 54-71: preferred detailed timing descriptor.
    let timing = DetailedTiming::for_mode(preferred)?;
    bytes[54..72].copy_from_slice(&timing.encode(spec.physical_mm));

    // 72-89: monitor name descriptor.
    let name = descriptor(0xFC, &spec.display_name());
    bytes[72..90].copy_from_slice(&name);

    // 90-107: serial number text descriptor.
    let serial_text = descriptor(0xFF, serial_source);
    bytes[90..108].copy_from_slice(&serial_text);

    // 108-125: range limits descriptor.
    bytes[108..126].copy_from_slice(&range_limits(&spec.modes));

    // 126: no extensions.
    bytes[126] = 0;

    // 127: checksum so the whole block sums to zero.
    let sum: u32 = bytes[..127].iter().map(|byte| u32::from(*byte)).sum();
    bytes[127] = (256 - (sum % 256) as i32) as u8;

    Ok(Edid { bytes })
}

fn mm_to_cm(mm: u32) -> u8 {
    (mm / 10).min(255) as u8
}

fn is_classic_established(mode: &DisplayMode) -> bool {
    matches!(
        (mode.width, mode.height, mode.refresh_hz),
        (640, 480, 60) | (800, 600, 60) | (1024, 768, 60)
    )
}

/// Encode one standard timing (EDID 1.4, 2 bytes) if the mode fits the format.
fn standard_timing(mode: &DisplayMode) -> Option<[u8; 2]> {
    if mode.width < 256 || mode.width > 2288 || mode.width % 8 != 0 {
        return None;
    }
    let width = (mode.width / 8 - 31) as u8;
    let aspect = aspect_code(mode.width, mode.height)?;
    let refresh = mode.refresh_hz.clamp(60, 123);
    Some([width, (aspect << 6) | (refresh - 60) as u8])
}

fn aspect_code(width: u32, height: u32) -> Option<u8> {
    let w = u64::from(width);
    let h = u64::from(height);
    if w * 10 == h * 16 {
        Some(0b00) // 16:10
    } else if w * 3 == h * 4 {
        Some(0b01) // 4:3
    } else if w * 4 == h * 5 {
        Some(0b10) // 5:4
    } else if w * 9 == h * 16 {
        Some(0b11) // 16:9
    } else {
        None
    }
}

/// A detailed timing, plus the blanking needed to encode it.
struct DetailedTiming {
    pixel_clock_10khz: u16,
    h_active: u16,
    h_blank: u16,
    v_active: u16,
    v_blank: u16,
    h_front_porch: u16,
    h_sync: u16,
    v_front_porch: u16,
    v_sync: u16,
}

impl DetailedTiming {
    fn for_mode(mode: &DisplayMode) -> Result<Self> {
        if mode.width == 0 || mode.height == 0 || mode.refresh_hz == 0 {
            return Err(ApiError::invalid(
                "An EDID detailed timing needs non-zero dimensions and refresh",
            ));
        }
        if mode.width > 4095 || mode.height > 4095 {
            return Err(ApiError::invalid(
                "An EDID detailed timing cannot exceed 4095 pixels in either axis",
            ));
        }

        let h_blank = (mode.width / 8).max(160).min(4095) as u16;
        let v_blank = (mode.height / 30).max(30).min(4095) as u16;

        let h_total = u64::from(mode.width) + u64::from(h_blank);
        let v_total = u64::from(mode.height) + u64::from(v_blank);
        let pixel_clock = h_total
            .saturating_mul(v_total)
            .saturating_mul(u64::from(mode.refresh_hz))
            / 10_000;
        // The base block's field is 16 bits of 10 kHz units. Modes that would
        // need more than 655.35 MHz cannot be fully described here; clamp so
        // the descriptor remains parsable and document the limitation.
        let pixel_clock_10khz = pixel_clock.clamp(1, u64::from(u16::MAX)) as u16;

        Ok(Self {
            pixel_clock_10khz,
            h_active: mode.width as u16,
            h_blank,
            v_active: mode.height as u16,
            v_blank,
            h_front_porch: (h_blank / 4).min(1023),
            h_sync: (h_blank / 8).min(1023),
            v_front_porch: (v_blank / 4).min(63),
            v_sync: (v_blank / 8).min(63),
        })
    }

    fn encode(&self, physical_mm: (u32, u32)) -> [u8; 18] {
        let h_size = physical_mm.0.min(4095) as u16;
        let v_size = physical_mm.1.min(4095) as u16;
        let mut d = [0u8; 18];

        d[0..2].copy_from_slice(&self.pixel_clock_10khz.to_le_bytes());
        d[2] = self.h_active as u8;
        d[3] = self.h_blank as u8;
        d[4] = ((self.h_active >> 8) as u8 & 0x0F) << 4 | ((self.h_blank >> 8) as u8 & 0x0F);
        d[5] = self.v_active as u8;
        d[6] = self.v_blank as u8;
        d[7] = ((self.v_active >> 8) as u8 & 0x0F) << 4 | ((self.v_blank >> 8) as u8 & 0x0F);
        d[8] = self.h_front_porch as u8;
        d[9] = self.h_sync as u8;
        d[10] = (self.v_front_porch as u8 & 0x0F) | ((self.v_sync as u8 & 0x0F) << 4);
        d[11] = ((self.h_front_porch >> 8) as u8 & 0x03) << 6
            | ((self.h_sync >> 8) as u8 & 0x03) << 4
            | ((self.v_front_porch >> 4) as u8 & 0x03) << 2
            | ((self.v_sync >> 4) as u8 & 0x03);
        d[12] = h_size as u8;
        d[13] = v_size as u8;
        d[14] = ((h_size >> 8) as u8 & 0x0F) << 4 | ((v_size >> 8) as u8 & 0x0F);
        d[15] = 0;
        d[16] = 0;
        // Digital separate sync, positive H and V sync.
        d[17] = 0x1A;
        d
    }
}

/// Build a display descriptor whose text payload is 13 bytes.
fn descriptor(tag: u8, text: &str) -> [u8; 18] {
    let mut d = [0u8; 18];
    d[0] = 0;
    d[1] = 0;
    d[2] = 0;
    d[3] = tag;
    d[4] = 0;
    let mut bytes = [b' '; 13];
    let mut len = 0usize;
    for character in text.chars() {
        if len == 13 {
            break;
        }
        let byte = if character.is_ascii_graphic() || character == ' ' {
            character as u8
        } else {
            b'?'
        };
        bytes[len] = byte;
        len += 1;
    }
    if len < 13 {
        bytes[len] = 0x0A;
    }
    d[5..18].copy_from_slice(&bytes);
    d
}

fn range_limits(modes: &[DisplayMode]) -> [u8; 18] {
    let mut min_v = u32::MAX;
    let mut max_v = 0u32;
    let mut min_h = u32::MAX;
    let mut max_h = 0u32;
    let mut max_clock_10mhz = 1u32;

    for mode in modes {
        let refresh = mode.refresh_hz.max(1);
        let h_blank = (mode.width / 8).max(160);
        let v_blank = (mode.height / 30).max(30);
        let h_total = u64::from(mode.width) + u64::from(h_blank);
        let v_total = u64::from(mode.height) + u64::from(v_blank);
        let h_freq_khz = (h_total * u64::from(refresh) / 1000).max(1);
        let clock_10mhz = (h_total * v_total * u64::from(refresh) / 10_000_000).max(1);

        min_v = min_v.min(refresh);
        max_v = max_v.max(refresh);
        min_h = min_h.min(h_freq_khz as u32);
        max_h = max_h.max(h_freq_khz as u32);
        max_clock_10mhz = max_clock_10mhz.max(clock_10mhz.min(255) as u32);
    }

    if modes.is_empty() {
        min_v = 50;
        max_v = 75;
        min_h = 30;
        max_h = 100;
    }

    let mut d = [0u8; 18];
    d[3] = 0xFD;
    d[5] = min_v.min(255) as u8;
    d[6] = max_v.min(255) as u8;
    d[7] = min_h.min(255) as u8;
    d[8] = max_h.min(255) as u8;
    d[9] = max_clock_10mhz.min(255) as u8;
    // 0x01: no timing formula; remaining padding bytes are not interpreted.
    d[10] = 0x01;
    d[11] = 0x0A;
    d
}

fn stable_u32(value: &str) -> u32 {
    // FNV-1a, chosen because it is stable across processes and platforms.
    let mut hash: u32 = 2_166_136_261;
    for byte in value.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(16_777_619);
    }
    hash
}

fn stable_u16(value: &str) -> u16 {
    let hash = stable_u32(value);
    ((hash ^ (hash >> 16)) as u16).max(1)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::display::{DisplayMode, HdrMode, Rotation};

    fn spec() -> VirtualDisplaySpec {
        let preferred = DisplayMode::new(1920, 1200, 60).hdr(HdrMode::None);
        let mut spec =
            VirtualDisplaySpec::new("laptop-1", "Remote laptop", (300, 190), preferred);
        spec.edid_serial = "OZMA-NODE-A3F2-LAPTOP-1".into();
        spec.modes.insert(1, DisplayMode::new(2560, 1440, 60));
        spec.modes.push(DisplayMode::new(1280, 720, 60));
        spec.rotation = Rotation::Deg0;
        spec
    }

    #[test]
    fn emits_one_full_base_block() {
        let edid = build(&spec()).unwrap();
        assert_eq!(edid.as_bytes().len(), EDID_LEN);
        assert_eq!(
            &edid.as_bytes()[0..8],
            &[0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00]
        );
        assert_eq!(&edid.as_bytes()[18..20], &[1, 4]);
        assert_eq!(edid.as_bytes()[126], 0, "no extension blocks");
    }

    #[test]
    fn checksum_sums_to_zero() {
        let edid = build(&spec()).unwrap();
        assert_eq!(edid.checksum_valid(), true);
        let sum: u32 = edid.as_bytes().iter().map(|b| u32::from(*b)).sum();
        assert_eq!(sum % 256, 0);
        assert_eq!(edid.checksum(), edid.as_bytes()[127]);
    }

    #[test]
    fn preferred_mode_is_the_first_detailed_timing() {
        let spec = spec();
        let edid = build(&spec).unwrap();
        let mode = edid.preferred_mode();
        assert_eq!(mode.width, spec.preferred.width);
        assert_eq!(mode.height, spec.preferred.height);
        assert_eq!(mode.refresh_hz, spec.preferred.refresh_hz);
    }

    #[test]
    fn serial_and_identity_are_stable() {
        let first = build(&spec()).unwrap();
        let second = build(&spec()).unwrap();
        assert_eq!(first.as_bytes(), second.as_bytes());

        // Product code is derived from the display id and does not drift.
        assert_eq!(&first.as_bytes()[10..12], &second.as_bytes()[10..12]);

        let mut other = spec();
        other.edid_serial = "A-DIFFERENT-SERIAL".into();
        let rebuilt = build(&other).unwrap();
        assert_ne!(&first.as_bytes()[12..16], &rebuilt.as_bytes()[12..16]);
        assert_eq!(
            &first.as_bytes()[12..16],
            &build(&spec()).unwrap().as_bytes()[12..16],
            "reconnects must keep the same serial"
        );
    }

    #[test]
    fn descriptors_carry_name_and_serial_text() {
        let edid = build(&spec()).unwrap();
        let monitor_name = &edid.as_bytes()[72..90];
        assert_eq!(&monitor_name[0..5], &[0, 0, 0, 0xFC, 0]);
        let name = String::from_utf8_lossy(&monitor_name[5..18]);
        assert!(name.starts_with("OZMA-laptop-1"), "name was {name:?}");

        let serial = &edid.as_bytes()[90..108];
        assert_eq!(&serial[0..5], &[0, 0, 0, 0xFF, 0]);
        let serial_text = String::from_utf8_lossy(&serial[5..18]);
        assert!(serial_text.starts_with("OZMA-NODE-A3"));
    }

    #[test]
    fn includes_established_and_standard_timings() {
        let edid = build(&spec()).unwrap();
        assert_eq!(edid.as_bytes()[35] & 0x21, 0x21);
        assert_eq!(edid.as_bytes()[36] & 0x08, 0x08);

        // 2560x1440@60 is 16:9, width 2560/8-31 fits a standard timing slot.
        let slot = &edid.as_bytes()[38..40];
        assert_ne!(slot, &[0x01, 0x01], "expected at least one standard timing");
        let width_px = (u32::from(slot[0]) + 31) * 8;
        assert!(width_px >= 1280, "encoded width {width_px}");
    }

    #[test]
    fn range_limits_descriptor_is_present() {
        let edid = build(&spec()).unwrap();
        let range = &edid.as_bytes()[108..126];
        assert_eq!(&range[0..4], &[0, 0, 0, 0xFD]);
        assert!(range[5] <= range[6], "min vertical <= max vertical");
        assert!(range[7] <= range[8], "min horizontal <= max horizontal");
    }

    #[test]
    fn rejects_an_invalid_spec() {
        let mut spec = spec();
        spec.id = "".into();
        let err = build(&spec).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::INVALID_REQUEST);
    }

    #[test]
    fn rejects_modes_too_wide_for_a_detailed_timing() {
        let mode = DisplayMode::new(5120, 1440, 60);
        let mut spec = VirtualDisplaySpec::new("wide", "Wide", (300, 190), mode);
        spec.modes = vec![spec.preferred.clone()];
        let err = build(&spec).unwrap_err();
        assert_eq!(err.code, ozma_protocol::codes::INVALID_REQUEST);
    }

    #[test]
    fn sanitizes_non_ascii_text() {
        let mut spec = spec();
        spec.name = "Remote — laptop ✓".into();
        let edid = build(&spec).unwrap();
        let text = &edid.as_bytes()[77..90];
        assert!(text.iter().all(|b| *b == b' ' || *b == 0x0A || (0x20..=0x7E).contains(b)));
    }
}
