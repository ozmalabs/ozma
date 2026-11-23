// SPDX-License-Identifier: AGPL-3.0-only
//! Native PipeWire capture for the audio pipeline.

use async_trait::async_trait;
use ozma_pipewire::{CaptureConfig, PipeWireAudioSource, PipeWireError};

use crate::error::AudioError;
use crate::frame::{AudioFormat, AudioFrame};
use crate::traits::AudioSource;

fn map_error(error: PipeWireError) -> AudioError {
    AudioError::Unavailable {
        device: "pipewire".to_owned(),
        message: error.to_string(),
        remediation: error.remediation().to_owned(),
    }
}

/// A PipeWire capture source that exposes interleaved S16 frames.
pub struct PipeWireCapture {
    inner: PipeWireAudioSource,
}

impl PipeWireCapture {
    /// Open the default PipeWire source, or a configured target.
    pub async fn open(config: CaptureConfig) -> Result<Self, AudioError> {
        let inner = PipeWireAudioSource::new(config).await.map_err(map_error)?;
        Ok(Self { inner })
    }

    /// Open the default source.
    pub async fn default_source() -> Result<Self, AudioError> {
        Self::open(CaptureConfig::default()).await
    }

    /// The format negotiated with the PipeWire graph.
    pub fn negotiated_format(&self) -> ozma_pipewire::NegotiatedFormat {
        self.inner.negotiated_format()
    }
}

#[async_trait]
impl AudioSource for PipeWireCapture {
    fn id(&self) -> &str {
        "pipewire"
    }

    fn format(&self) -> AudioFormat {
        let format = self.inner.negotiated_format();
        AudioFormat {
            codec: ozma_protocol::Codec::Opus,
            sample_rate: format.sample_rate,
            channels: format.channels as u8,
            bitrate_kbps: 128,
        }
    }

    async fn next_frame(&mut self) -> Result<AudioFrame, AudioError> {
        let frame = self.inner.next_frame().await.map_err(map_error)?;
        AudioFrame::new(
            frame.pts_micros,
            frame.sequence,
            frame.sample_rate,
            frame.channels as u8,
            frame.samples,
        )
    }

    async fn stop(&mut self) -> Result<(), AudioError> {
        self.inner.stop();
        Ok(())
    }
}
