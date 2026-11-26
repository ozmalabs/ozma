// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Real software H.264 encoding from packed BGRA frames.
//!
//! The configuration mirrors `ozma-encode`'s `H264SoftwareEncoder` (OpenH264,
//! baseline profile, real-time screen content, bitrate rate control, one
//! thread, no internal periodic IDR). That crate cannot be a dependency here
//! because its parent workspace pins a pre-release `x25519-dalek` that
//! conflicts with `webrtc-dtls`; the conversion and encoder settings are kept
//! deliberately identical so the wire output matches the product path.

use openh264::encoder::{
    BitRate, Encoder, EncoderConfig, FrameRate, FrameType, IntraFramePeriod, Profile,
    RateControlMode, SpsPpsStrategy, UsageType, VuiConfig,
};
use openh264::formats::YUVBuffer;
use openh264::OpenH264API;

/// Smallest bitrate accepted over signaling, in bits per second.
pub const MIN_BITRATE_BPS: u32 = 500_000;
/// Largest bitrate accepted over signaling, in bits per second.
pub const MAX_BITRATE_BPS: u32 = 50_000_000;

/// Clamp a requested bitrate to the helper's supported range.
pub fn clamp_bitrate_bps(requested: u64) -> u32 {
    requested.clamp(u64::from(MIN_BITRATE_BPS), u64::from(MAX_BITRATE_BPS)) as u32
}

/// Convert bits-per-second to the whole kbps the encoder wants.
pub fn bps_to_kbps(bps: u32) -> u32 {
    (bps / 1_000).max(1)
}

/// An encoder failure with a human-readable reason.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct EncodeError(pub String);

#[derive(Debug)]
struct I420Frame {
    data: Vec<u8>,
    width: usize,
    height: usize,
}

impl I420Frame {
    fn new(width: usize, height: usize) -> Self {
        debug_assert!(width % 2 == 0 && height % 2 == 0);
        Self {
            data: vec![0u8; width * height * 3 / 2],
            width,
            height,
        }
    }

    fn set_luma(&mut self, x: usize, y: usize, value: u8) {
        self.data[y * self.width + x] = value;
    }

    fn set_chroma(&mut self, x: usize, y: usize, u: u8, v: u8) {
        let chroma_width = self.width / 2;
        let chroma_height = self.height / 2;
        self.data[self.width * self.height + (y / 2) * chroma_width + x / 2] = u;
        self.data[self.width * self.height
            + chroma_width * chroma_height
            + (y / 2) * chroma_width
            + x / 2] = v;
    }
}

/// Limited-range RGB -> YUV matrix, matching the SPS VUI and `ozma-encode`.
#[derive(Clone, Copy)]
enum ColorMatrix {
    Bt601,
    Bt709,
}

impl ColorMatrix {
    fn for_frame(width: usize, height: usize) -> Self {
        if height >= 720 || width >= 1280 {
            Self::Bt709
        } else {
            Self::Bt601
        }
    }

    fn vui(self) -> VuiConfig {
        match self {
            Self::Bt601 => VuiConfig::bt601(),
            Self::Bt709 => VuiConfig::bt709(),
        }
    }
}

/// Convert packed BGRA to tightly packed I420, cropping odd dimensions down.
fn bgra_to_i420(data: &[u8], width: usize, height: usize) -> Result<I420Frame, EncodeError> {
    let coded_width = width & !1;
    let coded_height = height & !1;
    if coded_width < 2 || coded_height < 2 {
        return Err(EncodeError(format!(
            "cannot encode a {width}x{height} BGRA frame"
        )));
    }
    let needed = width
        .checked_mul(height)
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| EncodeError("BGRA frame size overflows usize".to_owned()))?;
    if data.len() < needed {
        return Err(EncodeError(format!(
            "BGRA frame is {} bytes, expected at least {needed} for {width}x{height}",
            data.len()
        )));
    }

    let matrix = ColorMatrix::for_frame(coded_width, coded_height);
    let mut frame = I420Frame::new(coded_width, coded_height);

    for y in 0..coded_height {
        for x in 0..coded_width {
            let index = (y * width + x) * 4;
            let (luma, _, _) = yuv_from_rgb(data[index + 2], data[index + 1], data[index], matrix);
            frame.set_luma(x, y, luma);
        }
    }
    for y in (0..coded_height).step_by(2) {
        for x in (0..coded_width).step_by(2) {
            let mut u_sum = 0u32;
            let mut v_sum = 0u32;
            for dy in 0..2 {
                for dx in 0..2 {
                    let index = ((y + dy) * width + (x + dx)) * 4;
                    let (_, u, v) =
                        yuv_from_rgb(data[index + 2], data[index + 1], data[index], matrix);
                    u_sum += u32::from(u);
                    v_sum += u32::from(v);
                }
            }
            frame.set_chroma(x, y, ((u_sum + 2) / 4) as u8, ((v_sum + 2) / 4) as u8);
        }
    }
    Ok(frame)
}

#[inline]
fn yuv_from_rgb(r: u8, g: u8, b: u8, matrix: ColorMatrix) -> (u8, u8, u8) {
    let r = i32::from(r);
    let g = i32::from(g);
    let b = i32::from(b);
    let (y, u, v) = match matrix {
        ColorMatrix::Bt601 => (
            ((66 * r + 129 * g + 25 * b + 128) >> 8) + 16,
            ((-38 * r - 74 * g + 112 * b + 128) >> 8) + 128,
            ((112 * r - 94 * g - 18 * b + 128) >> 8) + 128,
        ),
        ColorMatrix::Bt709 => (
            ((47 * r + 157 * g + 16 * b + 128) >> 8) + 16,
            ((-26 * r - 87 * g + 112 * b + 128) >> 8) + 128,
            ((112 * r - 102 * g - 10 * b + 128) >> 8) + 128,
        ),
    };
    (
        y.clamp(0, 255) as u8,
        u.clamp(0, 255) as u8,
        v.clamp(0, 255) as u8,
    )
}

/// NAL unit types found in an Annex-B buffer.
pub fn annex_b_nal_types(data: &[u8]) -> Vec<u8> {
    let mut types = Vec::new();
    let mut index = 0;
    while index + 3 <= data.len() {
        let start_len = if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else {
            index += 1;
            continue;
        };
        let header = index + start_len;
        if header < data.len() {
            types.push(data[header] & 0x1f);
        }
        index = header;
    }
    types
}

fn build_encoder(
    width: usize,
    height: usize,
    fps: u32,
    bitrate_kbps: u32,
) -> Result<Encoder, EncodeError> {
    let matrix = ColorMatrix::for_frame(width & !1, height & !1);
    let config = EncoderConfig::new()
        .bitrate(BitRate::from_bps(bitrate_kbps.saturating_mul(1_000)))
        .max_frame_rate(FrameRate::from_hz(fps as f32))
        .rate_control_mode(RateControlMode::Bitrate)
        .usage_type(UsageType::ScreenContentRealTime)
        .profile(Profile::Baseline)
        .vui(matrix.vui())
        .sps_pps_strategy(SpsPpsStrategy::ConstantId)
        // One IDR per second. OpenH264 treats period 0 as "auto", which is not
        // a disabled timer; an explicit period keeps the cadence predictable.
        .intra_frame_period(IntraFramePeriod::from_num_frames(fps.max(1)))
        .skip_frames(true)
        .adaptive_quantization(false)
        .background_detection(false)
        .num_threads(1);
    Encoder::with_api_config(OpenH264API::from_source(), config)
        .map_err(|error| EncodeError(format!("failed to initialise OpenH264: {error}")))
}

/// A real OpenH264 encoder that can change bitrate by rebuilding its codec.
pub struct BgraEncoder {
    width: usize,
    height: usize,
    fps: u32,
    bitrate_kbps: u32,
    encoder: Encoder,
    frames_since_idr: u32,
    force_idr: bool,
}

impl BgraEncoder {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Result<Self, EncodeError> {
        let width = width as usize;
        let height = height as usize;
        let encoder = build_encoder(width, height, fps, bitrate_kbps)?;
        Ok(Self {
            width,
            height,
            fps,
            bitrate_kbps,
            encoder,
            frames_since_idr: 0,
            force_idr: true,
        })
    }

    /// Encoded (even-cropped) width.
    pub fn coded_width(&self) -> u32 {
        (self.width & !1) as u32
    }

    /// Encoded (even-cropped) height.
    pub fn coded_height(&self) -> u32 {
        (self.height & !1) as u32
    }

    pub fn bitrate_kbps(&self) -> u32 {
        self.bitrate_kbps
    }

    /// Rebuild the encoder when the bitrate changed. The next frame is an IDR.
    pub fn set_bitrate(&mut self, bitrate_kbps: u32) -> Result<bool, EncodeError> {
        if bitrate_kbps == self.bitrate_kbps {
            return Ok(false);
        }
        self.encoder = build_encoder(self.width, self.height, self.fps, bitrate_kbps)?;
        self.bitrate_kbps = bitrate_kbps;
        self.frames_since_idr = 0;
        self.force_idr = true;
        Ok(true)
    }

    /// Encode one packed BGRA frame into Annex-B H.264 access units.
    ///
    /// A keyframe is forced when `force_keyframe` is set, after a bitrate
    /// change, and once per second so a late peer can decode.
    pub fn encode(&mut self, bgra: &[u8], force_keyframe: bool) -> Result<Vec<u8>, EncodeError> {
        let i420 = bgra_to_i420(bgra, self.width, self.height)?;
        let yuv = YUVBuffer::from_vec(i420.data, i420.width, i420.height);

        let periodic = self.frames_since_idr.saturating_add(1) >= self.fps.max(1);
        if force_keyframe || self.force_idr || periodic {
            self.encoder.force_intra_frame();
        }
        self.force_idr = false;

        let (mut data, mut frame_type) = {
            let bitstream = self
                .encoder
                .encode(&yuv)
                .map_err(|error| EncodeError(format!("OpenH264 encode failed: {error}")))?;
            (bitstream.to_vec(), bitstream.frame_type())
        };

        if data.is_empty() || frame_type == FrameType::Skip {
            self.encoder.force_intra_frame();
            let bitstream = self.encoder.encode(&yuv).map_err(|error| {
                EncodeError(format!("OpenH264 forced-IDR encode failed: {error}"))
            })?;
            data = bitstream.to_vec();
            frame_type = bitstream.frame_type();
        }

        if data.is_empty() || frame_type == FrameType::Invalid {
            return Err(EncodeError(
                "OpenH264 produced an empty or invalid access unit".to_owned(),
            ));
        }
        let types = annex_b_nal_types(&data);
        if types.is_empty() {
            return Err(EncodeError(
                "OpenH264 output had no Annex-B start code".to_owned(),
            ));
        }
        if types.contains(&5) {
            self.frames_since_idr = 0;
        } else {
            self.frames_since_idr = self.frames_since_idr.saturating_add(1);
        }
        Ok(data)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_pattern(width: usize, height: usize, phase: u8) -> Vec<u8> {
        let mut data = Vec::with_capacity(width * height * 4);
        for y in 0..height {
            for x in 0..width {
                data.push((x as u8).wrapping_add(phase)); // B
                data.push((y as u8).wrapping_add(phase)); // G
                data.push(((x + y) as u8).wrapping_add(phase)); // R
                data.push(0xFF); // A
            }
        }
        data
    }

    #[test]
    fn bitrate_is_clamped_to_the_helper_range() {
        assert_eq!(clamp_bitrate_bps(0), MIN_BITRATE_BPS);
        assert_eq!(clamp_bitrate_bps(100_000), MIN_BITRATE_BPS);
        assert_eq!(clamp_bitrate_bps(4_000_000), 4_000_000);
        assert_eq!(clamp_bitrate_bps(u64::MAX), MAX_BITRATE_BPS);
        assert_eq!(bps_to_kbps(500_000), 500);
        assert_eq!(bps_to_kbps(1), 1);
    }

    #[test]
    fn bgra_conversion_is_limited_range() {
        let white = [255u8, 255, 255, 255].repeat(4);
        let frame = bgra_to_i420(&white, 2, 2).expect("convert");
        assert_eq!(&frame.data[..4], &[235, 235, 235, 235]);
        assert_eq!(frame.data[4], 128);
        assert_eq!(frame.data[5], 128);
    }

    #[test]
    fn bgra_conversion_rejects_short_payloads() {
        let error = bgra_to_i420(&[0; 7], 2, 2).expect_err("short frame");
        assert!(error.to_string().contains("expected at least"), "{error}");
    }

    #[test]
    fn encodes_real_annex_b_with_sps_pps_and_idr() {
        let mut encoder = BgraEncoder::new(64, 48, 15, 1_000).expect("encoder");
        let first = encoder
            .encode(&test_pattern(64, 48, 0), true)
            .expect("first frame");
        let types = annex_b_nal_types(&first);
        assert!(types.contains(&7), "missing SPS in {types:?}");
        assert!(types.contains(&8), "missing PPS in {types:?}");
        assert!(types.contains(&5), "missing IDR in {types:?}");

        let second = encoder
            .encode(&test_pattern(64, 48, 0), false)
            .expect("second frame");
        let second_types = annex_b_nal_types(&second);
        assert!(!second.is_empty());
        assert!(
            !second_types.contains(&5),
            "expected a P-frame, got NAL types {second_types:?}"
        );
    }

    #[test]
    fn bitrate_change_rebuilds_and_forces_an_idr() {
        let mut encoder = BgraEncoder::new(64, 48, 15, 1_000).expect("encoder");
        let _ = encoder
            .encode(&test_pattern(64, 48, 0), true)
            .expect("first frame");
        assert!(encoder.set_bitrate(2_000).expect("rebuild"));
        let frame = encoder
            .encode(&test_pattern(64, 48, 8), false)
            .expect("frame after change");
        assert!(annex_b_nal_types(&frame).contains(&5));
        assert_eq!(encoder.bitrate_kbps(), 2_000);
        assert!(!encoder.set_bitrate(2_000).expect("no-op"));
    }

    #[test]
    fn zero_periodic_interval_still_emits_first_idr_even_without_force() {
        let mut encoder = BgraEncoder::new(32, 32, 30, 500).expect("encoder");
        let frame = encoder
            .encode(&test_pattern(32, 32, 0), false)
            .expect("first frame");
        // `new` starts with force_idr set so a peer can always decode the
        // first sample, regardless of caller flags.
        assert!(annex_b_nal_types(&frame).contains(&5));
    }
}
