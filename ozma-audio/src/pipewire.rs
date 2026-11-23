// SPDX-License-Identifier: AGPL-3.0-only
//! Native PipeWire capture for the audio pipeline.
//!
//! On Linux this wraps [`ozma_pipewire::PipeWireAudioSource`].  The named
//! conveniences accept an [`AudioTarget`] and build a [`CaptureConfig`]
//! without touching the graph until `open` is called, so validation and
//! target construction are testable without a live PipeWire session.
//!
//! On non-Linux platforms the type is still exported for API parity, but
//! every constructor returns [`AudioError::Unavailable`]: PipeWire capture is
//! a Linux feature, and the ALSA or fake sources are the fallbacks there.

#[cfg(target_os = "linux")]
use async_trait::async_trait;
#[cfg(target_os = "linux")]
use ozma_pipewire::{CaptureConfig, PipeWireAudioSource, PipeWireError};

use crate::error::AudioError;
#[cfg(target_os = "linux")]
use crate::frame::{AudioFormat, AudioFrame};
use crate::target::AudioTarget;
#[cfg(target_os = "linux")]
use crate::traits::AudioSource;

#[cfg(target_os = "linux")]
fn map_error(error: PipeWireError) -> AudioError {
    AudioError::Unavailable {
        device: "pipewire".to_owned(),
        message: error.to_string(),
        remediation: error.remediation().to_owned(),
    }
}

/// Build the capture config for a target without opening a stream.
///
/// This is deliberately pure: it is the tested bridge between the
/// backend-neutral [`AudioTarget`] and `ozma-pipewire`'s `CaptureConfig`.
#[cfg(target_os = "linux")]
pub fn capture_config_for_target(target: &AudioTarget) -> CaptureConfig {
    CaptureConfig {
        target: target.to_stream_target(),
        ..CaptureConfig::default()
    }
}

/// A PipeWire capture source that exposes interleaved S16 frames.
#[cfg(target_os = "linux")]
pub struct PipeWireCapture {
    inner: PipeWireAudioSource,
}

#[cfg(target_os = "linux")]
impl PipeWireCapture {
    /// Open the default PipeWire source, or a configured target.
    ///
    /// Existing callers can keep passing a full [`CaptureConfig`]; use
    /// [`PipeWireCapture::open_target`] when only the selection matters.
    pub async fn open(config: CaptureConfig) -> Result<Self, AudioError> {
        let inner = PipeWireAudioSource::new(config).await.map_err(map_error)?;
        Ok(Self { inner })
    }

    /// Open a named target.
    pub async fn open_target(target: AudioTarget) -> Result<Self, AudioError> {
        target.validate()?;
        Self::open(capture_config_for_target(&target)).await
    }

    /// Capture the monitor of a named sink.
    ///
    /// PipeWire names a sink's monitor `<sink>.monitor`.  The sink and its
    /// monitor must already exist in the graph; this does not create a null
    /// sink or route the VM's audio to it.
    pub async fn open_sink_monitor(sink: &str) -> Result<Self, AudioError> {
        Self::open_target(AudioTarget::sink_monitor(sink)?).await
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

#[cfg(target_os = "linux")]
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

#[cfg(not(target_os = "linux"))]
fn unsupported() -> AudioError {
    AudioError::unavailable(
        "pipewire",
        "native PipeWire capture is only available on Linux",
        "Run the node on Linux with PipeWire, or use the ALSA/fake audio source on this platform.",
    )
}

/// Non-Linux placeholder for API parity.
///
/// Constructors return [`AudioError::Unavailable`] with a remediation; they
/// never pretend a stream was opened.
#[cfg(not(target_os = "linux"))]
pub struct PipeWireCapture;

#[cfg(not(target_os = "linux"))]
impl PipeWireCapture {
    /// Always fails on non-Linux platforms.
    pub async fn open_target(_target: AudioTarget) -> Result<Self, AudioError> {
        Err(unsupported())
    }

    /// Always fails on non-Linux platforms.
    pub async fn open_sink_monitor(_sink: &str) -> Result<Self, AudioError> {
        Err(unsupported())
    }

    /// Always fails on non-Linux platforms.
    pub async fn default_source() -> Result<Self, AudioError> {
        Err(unsupported())
    }
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use super::*;
    use ozma_pipewire::StreamTarget;

    #[test]
    fn sink_monitor_config_targets_the_monitor_object() {
        let config = capture_config_for_target(&AudioTarget::monitor_for_sink("ozma-vm1"));
        assert_eq!(
            config.target,
            StreamTarget::Object("ozma-vm1.monitor".to_owned())
        );
        assert_eq!(config.stream_name, CaptureConfig::default().stream_name);
        assert_eq!(config.sample_rate, CaptureConfig::default().sample_rate);
        assert_eq!(config.channels, CaptureConfig::default().channels);
    }

    #[test]
    fn default_target_config_is_the_default_capture_config() {
        let config = capture_config_for_target(&AudioTarget::Default);
        assert_eq!(config.target, StreamTarget::Default);
        assert_eq!(config.stream_name, CaptureConfig::default().stream_name);
    }

    #[tokio::test]
    async fn open_sink_monitor_rejects_an_empty_sink_without_a_session() {
        let error = match PipeWireCapture::open_sink_monitor("").await {
            Ok(_) => panic!("empty sink must be rejected before opening"),
            Err(error) => error,
        };
        assert!(matches!(error, AudioError::InvalidTarget(_)));
    }
}

#[cfg(all(test, not(target_os = "linux")))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn constructors_are_honest_about_the_platform() {
        let mut errors = Vec::new();
        if let Err(error) = PipeWireCapture::open_target(AudioTarget::Default).await {
            errors.push(error);
        }
        if let Err(error) = PipeWireCapture::open_sink_monitor("ozma-vm1").await {
            errors.push(error);
        }
        if let Err(error) = PipeWireCapture::default_source().await {
            errors.push(error);
        }
        assert_eq!(errors.len(), 3, "every constructor must fail on non-Linux");
        for error in errors {
            assert!(matches!(error, AudioError::Unavailable { .. }));
            assert!(error.to_string().contains("Linux"));
        }
    }
}
