// SPDX-License-Identifier: AGPL-3.0-only
//! The client-side video decoder boundary.
//!
//! A decoder consumes one complete Annex-B access unit at a time and returns at
//! most one decoded picture. The trait is asynchronous so a future hardware or
//! platform decoder can hand work to another thread without changing callers;
//! the OpenH264 implementation in [`crate::H264Decoder`] is synchronous inside.

use async_trait::async_trait;
use ozma_protocol::Codec;

use crate::error::DecodeError;
use crate::frame::{DecodeMeta, DecodedVideoFrame};
use crate::stats::DecodeStats;

/// Codec and current dimensions of a decoder's output.
///
/// Dimensions are `0` until the first frame is decoded; use
/// [`VideoFormat::dimensions`] when "unknown" must be distinguishable from a
/// zero-sized frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFormat {
    /// Codec the decoder produces.
    pub codec: Codec,
    /// Current output width, or `0` before the first frame.
    pub width: u32,
    /// Current output height, or `0` before the first frame.
    pub height: u32,
}

impl VideoFormat {
    /// Construct a format description.
    pub const fn new(codec: Codec, width: u32, height: u32) -> Self {
        Self {
            codec,
            width,
            height,
        }
    }

    /// Dimensions once they are known.
    pub const fn dimensions(&self) -> Option<(u32, u32)> {
        if self.width == 0 || self.height == 0 {
            None
        } else {
            Some((self.width, self.height))
        }
    }
}

impl Default for VideoFormat {
    fn default() -> Self {
        Self::new(Codec::H264, 0, 0)
    }
}

/// A stateful video decoder.
///
/// # Contract
///
/// * [`decode`](VideoDecoder::decode) receives one complete Annex-B access
///   unit. It returns `Ok(None)` when the unit carried no picture (for example
///   SPS/PPS-only headers) or when the backend is holding the picture for
///   reordering; the next call or [`flush`](VideoDecoder::flush) may return it.
/// * A returned frame preserves the `pts_micros`, `sequence`, and `keyframe`
///   values from the access unit that produced it. When the backend reorders,
///   the decoder attributes output to the oldest outstanding unit.
/// * Empty and structurally invalid units are rejected with structured
///   [`DecodeError`] values; the decoder stays usable for the next keyframe.
/// * [`stop`](VideoDecoder::stop) is idempotent. After it, further calls return
///   [`DecodeError::Closed`] until a new decoder is created.
#[async_trait]
pub trait VideoDecoder: Send {
    /// Stable identifier, for example `openh264-h264`.
    fn id(&self) -> &str;

    /// Codec and current dimensions.
    fn format(&self) -> VideoFormat;

    /// Current dimensions once a frame has been decoded.
    fn dimensions(&self) -> Option<(u32, u32)> {
        self.format().dimensions()
    }

    /// Counters for decoded frames, drops, and errors.
    fn stats(&self) -> DecodeStats;

    /// Decode one complete Annex-B access unit.
    async fn decode(
        &mut self,
        access_unit: &[u8],
        meta: DecodeMeta,
    ) -> Result<Option<DecodedVideoFrame>, DecodeError>;

    /// Drain any pictures the backend is still holding.
    ///
    /// The low-latency baseline stream produced by `ozma-encode` emits one
    /// picture per access unit, so this normally returns an empty vector. A
    /// stream with reordering can return delayed pictures here; their metadata
    /// is taken from the oldest outstanding access units, in order.
    async fn flush(&mut self) -> Result<Vec<DecodedVideoFrame>, DecodeError>;

    /// Stop the decoder and release native resources. Must be idempotent.
    async fn stop(&mut self) -> Result<(), DecodeError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unknown_dimensions_are_not_reported_as_zero_sized() {
        let format = VideoFormat::new(Codec::H264, 0, 0);
        assert_eq!(format.dimensions(), None);
        assert_eq!(
            VideoFormat::new(Codec::H264, 1280, 720).dimensions(),
            Some((1280, 720))
        );
    }
}
