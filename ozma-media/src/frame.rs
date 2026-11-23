// SPDX-License-Identifier: AGPL-3.0-only
//! Frame and buffer types shared by capture, encoding, and tests.

use std::fmt;

use bytes::Bytes;
use ozma_protocol::{Codec, MediaFormat};
use serde::{Deserialize, Serialize};

use crate::error::MediaError;

/// Layout of the bytes inside a capture or encoded frame.
///
/// Raw layouts are what a capture device produces; compressed variants are
/// pass-through streams (for example MJPEG from a capture card). Protocol
/// codecs live in [`Codec`]; this enum only describes bytes on the wire in the
/// local pipeline.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PixelFormat {
    /// Packed 8-bit RGB, three bytes per pixel.
    Rgb24,
    /// Packed 8-bit BGRA, four bytes per pixel.
    Bgra,
    /// YUV 4:2:0 semi-planar, as used by most capture devices.
    Nv12,
    /// YUV 4:2:2 packed, two bytes per pixel.
    Yuyv,
    /// 10-bit 4:2:0 semi-planar (HDR capture).
    P010,
    /// JPEG-compressed frames from a capture device.
    Mjpeg,
    /// H.264 Annex-B or length-prefixed access units.
    H264,
    /// HEVC access units.
    Hevc,
    /// AV1 temporal units.
    Av1,
}

impl PixelFormat {
    /// True for formats that are already compressed bitstreams.
    pub fn is_compressed(self) -> bool {
        matches!(self, Self::Mjpeg | Self::H264 | Self::Hevc | Self::Av1)
    }

    /// The protocol codec this layout corresponds to, if any.
    pub fn codec(self) -> Option<Codec> {
        match self {
            Self::Mjpeg => Some(Codec::Mjpeg),
            Self::H264 => Some(Codec::H264),
            Self::Hevc => Some(Codec::Hevc),
            Self::Av1 => Some(Codec::Av1),
            Self::Rgb24 | Self::Bgra | Self::Nv12 | Self::Yuyv | Self::P010 => None,
        }
    }

    /// Nominal uncompressed byte length for a frame of this layout.
    ///
    /// Returns `None` for compressed formats, whose size is data-dependent.
    pub fn nominal_len(self, width: u32, height: u32) -> Option<usize> {
        let pixels = u64::from(width).checked_mul(u64::from(height))?;
        let bytes = match self {
            Self::Rgb24 => pixels.checked_mul(3)?,
            Self::Bgra => pixels.checked_mul(4)?,
            Self::Yuyv => pixels.checked_mul(2)?,
            Self::Nv12 => pixels.checked_mul(3)?.div_ceil(2),
            Self::P010 => pixels.checked_mul(3)?,
            Self::Mjpeg | Self::H264 | Self::Hevc | Self::Av1 => return None,
        };
        usize::try_from(bytes).ok()
    }

    /// Stable lowercase name, suitable for logs and capability strings.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Rgb24 => "rgb24",
            Self::Bgra => "bgra",
            Self::Nv12 => "nv12",
            Self::Yuyv => "yuyv",
            Self::P010 => "p010",
            Self::Mjpeg => "mjpeg",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Av1 => "av1",
        }
    }
}

impl fmt::Display for PixelFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Geometry and byte layout of frames a source produces.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct FrameFormat {
    /// Byte layout.
    pub pixel_format: PixelFormat,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
}

impl FrameFormat {
    /// Construct a format description. Dimensions are validated when a
    /// [`Frame`] is created, because discovery-only sources may not know them
    /// yet.
    pub const fn new(pixel_format: PixelFormat, width: u32, height: u32) -> Self {
        Self {
            pixel_format,
            width,
            height,
        }
    }

    /// Convenience constructor for packed RGB frames.
    pub const fn rgb24(width: u32, height: u32) -> Self {
        Self::new(PixelFormat::Rgb24, width, height)
    }

    /// True when two formats describe the same geometry and layout.
    pub fn is_compatible(&self, other: &Self) -> bool {
        self.pixel_format == other.pixel_format
            && self.width == other.width
            && self.height == other.height
    }

    /// True when the source can feed an encoder that declares `input`.
    ///
    /// Format negotiation is exact today; keeping this behind a method lets
    /// later capture adapters convert compatible layouts without changing the
    /// pipeline contract.
    pub fn feeds(&self, input: &Self) -> bool {
        self.is_compatible(input)
    }
}

impl fmt::Display for FrameFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "{} {}x{}",
            self.pixel_format, self.width, self.height
        )
    }
}

/// A non-empty owned frame payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FrameBuffer(Bytes);

impl FrameBuffer {
    /// Wrap owned bytes, rejecting an empty buffer.
    pub fn new(data: impl Into<Bytes>) -> Result<Self, MediaError> {
        let data = data.into();
        if data.is_empty() {
            return Err(MediaError::EmptyFrame);
        }
        Ok(Self(data))
    }

    /// Wrap a static slice without copying.
    pub fn from_static(data: &'static [u8]) -> Result<Self, MediaError> {
        Self::new(Bytes::from_static(data))
    }

    /// Payload length in bytes. Always greater than zero.
    pub fn len(&self) -> usize {
        self.0.len()
    }

    /// Always `false`; present to satisfy conventional buffer APIs.
    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    /// Borrow the payload.
    pub fn as_slice(&self) -> &[u8] {
        &self.0
    }

    /// Borrow the underlying `Bytes` for cheap clones.
    pub fn as_bytes(&self) -> &Bytes {
        &self.0
    }

    /// Consume the buffer and return the underlying bytes.
    pub fn into_bytes(self) -> Bytes {
        self.0
    }
}

impl AsRef<[u8]> for FrameBuffer {
    fn as_ref(&self) -> &[u8] {
        self.as_slice()
    }
}

impl From<FrameBuffer> for Bytes {
    fn from(buffer: FrameBuffer) -> Self {
        buffer.into_bytes()
    }
}

/// A captured frame with the metadata the pipeline and packetiser need.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    format: FrameFormat,
    pts_micros: u64,
    sequence: u64,
    keyframe: bool,
    buffer: FrameBuffer,
}

impl Frame {
    /// Build a frame from an already validated buffer.
    pub fn new(
        format: FrameFormat,
        pts_micros: u64,
        sequence: u64,
        keyframe: bool,
        buffer: FrameBuffer,
    ) -> Result<Self, MediaError> {
        if format.width == 0 || format.height == 0 {
            return Err(MediaError::InvalidFormat(format!(
                "frame dimensions must be non-zero, got {}x{}",
                format.width, format.height
            )));
        }
        Ok(Self {
            format,
            pts_micros,
            sequence,
            keyframe,
            buffer,
        })
    }

    /// Build a frame directly from bytes, rejecting an empty payload.
    pub fn with_data(
        format: FrameFormat,
        pts_micros: u64,
        sequence: u64,
        keyframe: bool,
        data: impl Into<Bytes>,
    ) -> Result<Self, MediaError> {
        Self::new(
            format,
            pts_micros,
            sequence,
            keyframe,
            FrameBuffer::new(data)?,
        )
    }

    /// Byte layout and geometry.
    pub fn format(&self) -> FrameFormat {
        self.format
    }

    /// Presentation timestamp in microseconds on the capture clock.
    pub fn pts_micros(&self) -> u64 {
        self.pts_micros
    }

    /// Monotonic frame sequence number assigned by the source.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Whether this frame starts a new decodable group.
    pub fn is_keyframe(&self) -> bool {
        self.keyframe
    }

    /// Borrow the payload.
    pub fn data(&self) -> &[u8] {
        self.buffer.as_slice()
    }

    /// Borrow the payload buffer.
    pub fn buffer(&self) -> &FrameBuffer {
        &self.buffer
    }

    /// Consume the frame and return its payload.
    pub fn into_data(self) -> Bytes {
        self.buffer.into_bytes()
    }

    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        self.buffer.len()
    }

    /// Always `false`; frames are non-empty by construction.
    pub fn is_empty(&self) -> bool {
        false
    }
}

/// An encoded frame produced by an [`crate::Encoder`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EncodedFrame {
    format: MediaFormat,
    pts_micros: u64,
    sequence: u64,
    keyframe: bool,
    data: Bytes,
}

impl EncodedFrame {
    /// Build an encoded frame, rejecting empty payloads and video formats with
    /// zero dimensions.
    pub fn new(
        format: MediaFormat,
        pts_micros: u64,
        sequence: u64,
        keyframe: bool,
        data: impl Into<Bytes>,
    ) -> Result<Self, MediaError> {
        if format.codec.is_video() && (format.width == 0 || format.height == 0) {
            return Err(MediaError::InvalidFormat(format!(
                "video format must have non-zero dimensions, got {}x{}",
                format.width, format.height
            )));
        }
        let data = data.into();
        if data.is_empty() {
            return Err(MediaError::EmptyFrame);
        }
        Ok(Self {
            format,
            pts_micros,
            sequence,
            keyframe,
            data,
        })
    }

    /// Codec, geometry, and rate-control description of the output.
    pub fn format(&self) -> &MediaFormat {
        &self.format
    }

    /// Presentation timestamp inherited from the captured frame.
    pub fn pts_micros(&self) -> u64 {
        self.pts_micros
    }

    /// Sequence number inherited from the captured frame.
    pub fn sequence(&self) -> u64 {
        self.sequence
    }

    /// Whether this access unit is independently decodable.
    pub fn is_keyframe(&self) -> bool {
        self.keyframe
    }

    /// Borrow the encoded payload.
    pub fn data(&self) -> &[u8] {
        &self.data
    }

    /// Consume the frame and return its payload.
    pub fn into_data(self) -> Bytes {
        self.data
    }

    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Always `false`; encoded frames are non-empty by construction.
    pub fn is_empty(&self) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_buffers_are_rejected() {
        assert_eq!(
            FrameBuffer::new(Vec::<u8>::new()).unwrap_err(),
            MediaError::EmptyFrame
        );
    }

    #[test]
    fn frame_metadata_roundtrips() {
        let format = FrameFormat::rgb24(16, 8);
        let frame = Frame::with_data(format, 33, 7, true, vec![1, 2, 3]).unwrap();
        assert_eq!(frame.format(), format);
        assert_eq!(frame.pts_micros(), 33);
        assert_eq!(frame.sequence(), 7);
        assert!(frame.is_keyframe());
        assert!(!frame.is_empty());
        assert_eq!(frame.data(), &[1, 2, 3]);
    }

    #[test]
    fn zero_dimensions_are_rejected() {
        let error = Frame::with_data(FrameFormat::rgb24(0, 8), 0, 0, false, vec![1]).unwrap_err();
        assert!(matches!(error, MediaError::InvalidFormat(_)));
    }

    #[test]
    fn nominal_sizes_are_sane() {
        assert_eq!(PixelFormat::Rgb24.nominal_len(16, 8), Some(384));
        assert_eq!(PixelFormat::Nv12.nominal_len(16, 8), Some(192));
        assert_eq!(PixelFormat::H264.nominal_len(16, 8), None);
    }

    #[test]
    fn format_compatibility_is_exact_today() {
        let a = FrameFormat::rgb24(320, 240);
        assert!(a.feeds(&FrameFormat::rgb24(320, 240)));
        assert!(!a.feeds(&FrameFormat::rgb24(320, 241)));
        assert!(!a.feeds(&FrameFormat::new(PixelFormat::Nv12, 320, 240)));
    }

    #[test]
    fn encoded_frames_must_be_non_empty() {
        let format = MediaFormat::new(Codec::H264, 320, 240, 30, 2_000);
        let error = EncodedFrame::new(format, 0, 0, true, Vec::<u8>::new()).unwrap_err();
        assert_eq!(error, MediaError::EmptyFrame);
    }
}
