// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Audio capture/encode runtime for a session.
//!
//! The runtime prefers a real ALSA source and falls back to the deterministic
//! fake source when no sound card is available. It produces Opus packets on a
//! bounded channel; the session transport sends them on the audio channel.

use std::sync::Arc;
use std::time::Duration;

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
    /// Start the real ALSA path, falling back to the fake source.
    pub fn start() -> Self {
        let source: Box<dyn AudioSource> = match AlsaCapture::open(AlsaCaptureConfig::default()) {
            Ok(capture) => Box::new(capture),
            Err(error) => {
                warn!(error = %error, "ALSA capture unavailable; using fake audio source");
                Box::new(FakeAudioSource::new().unlimited())
            }
        };
        Self::spawn(source)
    }

    /// Start with the deterministic fake source, for tests and local harnesses.
    pub fn fake() -> Self {
        Self::spawn(Box::new(FakeAudioSource::new().unlimited()))
    }

    fn spawn(mut source: Box<dyn AudioSource>) -> Self {
        let source_id = source.id().to_owned();
        let (tx, rx) = mpsc::channel(32);
        let task = tokio::spawn(async move {
            let mut encoder = match OpusEncoder::new(EncoderConfig::opus()) {
                Ok(encoder) => encoder,
                Err(error) => {
                    warn!(error = %error, "Opus encoder unavailable");
                    return;
                }
            };
            loop {
                let frame = match source.next_frame().await {
                    Ok(frame) => frame,
                    Err(error) => {
                        debug!(error = %error, "audio source ended");
                        break;
                    }
                };
                let packet = match encoder.encode(frame).await {
                    Ok(packet) => packet,
                    Err(error) => {
                        warn!(error = %error, "audio encode failed");
                        continue;
                    }
                };
                if tx.send(packet).await.is_err() {
                    break;
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
