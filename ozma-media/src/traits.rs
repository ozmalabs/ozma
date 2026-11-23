// SPDX-License-Identifier: AGPL-3.0-only
//! The capture and encode boundaries of the media pipeline.
//!
//! Both traits are asynchronous and object-safe. A source owns its device and
//! yields frames until it reports [`MediaError::EndOfStream`]; an encoder
//! consumes one frame at a time and returns exactly one encoded access unit.

use async_trait::async_trait;

use crate::error::MediaError;
use crate::frame::{EncodedFrame, Frame, FrameFormat};
use ozma_protocol::MediaFormat;

/// An asynchronous source of captured frames.
///
/// Implementations must produce monotonically increasing `sequence` and PTS
/// values, non-empty payloads, and must make [`CaptureSource::stop`] prompt.
/// The pipeline treats `EndOfStream` as a clean shutdown and every other error
/// as a failure that stops the track.
#[async_trait]
pub trait CaptureSource: Send {
    /// Stable identifier, for example `/dev/video0` or `fake0`.
    fn id(&self) -> &str;

    /// Format of the frames this source will produce.
    fn format(&self) -> FrameFormat;

    /// Wait for the next frame.
    async fn next_frame(&mut self) -> Result<Frame, MediaError>;

    /// Stop the device and release resources. Must be idempotent.
    async fn stop(&mut self) -> Result<(), MediaError> {
        Ok(())
    }
}

/// An asynchronous video encoder.
///
/// The encoder declares the exact [`FrameFormat`] it accepts so the pipeline
/// can reject a mis-negotiated capture path before any frame is encoded.
#[async_trait]
pub trait Encoder: Send {
    /// Stable identifier, for example `vaapi-h264` or `fake-h264`.
    fn id(&self) -> &str;

    /// The raw frame format this encoder accepts.
    fn input_format(&self) -> FrameFormat;

    /// The encoded format this encoder produces.
    fn output_format(&self) -> MediaFormat;

    /// Encode one frame. Implementations should preserve PTS and sequence.
    ///
    /// Returning [`MediaError::FormatMismatch`] for an unexpected input format
    /// is mandatory; do not silently reinterpret bytes.
    async fn encode(&mut self, frame: Frame) -> Result<EncodedFrame, MediaError>;

    /// Ask the encoder to produce a keyframe as soon as possible.
    async fn request_keyframe(&mut self) -> Result<(), MediaError> {
        Ok(())
    }

    /// Drain any buffered frames at end of stream.
    async fn flush(&mut self) -> Result<Vec<EncodedFrame>, MediaError> {
        Ok(Vec::new())
    }
}
