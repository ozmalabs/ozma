// SPDX-License-Identifier: AGPL-3.0-only
//! Decoded video frames, I420 planes, and headless conversion helpers.
//!
//! Clients receive Annex-B H.264 access units, decode them to I420, and then
//! either hand the planes to a platform renderer or convert them for a
//! screenshot. This module owns the owned, transport-independent frame type;
//! the OpenH264 wrapper lives in [`crate::h264`].
//!
//! The conversions deliberately have no GUI dependency: [`to_rgba`],
//! [`to_ppm`](DecodedVideoFrame::to_ppm), and
//! [`write_ppm`](DecodedVideoFrame::write_ppm) are enough for headless CI,
//! diagnostics, and a first "did a frame arrive?" check.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::DecodeError;
use crate::video::VideoFormat;
use ozma_protocol::Codec;

/// Metadata that travels with one encoded access unit through decode.
///
/// The client reassembler already carries these fields; decode must preserve
/// them exactly so A/V sync, keyframe detection, and frame drop accounting do
/// not depend on the codec bitstream.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeMeta {
    /// Presentation timestamp in microseconds on the sender's media clock.
    pub pts_micros: u64,
    /// Monotonic frame sequence number assigned by the sender.
    pub sequence: u64,
    /// Whether the sender marked this access unit as independently decodable.
    pub keyframe: bool,
}

impl DecodeMeta {
    /// Construct metadata for one access unit.
    pub const fn new(pts_micros: u64, sequence: u64, keyframe: bool) -> Self {
        Self {
            pts_micros,
            sequence,
            keyframe,
        }
    }
}

/// One tightly packed I420 (YUV 4:2:0 planar) picture.
///
/// Rows contain no stride padding: the length of `y` is `width * height`, and
/// each chroma plane is `ceil(width / 2) * ceil(height / 2)`. Encoders in this
/// workspace always produce even dimensions, so the `ceil` case is only a
/// defensive allowance.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct I420Planes {
    /// Luma plane, `width * height` bytes.
    pub y: Vec<u8>,
    /// Blue-difference chroma plane.
    pub u: Vec<u8>,
    /// Red-difference chroma plane.
    pub v: Vec<u8>,
}

impl I420Planes {
    /// Wrap three planes.
    pub fn new(y: Vec<u8>, u: Vec<u8>, v: Vec<u8>) -> Self {
        Self { y, u, v }
    }

    /// Nominal luma length for a frame, or `None` on overflow.
    pub fn expected_luma_len(width: u32, height: u32) -> Option<usize> {
        let width = usize::try_from(width).ok()?;
        let height = usize::try_from(height).ok()?;
        width.checked_mul(height)
    }

    /// Nominal length of one chroma plane, or `None` on overflow.
    pub fn expected_chroma_len(width: u32, height: u32) -> Option<usize> {
        let width = usize::try_from(width).ok()?.div_ceil(2);
        let height = usize::try_from(height).ok()?.div_ceil(2);
        width.checked_mul(height)
    }

    /// Check the planes against a frame geometry.
    pub fn validate(&self, width: u32, height: u32) -> Result<(), DecodeError> {
        let luma = Self::expected_luma_len(width, height)
            .ok_or_else(|| DecodeError::invalid_frame("frame dimensions overflow usize"))?;
        let chroma = Self::expected_chroma_len(width, height)
            .ok_or_else(|| DecodeError::invalid_frame("frame dimensions overflow usize"))?;

        if self.y.len() != luma {
            return Err(DecodeError::invalid_frame(format!(
                "luma plane is {} bytes but {width}x{height} needs {luma}",
                self.y.len()
            )));
        }
        if self.u.len() != chroma {
            return Err(DecodeError::invalid_frame(format!(
                "U plane is {} bytes but {width}x{height} needs {chroma}",
                self.u.len()
            )));
        }
        if self.v.len() != chroma {
            return Err(DecodeError::invalid_frame(format!(
                "V plane is {} bytes but {width}x{height} needs {chroma}",
                self.v.len()
            )));
        }
        Ok(())
    }

    /// Total bytes across all three planes.
    pub fn len(&self) -> usize {
        self.y.len() + self.u.len() + self.v.len()
    }

    /// True when all planes are empty.
    pub fn is_empty(&self) -> bool {
        self.y.is_empty() && self.u.is_empty() && self.v.is_empty()
    }
}

/// A decoded picture with the metadata from its source access unit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DecodedVideoFrame {
    /// Presentation timestamp copied from the encoded access unit.
    pub pts_micros: u64,
    /// Sequence number copied from the encoded access unit.
    pub sequence: u64,
    /// Whether the source access unit was a keyframe.
    pub keyframe: bool,
    /// Visible width in pixels.
    pub width: u32,
    /// Visible height in pixels.
    pub height: u32,
    /// Tightly packed I420 planes.
    pub yuv: I420Planes,
}

impl DecodedVideoFrame {
    /// Build a frame after validating that the planes match the geometry.
    pub fn new(
        meta: DecodeMeta,
        width: u32,
        height: u32,
        yuv: I420Planes,
    ) -> Result<Self, DecodeError> {
        if width == 0 || height == 0 {
            return Err(DecodeError::invalid_frame(format!(
                "frame dimensions must be non-zero, got {width}x{height}"
            )));
        }
        yuv.validate(width, height)?;
        Ok(Self {
            pts_micros: meta.pts_micros,
            sequence: meta.sequence,
            keyframe: meta.keyframe,
            width,
            height,
            yuv,
        })
    }

    /// The metadata carried by this frame.
    pub fn meta(&self) -> DecodeMeta {
        DecodeMeta::new(self.pts_micros, self.sequence, self.keyframe)
    }

    /// Codec and dimensions of this decoded frame.
    pub fn video_format(&self) -> VideoFormat {
        VideoFormat::new(Codec::H264, self.width, self.height)
    }

    /// Total bytes in the I420 representation.
    pub fn yuv_len(&self) -> usize {
        self.yuv.len()
    }

    /// Convert to tightly packed RGB24 (`width * height * 3` bytes).
    ///
    /// The matrix is the same convention the capture/encode path uses: BT.709
    /// for HD frames and BT.601 otherwise, with limited-range (16..=235)
    /// input. A decoder that parses the SPS VUI could do better, but this keeps
    /// screenshots and thumbnails deterministic without a colour pipeline.
    ///
    /// Returns an empty vector if the public plane fields no longer match the
    /// frame geometry (for example after a caller edits them directly); frames
    /// produced by [`DecodedVideoFrame::new`] are always well-formed.
    pub fn to_rgb(&self) -> Vec<u8> {
        if self.yuv.validate(self.width, self.height).is_err() {
            return Vec::new();
        }
        let width = usize::try_from(self.width).unwrap_or(0);
        let height = usize::try_from(self.height).unwrap_or(0);
        let mut rgb = Vec::with_capacity(width.saturating_mul(height).saturating_mul(3));
        let matrix = ColorMatrix::for_frame(self.width, self.height);
        let chroma_width = width.div_ceil(2);

        for y in 0..height {
            for x in 0..width {
                let luma = self.yuv.y[y * width + x];
                let chroma = (y / 2) * chroma_width + x / 2;
                let u = self.yuv.u[chroma];
                let v = self.yuv.v[chroma];
                let (r, g, b) = yuv_to_rgb(luma, u, v, matrix);
                rgb.push(r);
                rgb.push(g);
                rgb.push(b);
            }
        }
        rgb
    }

    /// Convert to tightly packed RGBA8 (`width * height * 4` bytes).
    ///
    /// Alpha is always `255`. This is the format most headless surface APIs
    /// and image crates expect.
    pub fn to_rgba(&self) -> Vec<u8> {
        let rgb = self.to_rgb();
        let mut rgba = Vec::with_capacity(rgb.len() / 3 * 4);
        for pixel in rgb.chunks_exact(3) {
            rgba.extend_from_slice(pixel);
            rgba.push(255);
        }
        rgba
    }

    /// Encode the frame as a binary PPM (`P6`) image.
    ///
    /// The result is a complete file: `P6\n<width> <height>\n255\n` followed by
    /// RGB24 pixels. PPM is used because it needs no image dependency and is
    /// readable by every common image viewer.
    pub fn to_ppm(&self) -> Vec<u8> {
        let rgb = self.to_rgb();
        let header = format!("P6\n{} {}\n255\n", self.width, self.height);
        let mut ppm = Vec::with_capacity(header.len() + rgb.len());
        ppm.extend_from_slice(header.as_bytes());
        ppm.extend_from_slice(&rgb);
        ppm
    }

    /// Write the frame to `path` as a binary PPM image.
    ///
    /// A caller that is rendering into a headless test can use this to keep the
    /// first decoded frame for inspection. The directory must already exist.
    pub fn write_ppm(&self, path: impl AsRef<Path>) -> std::io::Result<()> {
        std::fs::write(path, self.to_ppm())
    }
}

/// Limited-range luma/chroma matrix used for RGB conversion.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ColorMatrix {
    Bt601,
    Bt709,
}

impl ColorMatrix {
    fn for_frame(width: u32, height: u32) -> Self {
        if height >= 720 || width >= 1280 {
            Self::Bt709
        } else {
            Self::Bt601
        }
    }
}

fn yuv_to_rgb(y: u8, u: u8, v: u8, matrix: ColorMatrix) -> (u8, u8, u8) {
    let c = i32::from(y) - 16;
    let d = i32::from(u) - 128;
    let e = i32::from(v) - 128;
    let luma = 298 * c;

    let (r, g, b) = match matrix {
        // BT.601 limited range.
        ColorMatrix::Bt601 => (
            (luma + 409 * e + 128) >> 8,
            (luma - 100 * d - 208 * e + 128) >> 8,
            (luma + 516 * d + 128) >> 8,
        ),
        // BT.709 limited range.
        ColorMatrix::Bt709 => (
            (luma + 459 * e + 128) >> 8,
            (luma - 55 * d - 136 * e + 128) >> 8,
            (luma + 541 * d + 128) >> 8,
        ),
    };

    (clamp_u8(r), clamp_u8(g), clamp_u8(b))
}

fn clamp_u8(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(width: u32, height: u32) -> DecodedVideoFrame {
        let luma = usize::try_from(width * height).unwrap();
        let chroma = usize::try_from(width.div_ceil(2) * height.div_ceil(2)).unwrap();
        DecodedVideoFrame::new(
            DecodeMeta::new(7, 3, true),
            width,
            height,
            I420Planes::new(vec![16; luma], vec![128; chroma], vec![128; chroma]),
        )
        .unwrap()
    }

    #[test]
    fn metadata_is_preserved_in_the_constructor() {
        let frame = frame(4, 2);
        assert_eq!(frame.pts_micros, 7);
        assert_eq!(frame.sequence, 3);
        assert!(frame.keyframe);
        assert_eq!(frame.meta(), DecodeMeta::new(7, 3, true));
        assert_eq!(frame.video_format(), VideoFormat::new(Codec::H264, 4, 2));
    }

    #[test]
    fn plane_lengths_are_validated() {
        let error = DecodedVideoFrame::new(
            DecodeMeta::default(),
            4,
            4,
            I420Planes::new(vec![0; 15], vec![128; 4], vec![128; 4]),
        )
        .unwrap_err();
        assert_eq!(error.code(), crate::decode_codes::INVALID_FRAME);
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        let error = DecodedVideoFrame::new(
            DecodeMeta::default(),
            0,
            4,
            I420Planes::new(Vec::new(), Vec::new(), Vec::new()),
        )
        .unwrap_err();
        assert_eq!(error.code(), crate::decode_codes::INVALID_FRAME);
    }

    #[test]
    fn limited_range_black_and_white_convert_to_expected_rgb() {
        let mut black = frame(2, 2);
        assert_eq!(black.to_rgb(), vec![0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]);
        assert_eq!(black.to_rgba()[3], 255);

        let luma = 2 * 2;
        black.yuv.y = vec![235; luma];
        assert_eq!(black.to_rgb(), vec![255; 12]);
    }

    #[test]
    fn conversion_lengths_and_ppm_header_are_exact() {
        let frame = frame(6, 4);
        assert_eq!(frame.to_rgba().len(), 6 * 4 * 4);
        assert!(frame.to_rgba().chunks_exact(4).all(|pixel| pixel[3] == 255));

        let ppm = frame.to_ppm();
        let header = b"P6\n6 4\n255\n";
        assert!(ppm.starts_with(header));
        assert_eq!(ppm.len(), header.len() + 6 * 4 * 3);
    }

    #[test]
    fn conversion_of_invalid_planes_is_empty_not_a_panic() {
        let mut frame = frame(4, 4);
        frame.yuv.y.truncate(1);
        assert!(frame.to_rgb().is_empty());
        assert!(frame.to_rgba().is_empty());
    }

    #[test]
    fn odd_dimensions_use_ceil_chroma_planes() {
        let frame = DecodedVideoFrame::new(
            DecodeMeta::default(),
            3,
            3,
            I420Planes::new(vec![16; 9], vec![128; 4], vec![128; 4]),
        )
        .unwrap();
        assert_eq!(frame.to_rgba().len(), 3 * 3 * 4);
    }
}
