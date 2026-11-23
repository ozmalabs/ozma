// SPDX-License-Identifier: AGPL-3.0-only
//! Deterministic fake source and encoder for CI.
//!
//! Frames carry a sequence-derived PTS, so a test can assert exact timestamps
//! without depending on wall-clock time. Payloads are non-empty and repeatable.

use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use ozma_protocol::{Codec, MediaFormat};

use crate::error::MediaError;
use crate::frame::{EncodedFrame, Frame, FrameFormat};
use crate::traits::{CaptureSource, Encoder};

/// A deterministic capture source.
///
/// By default it produces unlimited 320x240 RGB24 frames at 30 fps. Limit the
/// frame count with [`FakeCapture::with_frame_limit`] to make a pipeline end
/// on its own, and raise the rate with [`FakeCapture::with_fps`] in tests that
/// do not need real-time pacing.
#[derive(Debug)]
pub struct FakeCapture {
    id: String,
    format: FrameFormat,
    frame_interval: Duration,
    frame_micros: u64,
    payload_len: usize,
    sequence: u64,
    remaining: Option<u64>,
    stopped: bool,
}

impl Default for FakeCapture {
    fn default() -> Self {
        Self::new(FrameFormat::rgb24(320, 240))
    }
}

impl FakeCapture {
    /// Create a source for the given format.
    pub fn new(format: FrameFormat) -> Self {
        let payload_len = format
            .pixel_format
            .nominal_len(format.width, format.height)
            .unwrap_or_else(|| {
                // Compressed fakes still need a plausible, non-empty payload.
                ((u64::from(format.width) * u64::from(format.height)) / 64).max(64) as usize
            });
        Self {
            id: "fake-capture".to_owned(),
            format,
            frame_interval: Duration::from_millis(33),
            frame_micros: 33_333,
            payload_len,
            sequence: 0,
            remaining: None,
            stopped: false,
        }
    }

    /// Set the identifier reported by [`CaptureSource::id`].
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Set the pacing rate. Values below one are treated as one.
    pub fn with_fps(mut self, fps: u32) -> Self {
        let fps = fps.max(1);
        self.frame_interval = Duration::from_secs_f64(1.0 / f64::from(fps));
        self.frame_micros = 1_000_000 / u64::from(fps);
        self
    }

    /// Stop after `frames` frames. `0` ends the stream immediately.
    pub fn with_frame_limit(mut self, frames: u64) -> Self {
        self.remaining = Some(frames);
        self
    }

    /// Override the payload length in bytes. Zero is treated as one.
    pub fn with_payload_len(mut self, len: usize) -> Self {
        self.payload_len = len.max(1);
        self
    }

    /// Frames left before end-of-stream, if limited.
    pub fn remaining(&self) -> Option<u64> {
        self.remaining
    }

    /// Number of frames produced so far.
    pub fn produced(&self) -> u64 {
        self.sequence
    }

    fn payload(&self, sequence: u64) -> Vec<u8> {
        let mut data = vec![0u8; self.payload_len];
        let seq = sequence.to_le_bytes();
        let prefix = data.len().min(seq.len());
        data[..prefix].copy_from_slice(&seq[..prefix]);
        for (index, byte) in data.iter_mut().enumerate().skip(prefix) {
            *byte = ((index as u64).wrapping_add(sequence) & 0xFF) as u8;
        }
        data
    }
}

#[async_trait]
impl CaptureSource for FakeCapture {
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
        let pts_micros = sequence.saturating_mul(self.frame_micros);
        let frame = Frame::with_data(
            self.format,
            pts_micros,
            sequence,
            true,
            self.payload(sequence),
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

/// A deterministic "encoder" that frames the input instead of compressing it.
///
/// The output is not a valid bitstream. It is a stable, non-empty payload with
/// an Annex-B-like prefix, which is enough to exercise queueing, keyframe
/// cadence, and metrics without a codec dependency.
#[derive(Debug)]
pub struct FakeEncoder {
    id: String,
    input: FrameFormat,
    output: MediaFormat,
    keyframe_interval: u64,
    encoded: u64,
}

impl FakeEncoder {
    /// Create an encoder from a raw input format and an encoded output format.
    pub fn new(input: FrameFormat, output: MediaFormat) -> Self {
        Self {
            id: "fake-encoder".to_owned(),
            input,
            output,
            keyframe_interval: 30,
            encoded: 0,
        }
    }

    /// A fake H.264 encoder for a raw input format.
    pub fn h264(input: FrameFormat, bitrate_kbps: u32) -> Self {
        let output = MediaFormat::new(Codec::H264, input.width, input.height, 30, bitrate_kbps);
        Self::new(input, output)
    }

    /// Set the identifier reported by [`Encoder::id`].
    pub fn with_id(mut self, id: impl Into<String>) -> Self {
        self.id = id.into();
        self
    }

    /// Emit a keyframe every `frames` frames; the first frame is always a
    /// keyframe. `0` disables the periodic cadence.
    pub fn with_keyframe_interval(mut self, frames: u64) -> Self {
        self.keyframe_interval = frames;
        self
    }

    /// Number of frames encoded so far.
    pub fn encoded_count(&self) -> u64 {
        self.encoded
    }
}

#[async_trait]
impl Encoder for FakeEncoder {
    fn id(&self) -> &str {
        &self.id
    }

    fn input_format(&self) -> FrameFormat {
        self.input
    }

    fn output_format(&self) -> MediaFormat {
        self.output.clone()
    }

    async fn encode(&mut self, frame: Frame) -> Result<EncodedFrame, MediaError> {
        if !frame.format().feeds(&self.input) {
            return Err(MediaError::FormatMismatch {
                expected: self.input.to_string(),
                actual: frame.format().to_string(),
            });
        }

        let keyframe = self.encoded == 0
            || (self.keyframe_interval > 0 && self.encoded % self.keyframe_interval == 0);
        let source = frame.data();
        let mut data = Vec::with_capacity(16 + source.len().min(64));
        data.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        data.extend_from_slice(if keyframe { b"KEY" } else { b"FRM" });
        data.extend_from_slice(&frame.sequence().to_le_bytes());
        data.extend_from_slice(&source[..source.len().min(64)]);

        let encoded = EncodedFrame::new(
            self.output.clone(),
            frame.pts_micros(),
            frame.sequence(),
            keyframe,
            Bytes::from(data),
        )?;
        self.encoded = self.encoded.saturating_add(1);
        Ok(encoded)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::PixelFormat;

    #[tokio::test]
    async fn fake_frames_are_non_empty_and_monotonic() {
        let mut source = FakeCapture::default().with_fps(1_000).with_frame_limit(5);
        let mut previous = None;
        for expected_sequence in 0..5 {
            let frame = source.next_frame().await.unwrap();
            assert!(!frame.is_empty());
            assert_eq!(frame.sequence(), expected_sequence);
            if let Some(previous) = previous {
                assert!(frame.pts_micros() > previous);
            }
            previous = Some(frame.pts_micros());
        }
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
    }

    #[tokio::test]
    async fn fake_encoder_rejects_wrong_input() {
        let mut encoder = FakeEncoder::h264(FrameFormat::rgb24(16, 16), 1_000);
        let frame = Frame::with_data(
            FrameFormat::new(PixelFormat::Nv12, 16, 16),
            0,
            0,
            true,
            vec![0; 32],
        )
        .unwrap();
        let error = encoder.encode(frame).await.unwrap_err();
        assert!(matches!(error, MediaError::FormatMismatch { .. }));
    }

    #[tokio::test]
    async fn fake_encoder_emits_keyframes_on_cadence() {
        let mut encoder =
            FakeEncoder::h264(FrameFormat::rgb24(16, 16), 1_000).with_keyframe_interval(2);
        for sequence in 0u64..4 {
            let frame = Frame::with_data(
                FrameFormat::rgb24(16, 16),
                u64::from(sequence),
                u64::from(sequence),
                true,
                vec![1, 2, 3],
            )
            .unwrap();
            let encoded = encoder.encode(frame).await.unwrap();
            assert_eq!(
                encoded.is_keyframe(),
                sequence % 2 == 0,
                "sequence {sequence}"
            );
        }
    }
}
