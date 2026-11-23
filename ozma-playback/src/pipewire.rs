// SPDX-License-Identifier: AGPL-3.0-only
//! Native PipeWire playback sink for Linux clients.

use ozma_decode::DecodedAudio;
use ozma_pipewire::{PipeWireAudioSink as Inner, PipeWireError, SinkConfig};

use crate::audio::{validate_audio_format, AudioSink};
use crate::error::PlaybackError;

fn map_error(error: PipeWireError) -> PlaybackError {
    PlaybackError::DeviceUnavailable {
        device: "pipewire".to_owned(),
        message: error.to_string(),
        remediation: error.remediation().to_owned(),
    }
}

/// A PipeWire playback sink implementing the client [`AudioSink`] trait.
pub struct PipeWireAudioSink {
    inner: Inner,
    opened: bool,
}

impl PipeWireAudioSink {
    /// Open the default PipeWire sink, or a configured target.
    pub async fn connect(config: SinkConfig) -> Result<Self, PlaybackError> {
        let inner = Inner::new(config).await.map_err(map_error)?;
        Ok(Self {
            inner,
            opened: true,
        })
    }

    /// Open the default sink.
    pub async fn default_sink() -> Result<Self, PlaybackError> {
        Self::connect(SinkConfig::default()).await
    }

    /// Current playback counters.
    pub fn stats(&self) -> ozma_pipewire::SinkStats {
        self.inner.stats()
    }
}

impl AudioSink for PipeWireAudioSink {
    fn open(&mut self) -> Result<(), PlaybackError> {
        self.opened = true;
        Ok(())
    }

    fn play(&mut self, audio: &DecodedAudio) -> Result<(), PlaybackError> {
        if !self.opened {
            return Err(PlaybackError::NotOpen {
                sink: "pipewire".to_owned(),
            });
        }
        validate_audio_format(audio)?;
        self.inner.write_all(&audio.samples).map_err(map_error)
    }

    fn flush(&mut self) -> Result<(), PlaybackError> {
        self.inner.flush().map_err(map_error)
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        if self.opened {
            self.opened = false;
            self.inner.stop();
        }
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.opened
    }
}
