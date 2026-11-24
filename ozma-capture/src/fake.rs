// SPDX-License-Identifier: AGPL-3.0-only
//! A registry-compatible fake capture provider.
//!
//! This is the deterministic source used by node, session, and API tests. It
//! wraps [`ozma_media::FakeCapture`] so tests exercise the same provider and
//! registry path as a real device, without touching V4L2.

use std::time::Duration;

use async_trait::async_trait;
use ozma_media::{CaptureSource, FakeCapture, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};

use crate::provider::CaptureProvider;

/// Payload pattern produced by the dev/test fake provider.
///
/// The default ramp is the historical deterministic pattern used by CI. The
/// blocks pattern is a moving 32x32 color mosaic for encode/rate-control
/// validation: a pure gradient lets a VBR encoder undershoot any requested
/// bitrate, while full per-pixel noise is so expensive that VAAPI hits its
/// minimum-QP floor and overshoots. A block mosaic sits in between, so a CBR
/// target can actually be measured.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum FakePattern {
    /// Repeating byte ramp; very compressible.
    #[default]
    Ramp,
    /// Deterministic 32x32 full-color blocks, reseeded each frame.
    Blocks,
}

/// A provider that exposes exactly one synthetic capture source.
#[derive(Clone, Debug)]
pub struct FakeCaptureProvider {
    capability: CaptureCapability,
    format: FrameFormat,
    frame_limit: Option<u64>,
    fps: u32,
    pattern: FakePattern,
}

impl Default for FakeCaptureProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeCaptureProvider {
    /// One 320x240 RGB24 source at 30 fps with unlimited frames.
    pub fn new() -> Self {
        Self {
            capability: CaptureCapability {
                id: "fake0".to_owned(),
                name: "Fake capture".to_owned(),
                kind: CaptureKind::Fake,
                formats: vec![PixelFormat::Rgb24.as_str().to_owned()],
                max_width: 320,
                max_height: 240,
                max_fps: 30,
                compressed: false,
                hdr: false,
                has_audio: false,
            },
            format: FrameFormat::rgb24(320, 240),
            frame_limit: None,
            fps: 30,
            pattern: FakePattern::Ramp,
        }
    }

    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.capability.id = id.into();
        self.capability.name = name.into();
        self
    }

    /// Set the synthetic frame format and matching capability geometry.
    pub fn with_format(mut self, format: FrameFormat) -> Self {
        self.format = format;
        self.capability.formats = vec![format.pixel_format.as_str().to_owned()];
        self.capability.max_width = format.width;
        self.capability.max_height = format.height;
        // A P010 source is 10-bit but this deterministic pattern is not
        // colorimetrically HDR; leave the HDR claim to an explicit opt-in.
        self.capability.hdr = false;
        self
    }

    /// Switch the synthetic source to 10-bit semi-planar P010.
    ///
    /// The block pattern is written in the real P010 layout (16-bit
    /// little-endian Y, then interleaved UV), so an HLS/encoder test can feed
    /// genuine 10-bit frames without a camera or an external converter.
    pub fn with_p010(mut self) -> Self {
        self.format.pixel_format = PixelFormat::P010;
        self.capability.formats = vec![PixelFormat::P010.as_str().to_owned()];
        self
    }

    /// Mark the advertised capability as HDR (dev/test only).
    pub fn with_hdr(mut self, hdr: bool) -> Self {
        self.capability.hdr = hdr;
        self
    }

    /// Limit the number of frames produced by opened sources.
    pub fn with_frame_limit(mut self, frames: u64) -> Self {
        self.frame_limit = Some(frames);
        self
    }

    /// Set the pacing rate of opened sources.
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps.max(1);
        self.capability.max_fps = self.fps;
        self
    }

    /// Select the deterministic payload pattern for opened sources.
    pub fn with_pattern(mut self, pattern: FakePattern) -> Self {
        self.pattern = pattern;
        self
    }

    /// Use a moving 32x32 block mosaic instead of the compressible ramp.
    ///
    /// Intended for encode/rate-control validation, not for CI snapshots.
    pub fn with_blocks(self) -> Self {
        self.with_pattern(FakePattern::Blocks)
    }

    /// The capability this provider advertises.
    pub fn capability(&self) -> &CaptureCapability {
        &self.capability
    }
}

impl CaptureProvider for FakeCaptureProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::Fake
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(vec![self.capability.clone()])
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::Fake || capability.id != self.capability.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("Fake capture source `{}` is not available.", capability.id),
            )
            .with_details(capability.id.clone()));
        }

        let mut source = FakeCapture::new(self.format)
            .with_id(self.capability.id.clone())
            .with_fps(self.fps);
        if let Some(limit) = self.frame_limit {
            source = source.with_frame_limit(limit);
        }
        match self.pattern {
            FakePattern::Ramp => Ok(Box::new(source)),
            FakePattern::Blocks => Ok(Box::new(
                BlocksCapture::new(self.format, self.fps, self.frame_limit)
                    .with_id(self.capability.id.clone()),
            )),
        }
    }
}

/// A deterministic 32x32 color-block source for encode validation.
///
/// Each block gets an independent per-frame RGB color, so the picture has
/// moderate spatial detail and no temporal correlation. That is enough to keep
/// a VBR encoder from coasting on a gradient, while staying below the
/// incompressible-noise regime where VAAPI's minimum QP floors the bitrate far
/// above the target. A full-HD ramp or a full-noise frame would each make a CBR
/// measurement meaningless in opposite directions.
///
/// It is deliberately not the default [`FakePattern`]; CI keeps the small,
/// compressible ramp. Pacing and PTS match [`ozma_media::FakeCapture`].
struct BlocksCapture {
    id: String,
    format: FrameFormat,
    frame_interval: Duration,
    frame_micros: u64,
    payload_len: usize,
    sequence: u64,
    remaining: Option<u64>,
    stopped: bool,
}

/// Width/height, in pixels, of one synthetic color block.
const BLOCK_PIXELS: usize = 32;

impl BlocksCapture {
    fn new(format: FrameFormat, fps: u32, frame_limit: Option<u64>) -> Self {
        let fps = fps.max(1);
        let payload_len = format
            .pixel_format
            .nominal_len(format.width, format.height)
            .unwrap_or_else(|| {
                ((u64::from(format.width) * u64::from(format.height)) / 64).max(64) as usize
            });
        Self {
            id: "fake-capture".to_owned(),
            format,
            frame_interval: Duration::from_secs_f64(1.0 / f64::from(fps)),
            frame_micros: 1_000_000 / u64::from(fps),
            payload_len,
            sequence: 0,
            remaining: frame_limit,
            stopped: false,
        }
    }

    fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    fn fill(&self, sequence: u64, data: &mut [u8]) {
        let height = self.format.height as usize;
        let width = self.format.width as usize;
        if height == 0 || width == 0 {
            return;
        }
        if self.format.pixel_format == PixelFormat::P010 {
            self.fill_p010(sequence, data, width, height);
            return;
        }
        let stride = data.len() / height;
        if stride == 0 {
            return;
        }
        // RGB24 has three bytes per pixel; BGRA four; NV12 one (a byte-block
        // pattern is meaningless for chroma planes, but this source is only
        // used with packed RGB in the dev path).
        let bytes_per_pixel = (stride / width).max(1);
        let block_bytes = (BLOCK_PIXELS * bytes_per_pixel).min(stride).max(1);
        let mut pattern = vec![0u8; block_bytes];

        for (row_index, row) in data.chunks_mut(stride).enumerate() {
            let block_y = row_index / BLOCK_PIXELS;
            for (block_x, chunk) in row.chunks_mut(block_bytes).enumerate() {
                let color = block_color(sequence, block_y, block_x);
                for (index, byte) in pattern.iter_mut().enumerate() {
                    *byte = color[index % color.len()];
                }
                let len = chunk.len();
                chunk.copy_from_slice(&pattern[..len]);
            }
        }
    }

    /// Fill a real P010 (10-bit 4:2:0 semi-planar) payload.
    ///
    /// Luma is a 32x32 block mosaic in the 10-bit limited range, and chroma is
    /// a coarser 16x16 block pattern around neutral 512. Both planes use
    /// 16-bit little-endian samples, matching ffmpeg `p010le`.
    fn fill_p010(&self, sequence: u64, data: &mut [u8], width: usize, height: usize) {
        let y_samples = width.saturating_mul(height);
        let y_bytes = y_samples.saturating_mul(2).min(data.len());
        for y in 0..height {
            for x in 0..width {
                let offset = (y * width + x) * 2;
                if offset + 2 > y_bytes {
                    break;
                }
                let color = block_color(sequence, y / BLOCK_PIXELS, x / BLOCK_PIXELS);
                // Limited-range 10-bit luma: 64..940.
                let luma = 64 + (u32::from(color[0]) * 876 / 255) as u16;
                data[offset..offset + 2].copy_from_slice(&luma.to_le_bytes());
            }
        }

        let chroma_width = width / 2;
        let chroma_height = height / 2;
        let mut offset = y_bytes;
        for y in 0..chroma_height {
            for x in 0..chroma_width {
                let color = block_color(sequence ^ 0xA5A5_A5A5_A5A5_A5A5, y / 16, x / 16);
                // Limited-range 10-bit chroma around the 512 neutral point.
                let u = 512 + (i32::from(color[1]) - 128) * 3 / 4;
                let v = 512 + (i32::from(color[2]) - 128) * 3 / 4;
                for sample in [u.clamp(64, 960) as u16, v.clamp(64, 960) as u16] {
                    if offset + 2 > data.len() {
                        return;
                    }
                    data[offset..offset + 2].copy_from_slice(&sample.to_le_bytes());
                    offset += 2;
                }
            }
        }
    }
}

/// splitmix64-derived color for one block of one frame.
fn block_color(sequence: u64, block_y: usize, block_x: usize) -> [u8; 3] {
    let mut value = 0x9E37_79B9_7F4A_7C15u64
        ^ sequence.wrapping_mul(0xBF58_476D_1CE4_E5B9)
        ^ (block_y as u64).wrapping_mul(0x94D0_49BB_1331_11EB)
        ^ (block_x as u64).wrapping_mul(0xD6E8_FEB8_6659_FD93);
    value = (value ^ (value >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    value ^= value >> 31;
    [value as u8, (value >> 8) as u8, (value >> 16) as u8]
}

#[async_trait]
impl CaptureSource for BlocksCapture {
    fn id(&self) -> &str {
        &self.id
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        if self.stopped {
            return Err(MediaError::Closed {
                id: self.id.clone(),
            });
        }
        if let Some(remaining) = self.remaining {
            if remaining == 0 {
                return Err(MediaError::EndOfStream);
            }
        }

        if self.sequence > 0 && !self.frame_interval.is_zero() {
            tokio::time::sleep(self.frame_interval).await;
        }
        if self.stopped {
            return Err(MediaError::Closed {
                id: self.id.clone(),
            });
        }

        let sequence = self.sequence;
        let mut data = vec![0u8; self.payload_len];
        self.fill(sequence, &mut data);
        let frame = Frame::with_data(
            self.format,
            sequence.saturating_mul(self.frame_micros),
            sequence,
            true,
            data,
        )?;

        self.sequence = self.sequence.saturating_add(1);
        if let Some(remaining) = &mut self.remaining {
            *remaining = remaining.saturating_sub(1);
        }
        Ok(frame)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        self.stopped = true;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn blocks_pattern_is_deterministic_and_frame_dependent() {
        let provider = FakeCaptureProvider::new()
            .with_format(FrameFormat::rgb24(64, 32))
            .with_blocks()
            .with_fps(1_000)
            .with_frame_limit(3);

        let mut first = provider
            .open(provider.capability())
            .expect("blocks source opens");
        let mut second = provider
            .open(provider.capability())
            .expect("blocks source opens");

        let a = first.next_frame().await.unwrap();
        let b = first.next_frame().await.unwrap();
        let c = second.next_frame().await.unwrap();
        let d = second.next_frame().await.unwrap();

        assert_eq!(a.data(), c.data(), "same sequence is reproducible");
        assert_eq!(b.data(), d.data(), "same sequence is reproducible");
        assert_ne!(a.data(), b.data(), "frames must not repeat the payload");
        // A ramp would have runs of consecutive byte values; a mosaic should
        // not. Check the first 32-pixel block is flat, which is the property
        // that keeps the encoder out of the full-noise regime.
        assert!(
            a.data()[..90]
                .chunks_exact(3)
                .all(|pixel| pixel == &a.data()[..3]),
            "the first 32x32 block must be one uniform color"
        );
        assert!(
            a.data()
                .windows(8)
                .filter(|w| w.windows(2).all(|p| p[1] == p[0].wrapping_add(1)))
                .count()
                < a.data().len() / 8,
            "mosaic payload must not be a compressible ramp"
        );
    }

    #[tokio::test]
    async fn p010_pattern_is_real_ten_bit_layout_and_frame_dependent() {
        let provider = FakeCaptureProvider::new()
            .with_format(FrameFormat::rgb24(64, 32))
            .with_p010()
            .with_hdr(true)
            .with_blocks()
            .with_fps(1_000)
            .with_frame_limit(2);

        assert_eq!(provider.capability().formats, vec!["p010"]);
        assert!(provider.capability().hdr);
        let mut source = provider
            .open(provider.capability())
            .expect("p010 source opens");
        assert_eq!(source.format().pixel_format, PixelFormat::P010);
        let first = source.next_frame().await.unwrap();
        let second = source.next_frame().await.unwrap();

        assert_eq!(
            first.data().len(),
            PixelFormat::P010.nominal_len(64, 32).unwrap()
        );
        assert_ne!(first.data(), second.data());
        // Every Y sample is a 10-bit little-endian value in limited range.
        for sample in first.data()[..64 * 32 * 2].chunks_exact(2) {
            let value = u16::from_le_bytes([sample[0], sample[1]]);
            assert!(
                (64..=960).contains(&value),
                "unexpected 10-bit luma {value}"
            );
        }
        // The first 32x32 luma block is uniform (the block mosaic property).
        let first_luma = &first.data()[..2 * 32];
        assert!(first_luma
            .chunks_exact(2)
            .all(|sample| sample == &first_luma[..2]));
    }
}
