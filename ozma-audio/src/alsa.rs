// SPDX-License-Identifier: AGPL-3.0-only
//! ALSA capture with a dedicated blocking worker thread.
//!
//! `alsa`'s `PCM` handle is blocking and not designed to be shared with an
//! async runtime. Mirroring the V4L2 capture worker in `ozma-capture`, a
//! dedicated thread owns the handle for its whole lifetime, reads interleaved
//! S16 periods, assembles exact 20 ms frames, and sends them over a bounded
//! `tokio::sync::mpsc` channel. [`AudioSource::next_frame`] only awaits that
//! channel.
//!
//! The worker opens the device during construction and reports success or a
//! structured [`AudioError`] before the constructor returns, so a missing or
//! misconfigured device fails cleanly instead of hanging the caller. Frames
//! are timestamped from the sample clock starting at stream open, not from
//! wall-clock guesses; a capture resync therefore shows up as a gap in PTS
//! rather than as A/V drift.

use std::sync::{
    atomic::{AtomicBool, Ordering},
    mpsc as std_mpsc, Arc,
};
use std::time::Duration;

use alsa::{
    pcm::{Access, Format, HwParams, PCM},
    Direction, ValueOr,
};
use async_trait::async_trait;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::error::AudioError;
use crate::frame::{
    samples_per_frame, AudioFormat, AudioFrame, CHANNELS, FRAME_MICROS, SAMPLE_RATE,
};
use crate::traits::AudioSource;

/// Capacity of the frame channel between the worker and the async source.
///
/// Four 20 ms frames is ~80 ms of buffering: enough to absorb scheduler
/// jitter, small enough that `stop` cannot be stuck behind a long backlog.
pub const FRAME_CHANNEL_CAPACITY: usize = 4;

/// Default ALSA period: 960 frames, i.e. 20 ms at 48 kHz.
pub const DEFAULT_PERIOD_FRAMES: u32 = 960;

/// Default ring buffer length in periods.
pub const DEFAULT_BUFFER_PERIODS: u32 = 4;

/// How long [`AlsaCapture::open`] waits for the worker to open and configure
/// the device.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long the worker waits for capture data before re-checking the stop
/// flag. Bounds shutdown latency on a stalled device.
const READ_WAIT_TIMEOUT_MS: u32 = 100;

/// Configuration for [`AlsaCapture`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlsaCaptureConfig {
    /// ALSA PCM name, for example `default`, `hw:1,0`, or `plughw:1,0`.
    pub device: String,
    /// Sample rate. Must be 48 kHz for the Opus encoder.
    pub sample_rate: u32,
    /// Interleaved channel count. Must be 2 for the Opus encoder.
    pub channels: u8,
    /// ALSA period size in frames.
    pub period_frames: u32,
    /// Ring buffer length in periods.
    pub buffer_periods: u32,
    /// Constructor timeout for opening and configuring the device.
    pub start_timeout: Duration,
}

impl Default for AlsaCaptureConfig {
    fn default() -> Self {
        Self {
            device: "default".to_owned(),
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            period_frames: DEFAULT_PERIOD_FRAMES,
            buffer_periods: DEFAULT_BUFFER_PERIODS,
            start_timeout: DEFAULT_START_TIMEOUT,
        }
    }
}

impl AlsaCaptureConfig {
    /// Config for a specific ALSA device, keeping the 48 kHz stereo defaults.
    pub fn for_device(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            ..Self::default()
        }
    }

    /// Samples per channel in one emitted frame (20 ms by default).
    fn frame_samples(&self) -> usize {
        samples_per_frame(self.sample_rate, FRAME_MICROS).unwrap_or(0)
    }

    fn validate(&self) -> Result<(), AudioError> {
        if self.sample_rate == 0 || self.channels == 0 || self.period_frames == 0 {
            return Err(AudioError::unavailable(
                self.device.clone(),
                "invalid ALSA configuration: sample rate, channels, and period must be non-zero",
                "Use the default 48 kHz stereo configuration unless the device is known to differ.",
            ));
        }
        if self.sample_rate != SAMPLE_RATE || self.channels != CHANNELS {
            return Err(AudioError::unavailable(
                self.device.clone(),
                format!(
                    "the capture path is fixed at {SAMPLE_RATE} Hz stereo, not {} Hz {} ch",
                    self.sample_rate, self.channels
                ),
                "Capture at 48000 Hz stereo S16_LE; a `plug` ALSA device can resample legacy hardware.",
            ));
        }
        if self.frame_samples() == 0 {
            return Err(AudioError::unavailable(
                self.device.clone(),
                format!(
                    "cannot form a {} us frame at {} Hz",
                    FRAME_MICROS, self.sample_rate
                ),
                "Use a sample rate that divides evenly into 20 ms frames, normally 48000 Hz.",
            ));
        }
        Ok(())
    }
}

/// A live ALSA capture source.
///
/// Construction spawns the worker and waits for it to open and configure the
/// PCM. Dropping the source signals the worker and detaches it; call
/// [`AudioSource::stop`] for a joined, deterministic shutdown.
pub struct AlsaCapture {
    id: String,
    format: AudioFormat,
    frames: mpsc::Receiver<Result<AudioFrame, AudioError>>,
    stop: Arc<AtomicBool>,
    worker: Option<std::thread::JoinHandle<()>>,
    stopped: bool,
}

impl std::fmt::Debug for AlsaCapture {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlsaCapture")
            .field("id", &self.id)
            .field("format", &self.format)
            .field("stopped", &self.stopped)
            .finish()
    }
}

impl AlsaCapture {
    /// Open and configure an ALSA capture device.
    ///
    /// Fails with [`AudioError::Unavailable`] (code `audio.unavailable`) when
    /// the device does not exist, cannot provide 48 kHz stereo S16_LE, or is
    /// busy. The call is bounded by [`AlsaCaptureConfig::start_timeout`]; a
    /// driver that wedges during setup does not block the caller forever.
    pub fn open(config: AlsaCaptureConfig) -> Result<Self, AudioError> {
        config.validate()?;

        let id = config.device.clone();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (frame_tx, frame_rx) = mpsc::channel(FRAME_CHANNEL_CAPACITY);
        let stop = Arc::new(AtomicBool::new(false));

        let worker_config = config.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name(format!("ozma-alsa-{}", sanitize_thread_name(&id)))
            .spawn(move || capture_worker(worker_config, worker_stop, ready_tx, frame_tx))
            .map_err(|error| {
                AudioError::unavailable(
                    id.clone(),
                    format!("failed to start the ALSA capture worker: {error}"),
                    "Check process/thread limits and retry.",
                )
            })?;

        match ready_rx.recv_timeout(config.start_timeout) {
            Ok(Ok(())) => Ok(Self {
                id,
                // The AlsaCaptureConfig contract is 48 kHz stereo; `open_pcm`
                // rejects a device that negotiates anything else.
                format: AudioFormat::opus(),
                frames: frame_rx,
                stop,
                worker: Some(worker),
                stopped: false,
            }),
            Ok(Err(error)) => {
                // The worker exits immediately after reporting a setup error.
                let _ = worker.join();
                Err(error)
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // Do not join a thread that may be wedged inside ALSA; ask it
                // to stop at its next checkpoint and return the error now.
                stop.store(true, Ordering::Relaxed);
                Err(AudioError::unavailable(
                    id,
                    format!(
                        "timed out after {} ms opening the device",
                        config.start_timeout.as_millis()
                    ),
                    "Check that the device is present and not held by another process.",
                ))
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                Err(AudioError::unavailable(
                    id,
                    "the ALSA capture worker exited during setup",
                    "Check dmesg for a USB audio reset, then retry.",
                ))
            }
        }
    }

    /// ALSA device string behind this source.
    pub fn device(&self) -> &str {
        &self.id
    }
}

impl Drop for AlsaCapture {
    fn drop(&mut self) {
        // `stop` is the joined path. A bare drop must not block, so close the
        // receiver (unblocking a worker waiting to send) and detach the join
        // handle. The worker closes the PCM as soon as it observes the flag.
        self.frames.close();
        self.stop.store(true, Ordering::Relaxed);
        self.worker.take();
    }
}

#[async_trait]
impl AudioSource for AlsaCapture {
    fn id(&self) -> &str {
        &self.id
    }

    fn format(&self) -> AudioFormat {
        self.format.clone()
    }

    async fn next_frame(&mut self) -> Result<AudioFrame, AudioError> {
        if self.stopped {
            return Err(AudioError::Closed {
                id: self.id.clone(),
            });
        }

        // Return a frame already buffered by a cancelled `next_frame` before
        // awaiting the worker again.
        if let Ok(outcome) = self.frames.try_recv() {
            return outcome;
        }

        match self.frames.recv().await {
            Some(outcome) => outcome,
            None => {
                self.stopped = true;
                Err(AudioError::Closed {
                    id: self.id.clone(),
                })
            }
        }
    }

    async fn stop(&mut self) -> Result<(), AudioError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        self.frames.close();
        self.stop.store(true, Ordering::Relaxed);

        if let Some(worker) = self.worker.take() {
            match tokio::task::spawn_blocking(move || worker.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    return Err(AudioError::capture(
                        &self.id,
                        "ALSA capture worker panicked while stopping",
                    ))
                }
                Err(error) => {
                    return Err(AudioError::capture(
                        &self.id,
                        format!("could not join ALSA capture worker: {error}"),
                    ))
                }
            }
        }
        Ok(())
    }
}

/// Owns the blocking PCM handle for the life of the source.
fn capture_worker(
    config: AlsaCaptureConfig,
    stop: Arc<AtomicBool>,
    ready: std_mpsc::SyncSender<Result<(), AudioError>>,
    frames: mpsc::Sender<Result<AudioFrame, AudioError>>,
) {
    let device = config.device.clone();
    let pcm = match open_pcm(&config) {
        Ok(pcm) => pcm,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    if ready.send(Ok(())).is_err() {
        // The constructor went away (for example on timeout).
        return;
    }
    info!(
        device = %device,
        sample_rate = config.sample_rate,
        channels = config.channels,
        period_frames = config.period_frames,
        "ALSA capture worker started"
    );

    let channels = usize::from(config.channels);
    let frame_samples = config.frame_samples();
    let frame_len = frame_samples * channels;
    let frame_micros = 1_000_000u64 * frame_samples as u64 / u64::from(config.sample_rate);
    let read_frames = configured_period(&pcm, config.period_frames * config.buffer_periods);
    let mut read_buffer = vec![0i16; read_frames * channels];
    let mut pending: Vec<i16> = Vec::with_capacity(frame_len * 2);

    let Ok(io) = pcm.io_i16() else {
        let _ = frames.blocking_send(Err(AudioError::capture(
            device,
            "device does not support interleaved S16 I/O",
        )));
        return;
    };

    let mut sequence = 0u64;
    let mut pts = 0u64;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }

        // Poll for readiness with a short timeout instead of blocking in
        // `readi` forever. This keeps `stop` bounded even if the device
        // stalls, while still doing real blocking capture I/O.
        match pcm.wait(Some(READ_WAIT_TIMEOUT_MS)) {
            Ok(true) => {}
            Ok(false) => continue,
            Err(error) => {
                if pcm.try_recover(error, true).is_ok() {
                    debug!(device = %device, "recovered while waiting for ALSA capture data");
                    continue;
                }
                let _ = frames.blocking_send(Err(AudioError::capture(
                    &device,
                    format!("wait failed: {error}"),
                )));
                break;
            }
        }

        let read = match io.readi(&mut read_buffer) {
            Ok(0) => continue,
            Ok(frames_read) => frames_read,
            Err(error) => {
                if pcm.try_recover(error, true).is_ok() {
                    debug!(device = %device, "recovered from an ALSA capture error");
                    continue;
                }
                let _ = frames.blocking_send(Err(AudioError::capture(
                    &device,
                    format!("read failed: {error}"),
                )));
                break;
            }
        };

        let samples = &read_buffer[..read * channels];
        pending.extend_from_slice(samples);

        while pending.len() >= frame_len {
            if stop.load(Ordering::Relaxed) {
                break;
            }
            let chunk: Vec<i16> = pending.drain(..frame_len).collect();
            let frame =
                match AudioFrame::new(pts, sequence, config.sample_rate, config.channels, chunk) {
                    Ok(frame) => frame,
                    Err(error) => {
                        let _ = frames.blocking_send(Err(error));
                        return;
                    }
                };
            if frames.blocking_send(Ok(frame)).is_err() {
                // The async side was dropped or stopped.
                return;
            }
            sequence = sequence.saturating_add(1);
            pts = pts.saturating_add(frame_micros);
        }
    }

    debug!(device = %device, "ALSA capture worker stopped");
}

/// Open and configure a capture PCM at 48 kHz stereo S16_LE.
fn open_pcm(config: &AlsaCaptureConfig) -> Result<PCM, AudioError> {
    let device = config.device.clone();
    let pcm = PCM::new(&device, Direction::Capture, false).map_err(|error| {
        AudioError::unavailable(
            device.clone(),
            format!("cannot open ALSA capture device: {error}"),
            "Check the device name with `arecord -l`, that the service user is in the audio group, and that no other process holds it.",
        )
    })?;

    let configure = || -> Result<(), AudioError> {
        let hwp = HwParams::any(&pcm).map_err(|error| {
            AudioError::unavailable(
                device.clone(),
                format!("cannot read hardware parameters: {error}"),
                "The device may not support capture; check `arecord -l`.",
            )
        })?;
        hwp.set_channels(u32::from(config.channels))
            .map_err(|error| unavailable_param(&device, "channels", error))?;
        hwp.set_rate(config.sample_rate, ValueOr::Nearest)
            .map_err(|error| unavailable_param(&device, "sample rate", error))?;
        hwp.set_format(Format::S16LE)
            .map_err(|error| unavailable_param(&device, "S16_LE format", error))?;
        hwp.set_access(Access::RWInterleaved)
            .map_err(|error| unavailable_param(&device, "interleaved access", error))?;
        hwp.set_period_size(config.period_frames as alsa::pcm::Frames, ValueOr::Nearest)
            .map_err(|error| unavailable_param(&device, "period size", error))?;
        hwp.set_buffer_size((config.period_frames * config.buffer_periods) as alsa::pcm::Frames)
            .map_err(|error| unavailable_param(&device, "buffer size", error))?;
        pcm.hw_params(&hwp)
            .map_err(|error| unavailable_param(&device, "hardware parameters", error))?;
        pcm.prepare().map_err(|error| {
            AudioError::unavailable(
                device.clone(),
                format!("cannot prepare the capture stream: {error}"),
                "Another process may be using the device; try again once it is free.",
            )
        })?;
        pcm.start().map_err(|error| {
            AudioError::unavailable(
                device.clone(),
                format!("cannot start the capture stream: {error}"),
                "Check that the device supports capture and is not suspended.",
            )
        })?;
        Ok(())
    };
    configure()?;

    // Verify the kernel accepted exactly what the encoder needs. ALSA plugins
    // may silently resample; reporting that as an unavailable device is more
    // honest than labelling frames with a rate they do not have.
    let (rate, channels) = {
        let current = pcm.hw_params_current().map_err(|error| {
            AudioError::unavailable(
                device.clone(),
                format!("cannot read negotiated hardware parameters: {error}"),
                "Retry opening the device.",
            )
        })?;
        (
            current.get_rate().unwrap_or(0),
            current.get_channels().unwrap_or(0),
        )
    };
    if rate != config.sample_rate || channels != u32::from(config.channels) {
        warn!(
            device = %device,
            negotiated_rate = rate,
            negotiated_channels = channels,
            "ALSA device did not accept 48 kHz stereo"
        );
        return Err(AudioError::unavailable(
            device,
            format!(
                "device negotiated {rate} Hz {channels} ch, but the encoder requires {} Hz {} ch",
                config.sample_rate, config.channels
            ),
            "Use a device or plugin that supports 48 kHz stereo, for example `plug:default`.",
        ));
    }

    Ok(pcm)
}

fn configured_period(pcm: &PCM, fallback_frames: u32) -> usize {
    let period = pcm
        .hw_params_current()
        .and_then(|current| current.get_period_size())
        .ok()
        .and_then(|frames| usize::try_from(frames).ok())
        .filter(|frames| *frames > 0)
        .unwrap_or(fallback_frames as usize);
    period.min(fallback_frames as usize).max(1)
}

fn unavailable_param(device: &str, parameter: &str, error: alsa::Error) -> AudioError {
    AudioError::unavailable(
        device.to_owned(),
        format!("device cannot provide the required {parameter}: {error}"),
        "Verify the device supports 48 kHz stereo S16_LE capture.",
    )
}

fn sanitize_thread_name(device: &str) -> String {
    let name: String = device
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '-'
            }
        })
        .collect();
    name.chars().take(24).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn defaults_are_the_interactive_values() {
        let config = AlsaCaptureConfig::default();
        assert_eq!(config.device, "default");
        assert_eq!(config.sample_rate, 48_000);
        assert_eq!(config.channels, 2);
        assert_eq!(config.period_frames, 960);
        assert_eq!(config.frame_samples(), 960);
        assert_eq!(config.buffer_periods, 4);
    }

    #[test]
    fn invalid_configs_are_rejected_before_opening_a_device() {
        let mut config = AlsaCaptureConfig::default();
        config.channels = 0;
        assert!(matches!(
            AlsaCapture::open(config).unwrap_err(),
            AudioError::Unavailable { .. }
        ));

        let mut config = AlsaCaptureConfig::default();
        config.sample_rate = 0;
        assert!(matches!(
            AlsaCapture::open(config).unwrap_err(),
            AudioError::Unavailable { .. }
        ));

        let mut config = AlsaCaptureConfig::default();
        config.sample_rate = 44_100;
        assert!(matches!(
            AlsaCapture::open(config).unwrap_err(),
            AudioError::Unavailable { .. }
        ));

        let mut config = AlsaCaptureConfig::default();
        config.channels = 1;
        assert!(matches!(
            AlsaCapture::open(config).unwrap_err(),
            AudioError::Unavailable { .. }
        ));
    }

    #[test]
    fn thread_names_are_safe_and_bounded() {
        assert_eq!(sanitize_thread_name("hw:1,0"), "hw-1-0");
        assert_eq!(sanitize_thread_name("default"), "default");
        assert!(sanitize_thread_name(&"x".repeat(100)).len() <= 24);
    }

    /// A clearly invalid device name must fail fast with a structured
    /// `audio.unavailable` error rather than blocking the caller.
    #[test]
    fn missing_device_returns_a_structured_error_without_hanging() {
        let config = AlsaCaptureConfig {
            device: "ozma-audio-no-such-device".to_owned(),
            start_timeout: Duration::from_secs(2),
            ..AlsaCaptureConfig::default()
        };
        let started = Instant::now();
        let error = AlsaCapture::open(config).expect_err("invalid ALSA device must not open");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "opening an invalid device took {elapsed:?}"
        );
        match &error {
            AudioError::Unavailable { device, .. } => {
                assert_eq!(device, "ozma-audio-no-such-device");
            }
            other => panic!("expected AudioError::Unavailable, got {other:?}"),
        }
        let api = error.to_api_error();
        assert_eq!(api.code, crate::error::audio_codes::AUDIO_UNAVAILABLE);
        assert_eq!(api.details.as_str(), Some("ozma-audio-no-such-device"));
        assert!(api.remediation.is_some());
    }

    /// Opt-in hardware test. Run with:
    ///
    /// ```text
    /// OZMA_AUDIO_TEST_DEVICE=default \
    ///     cargo test -p ozma-audio -- --ignored real_alsa_capture
    /// ```
    #[tokio::test]
    #[ignore = "requires a real ALSA capture device; set OZMA_AUDIO_TEST_DEVICE"]
    async fn real_alsa_capture_yields_monotonic_20ms_frames() {
        let Ok(device) = std::env::var("OZMA_AUDIO_TEST_DEVICE") else {
            eprintln!("skipping: OZMA_AUDIO_TEST_DEVICE is not set");
            return;
        };
        let mut source = AlsaCapture::open(AlsaCaptureConfig::for_device(device))
            .expect("open configured ALSA device");
        let first = source.next_frame().await.expect("first frame");
        assert_eq!(first.sequence, 0);
        assert_eq!(first.frames_per_channel(), 960);
        assert!(!first.is_empty());
        let second = source.next_frame().await.expect("second frame");
        assert_eq!(second.sequence, 1);
        assert!(second.pts_micros > first.pts_micros);
        source.stop().await.expect("stop");
        source.stop().await.expect("stop is idempotent");
    }
}
