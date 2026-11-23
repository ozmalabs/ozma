// SPDX-License-Identifier: AGPL-3.0-only
//! Encoded video sources consumed by an RTSP `PLAY` request.
//!
//! The RTSP server deliberately depends on a tiny trait rather than on
//! `ozma-encode` directly. A session can be fed by an encoder pipeline, a
//! channel bridge, a test fixture, or an HLS packager that taps the same frames.

use std::collections::VecDeque;
use std::time::Duration;

use async_trait::async_trait;
use ozma_media::EncodedFrame;
use tokio::sync::mpsc;

/// A pull-based stream of encoded access units.
///
/// Returning `None` ends playback for the session; the RTSP connection stays
/// open and accepts `TEARDOWN`, `PAUSE`, or a fresh `PLAY`.
#[async_trait]
pub trait EncodedVideoSource: Send {
    /// Wait for the next encoded frame.
    async fn next_frame(&mut self) -> Option<EncodedFrame>;
}

/// A source backed by a Tokio channel.
///
/// This is the intended bridge from `ozma-encode` or a pipeline worker. A
/// bounded channel provides backpressure and makes it obvious when a slow
/// client is falling behind.
pub struct ChannelVideoSource {
    receiver: mpsc::Receiver<EncodedFrame>,
}

impl ChannelVideoSource {
    /// Wrap an existing receiver.
    pub fn new(receiver: mpsc::Receiver<EncodedFrame>) -> Self {
        Self { receiver }
    }

    /// Create a channel and its source.
    pub fn channel(capacity: usize) -> (mpsc::Sender<EncodedFrame>, Self) {
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        (sender, Self::new(receiver))
    }
}

#[async_trait]
impl EncodedVideoSource for ChannelVideoSource {
    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        self.receiver.recv().await
    }
}

/// A finite in-memory source, useful for tests, demos, and still-frame loops.
pub struct VecVideoSource {
    frames: VecDeque<EncodedFrame>,
    delay: Option<Duration>,
}

impl VecVideoSource {
    /// Play the frames as fast as the transport accepts them.
    pub fn new(frames: Vec<EncodedFrame>) -> Self {
        Self {
            frames: frames.into(),
            delay: None,
        }
    }

    /// Play frames at a fixed inter-frame delay.
    pub fn with_delay(frames: Vec<EncodedFrame>, delay: Duration) -> Self {
        Self {
            frames: frames.into(),
            delay: Some(delay),
        }
    }
}

#[async_trait]
impl EncodedVideoSource for VecVideoSource {
    async fn next_frame(&mut self) -> Option<EncodedFrame> {
        if let Some(delay) = self.delay {
            tokio::time::sleep(delay).await;
        }
        self.frames.pop_front()
    }
}
