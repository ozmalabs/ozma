// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Audio capture/encode runtime for a session.
//!
//! The runtime prefers a real ALSA source and falls back to the deterministic
//! fake source when no sound card is available. It produces Opus packets on a
//! bounded channel; the session transport sends them on the audio channel.

use std::sync::Arc;
use std::time::Duration;

#[cfg(target_os = "linux")]
use ozma_audio::PipeWireCapture;
use ozma_audio::{
    AlsaCapture, AlsaCaptureConfig, AudioEncoder, AudioSource, EncodedAudio, EncoderConfig,
    FakeAudioSource, OpusEncoder,
};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, warn};

/// A running audio capture/encode loop.
pub struct AudioRuntime {
    rx: mpsc::Receiver<EncodedAudio>,
    task: Option<JoinHandle<()>>,
    source: String,
}

impl AudioRuntime {
    /// Start the best native audio source: PipeWire first, then ALSA, then the
    /// deterministic fake source.
    ///
    /// `OZMA_AUDIO_SOURCE` overrides the choice for diagnostics and browser
    /// tests: `fake` emits a deterministic tone, `pipewire`/`alsa` disable the
    /// fallback chain. Any other value means `auto`.
    pub async fn start() -> Self {
        match std::env::var("OZMA_AUDIO_SOURCE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "fake" => {
                tracing::info!(
                    "Using paced deterministic fake audio source (OZMA_AUDIO_SOURCE=fake)"
                );
                return Self::spawn(
                    Box::new(PacedFakeAudioSource::new(
                        FakeAudioSource::new().unlimited(),
                    )),
                    None,
                );
            }
            "alsa" => {
                tracing::info!("Using ALSA audio source only (OZMA_AUDIO_SOURCE=alsa)");
                return Self::start_alsa();
            }
            "pipewire" => {
                tracing::info!("Using PipeWire audio source only (OZMA_AUDIO_SOURCE=pipewire)");
                return Self::start_pipewire().await;
            }
            _ => {}
        }
        #[cfg(target_os = "linux")]
        {
            match PipeWireCapture::default_source().await {
                Ok(mut capture) => {
                    // A suspended/idle PipeWire source may negotiate a format
                    // but not deliver a frame. Do not let that stall a session;
                    // fall back to ALSA/fake after a short probe.
                    match tokio::time::timeout(Duration::from_millis(1500), capture.next_frame())
                        .await
                    {
                        Ok(Ok(first)) => {
                            tracing::info!("Using PipeWire audio capture");
                            return Self::spawn(Box::new(capture), Some(first));
                        }
                        Ok(Err(error)) => {
                            warn!(error = %error, "PipeWire capture produced no frame; trying ALSA");
                        }
                        Err(_) => {
                            warn!("PipeWire capture produced no frame in time; trying ALSA");
                        }
                    }
                    let _ = capture.stop().await;
                }
                Err(error) => {
                    warn!(error = %error, "PipeWire capture unavailable; trying ALSA");
                }
            }
        }
        match AlsaCapture::open(AlsaCaptureConfig::default()) {
            Ok(mut capture) => {
                match tokio::time::timeout(Duration::from_millis(1500), capture.next_frame()).await
                {
                    Ok(Ok(first)) => {
                        tracing::info!("Using ALSA audio capture");
                        return Self::spawn(Box::new(capture), Some(first));
                    }
                    Ok(Err(error)) => {
                        warn!(error = %error, "ALSA capture produced no frame; using fake source");
                    }
                    Err(_) => {
                        warn!("ALSA capture produced no frame in time; using fake source");
                    }
                }
                let _ = capture.stop().await;
            }
            Err(error) => {
                warn!(error = %error, "ALSA capture unavailable; using fake audio source");
            }
        }
        Self::spawn(Box::new(FakeAudioSource::new().unlimited()), None)
    }

    async fn start_pipewire() -> Self {
        #[cfg(target_os = "linux")]
        {
            match PipeWireCapture::default_source().await {
                Ok(capture) => return Self::spawn(Box::new(capture), None),
                Err(error) => warn!(error = %error, "PipeWire audio capture unavailable"),
            }
        }
        Self::spawn(Box::new(FakeAudioSource::new().unlimited()), None)
    }

    fn start_alsa() -> Self {
        match AlsaCapture::open(AlsaCaptureConfig::default()) {
            Ok(capture) => Self::spawn(Box::new(capture), None),
            Err(error) => {
                warn!(error = %error, "ALSA capture unavailable; using fake audio source");
                Self::spawn(Box::new(FakeAudioSource::new().unlimited()), None)
            }
        }
    }

    /// Start with the deterministic fake source, for tests and local harnesses.
    ///
    /// This source emits frames as fast as the consumer polls. Use
    /// `OZMA_AUDIO_SOURCE=fake` for a real-time-paced tone.
    pub fn fake() -> Self {
        Self::spawn(Box::new(FakeAudioSource::new().unlimited()), None)
    }

    fn spawn(mut source: Box<dyn AudioSource>, first: Option<ozma_audio::AudioFrame>) -> Self {
        let source_id = source.id().to_owned();
        let (tx, rx) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            let config = EncoderConfig::opus();
            let mut encoder = match OpusEncoder::new(config.clone()) {
                Ok(encoder) => encoder,
                Err(error) => {
                    warn!(error = %error, "Opus encoder unavailable");
                    return;
                }
            };
            let mut reframer = match ozma_audio::AudioReframer::from_config(&config) {
                Ok(reframer) => reframer,
                Err(error) => {
                    warn!(error = %error, "Opus frame reframer unavailable");
                    return;
                }
            };
            let mut pending = first;
            loop {
                let frame = match match pending.take() {
                    Some(frame) => Ok(frame),
                    None => source.next_frame().await,
                } {
                    Ok(frame) => frame,
                    Err(error) => {
                        debug!(error = %error, "audio source ended");
                        break;
                    }
                };
                let frames = match reframer.push(frame) {
                    Ok(frames) => frames,
                    Err(error) => {
                        warn!(error = %error, "audio capture buffer cannot be reframed");
                        continue;
                    }
                };
                for frame in frames {
                    let packet = match encoder.encode(frame).await {
                        Ok(packet) => packet,
                        Err(error) => {
                            warn!(error = %error, "audio encode failed");
                            continue;
                        }
                    };
                    if tx.send(packet).await.is_err() {
                        return;
                    }
                }
            }
            let _ = source.stop().await;
        });
        Self {
            rx,
            task: Some(task),
            source: source_id,
        }
    }

    /// Stable source id for diagnostics.
    pub fn source_id(&self) -> &str {
        &self.source
    }

    /// Wait for the next encoded Opus packet.
    pub async fn recv(&mut self) -> Option<EncodedAudio> {
        self.rx.recv().await
    }

    /// Try to receive without waiting.
    pub fn try_recv(&mut self) -> Option<EncodedAudio> {
        self.rx.try_recv().ok()
    }

    /// Stop the worker. Safe to call more than once.
    pub fn stop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

impl Drop for AudioRuntime {
    fn drop(&mut self) {
        self.stop();
    }
}

/// A short-lived background task in the node can use this to verify the audio
/// path without opening a sound card.
pub async fn capture_one_fake_packet() -> Option<EncodedAudio> {
    let mut runtime = AudioRuntime::fake();
    let packet = tokio::time::timeout(Duration::from_secs(2), runtime.recv())
        .await
        .ok()
        .flatten();
    runtime.stop();
    packet
}

/// The transport uses `Arc<AudioRuntime>` only when it needs shared access; the
/// normal path owns it in the session task.
pub type SharedAudioRuntime = Arc<tokio::sync::Mutex<AudioRuntime>>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_runtime_produces_opus_packets() {
        let packet = capture_one_fake_packet().await.expect("fake audio packet");
        assert!(!packet.data.is_empty());
        assert!(packet.is_independent());
        assert_eq!(packet.sample_rate, 48_000);
        assert_eq!(packet.channels, 2);
    }
}

/// A deterministic tone paced at the Opus 20 ms frame interval.
///
/// `FakeAudioSource` is intentionally unpaced so unit tests run instantly. A
/// real session must not receive thousands of fake packets per second, so this
/// wrapper sleeps between frames. Used by `OZMA_AUDIO_SOURCE=fake`.
struct PacedFakeAudioSource {
    inner: FakeAudioSource,
    interval: Duration,
}

impl PacedFakeAudioSource {
    fn new(inner: FakeAudioSource) -> Self {
        Self {
            inner,
            interval: Duration::from_micros(u64::from(EncoderConfig::opus().frame_micros)),
        }
    }
}

#[async_trait::async_trait]
impl AudioSource for PacedFakeAudioSource {
    fn id(&self) -> &str {
        self.inner.id()
    }

    fn format(&self) -> ozma_audio::AudioFormat {
        self.inner.format()
    }

    async fn next_frame(&mut self) -> Result<ozma_audio::AudioFrame, ozma_audio::AudioError> {
        let frame = self.inner.next_frame().await?;
        tokio::time::sleep(self.interval).await;
        Ok(frame)
    }

    async fn stop(&mut self) -> Result<(), ozma_audio::AudioError> {
        self.inner.stop().await
    }
}
