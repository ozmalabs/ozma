// SPDX-License-Identifier: AGPL-3.0-only
//! Audio sinks: a null recorder, and blocking ALSA output on a worker thread.
//!
//! The decode crate produces [`DecodedAudio`] blocks that are already
//! interleaved S16. A sink only has to move those blocks to a device. The
//! client's fixed interactive format is 48 kHz stereo, matching
//! [`ozma_audio::SAMPLE_RATE`] and [`ozma_audio::CHANNELS`]; an ALSA sink rejects
//! anything else instead of silently resampling or mislabelling time.
//!
//! [`AlsaAudioSink`] follows the same rule as `ozma-audio`'s capture source:
//! ALSA's PCM handle is blocking and is not safe to share with an async runtime,
//! so a dedicated thread owns it for its whole life. [`AudioSink::play`] only
//! enqueues into a bounded channel and never touches the device, which keeps a
//! slow or wedged sound card from blocking the session's control path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{mpsc as std_mpsc, Arc};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::{Direction, ValueOr};
use ozma_decode::DecodedAudio;
use tracing::{debug, info, warn};

use crate::error::PlaybackError;

/// Interactive sample rate required by every Ozma audio sink.
pub const PLAYBACK_SAMPLE_RATE: u32 = ozma_audio::SAMPLE_RATE;

/// Interactive channel count required by every Ozma audio sink.
pub const PLAYBACK_CHANNELS: u8 = ozma_audio::CHANNELS;

/// Default ALSA period: 960 frames, i.e. 20 ms at 48 kHz.
pub const DEFAULT_PERIOD_FRAMES: u32 = 960;

/// Default ring buffer length in periods.
pub const DEFAULT_BUFFER_PERIODS: u32 = 4;

/// Default number of 20 ms blocks buffered between the caller and the device.
pub const DEFAULT_QUEUE_CAPACITY: usize = 8;

/// How long [`AlsaAudioSink::open`] waits for the worker to open the device.
pub const DEFAULT_START_TIMEOUT: Duration = Duration::from_secs(5);

/// How long [`AudioSink::play`] waits for room in the bounded queue.
const PLAY_BACKPRESSURE_TIMEOUT: Duration = Duration::from_secs(1);

/// How long [`AudioSink::flush`] waits for the device drain to finish.
const FLUSH_TIMEOUT: Duration = Duration::from_secs(5);

/// A playback destination for decoded interleaved S16 audio.
///
/// Implementations are `Send` because the playback engine moves them to a
/// dedicated worker thread. Methods are synchronous; an async caller should
/// keep the engine off the reactor or wrap setup in `spawn_blocking`.
pub trait AudioSink: Send {
    /// Open the underlying device or output.
    ///
    /// Calling `open` on an already-open sink is a no-op so a caller can own
    /// lifecycle without tracking readiness separately.
    fn open(&mut self) -> Result<(), PlaybackError>;

    /// Queue one decoded block for playback.
    ///
    /// The block must match [`PLAYBACK_SAMPLE_RATE`] and [`PLAYBACK_CHANNELS`].
    fn play(&mut self, audio: &DecodedAudio) -> Result<(), PlaybackError>;

    /// Discard queued audio, then wait for the device to drain what it has.
    fn flush(&mut self) -> Result<(), PlaybackError>;

    /// Stop playback and release the device. Repeated calls return `Ok(())`.
    fn stop(&mut self) -> Result<(), PlaybackError>;

    /// True while the sink is open and able to accept audio.
    fn is_open(&self) -> bool;
}

/// Validate the structural invariants of a decoded block.
pub fn validate_audio_block(audio: &DecodedAudio) -> Result<(), PlaybackError> {
    if audio.sample_rate == 0 {
        return Err(PlaybackError::invalid_audio("sample rate must be non-zero"));
    }
    if audio.channels == 0 {
        return Err(PlaybackError::invalid_audio(
            "channel count must be non-zero",
        ));
    }
    if audio.samples.is_empty() {
        return Err(PlaybackError::invalid_audio(
            "sample buffer must not be empty",
        ));
    }
    let channels = usize::from(audio.channels);
    if audio.samples.len() % channels != 0 {
        return Err(PlaybackError::invalid_audio(format!(
            "{} samples is not a whole number of {channels}-channel frames",
            audio.samples.len()
        )));
    }
    Ok(())
}

/// Validate that a block can be played by a canonical Ozma sink.
///
/// This is public so a client can check before allocating a decode buffer. The
/// expected format is the one the node encodes: 48 kHz stereo S16_LE.
pub fn validate_audio_format(audio: &DecodedAudio) -> Result<(), PlaybackError> {
    validate_audio_block(audio)?;
    if audio.sample_rate != PLAYBACK_SAMPLE_RATE || audio.channels != PLAYBACK_CHANNELS {
        return Err(PlaybackError::format_mismatch(
            format!("{PLAYBACK_SAMPLE_RATE} Hz {PLAYBACK_CHANNELS} ch S16"),
            format!("{} Hz {} ch S16", audio.sample_rate, audio.channels),
        ));
    }
    Ok(())
}

/// One block recorded by [`NullAudioSink`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct AudioRecord {
    /// Presentation timestamp copied from the decoded block.
    pub pts_micros: u64,
    /// Sequence number copied from the decoded block.
    pub sequence: u64,
    /// Sample rate of the recorded block.
    pub sample_rate: u32,
    /// Interleaved channel count of the recorded block.
    pub channels: u8,
    /// Number of interleaved `i16` samples.
    pub samples: usize,
}

/// An audio sink that records every block instead of touching hardware.
///
/// This is the headless/CI sink. It intentionally accepts any structurally
/// valid rate and channel count, so tests can exercise non-canonical input
/// without a device; [`validate_audio_format`] is available when a caller wants
/// the strict contract.
#[derive(Debug, Default)]
pub struct NullAudioSink {
    opened: bool,
    records: Vec<AudioRecord>,
    bytes_played: u64,
    flushes: u64,
    stop_calls: u64,
}

impl NullAudioSink {
    /// Create a closed null sink. Call [`AudioSink::open`] before playing.
    pub fn new() -> Self {
        Self::default()
    }

    /// Blocks recorded so far, oldest first.
    pub fn records(&self) -> &[AudioRecord] {
        &self.records
    }

    /// Number of blocks accepted by [`AudioSink::play`].
    pub fn frames_played(&self) -> u64 {
        u64::try_from(self.records.len()).unwrap_or(u64::MAX)
    }

    /// Total S16 sample values accepted, across all channels.
    pub fn samples_played(&self) -> u64 {
        self.records
            .iter()
            .map(|record| u64::try_from(record.samples).unwrap_or(u64::MAX))
            .fold(0u64, u64::saturating_add)
    }

    /// Total bytes of S16 PCM accepted.
    pub fn bytes_played(&self) -> u64 {
        self.bytes_played
    }

    /// Number of [`AudioSink::flush`] calls.
    pub fn flushes(&self) -> u64 {
        self.flushes
    }

    /// Number of [`AudioSink::stop`] calls, including repeated ones.
    pub fn stop_calls(&self) -> u64 {
        self.stop_calls
    }

    /// The most recently recorded block, if any.
    pub fn last(&self) -> Option<&AudioRecord> {
        self.records.last()
    }
}

impl AudioSink for NullAudioSink {
    fn open(&mut self) -> Result<(), PlaybackError> {
        self.opened = true;
        Ok(())
    }

    fn play(&mut self, audio: &DecodedAudio) -> Result<(), PlaybackError> {
        if !self.opened {
            return Err(PlaybackError::not_open("null-audio"));
        }
        validate_audio_block(audio)?;
        let samples = audio.samples.len();
        self.records.push(AudioRecord {
            pts_micros: audio.pts_micros,
            sequence: audio.sequence,
            sample_rate: audio.sample_rate,
            channels: audio.channels,
            samples,
        });
        self.bytes_played = self
            .bytes_played
            .saturating_add(u64::try_from(samples).unwrap_or(u64::MAX).saturating_mul(2));
        Ok(())
    }

    fn flush(&mut self) -> Result<(), PlaybackError> {
        self.flushes = self.flushes.saturating_add(1);
        Ok(())
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        self.opened = false;
        self.stop_calls = self.stop_calls.saturating_add(1);
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.opened
    }
}

/// Configuration for [`AlsaAudioSink`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AlsaAudioSinkConfig {
    /// ALSA PCM name, for example `default`, `hw:1,0`, or `plughw:1,0`.
    pub device: String,
    /// Sample rate. Must be 48 kHz.
    pub sample_rate: u32,
    /// Interleaved channel count. Must be 2.
    pub channels: u8,
    /// ALSA period size in frames.
    pub period_frames: u32,
    /// Ring buffer length in periods.
    pub buffer_periods: u32,
    /// Blocks buffered between the caller and the device worker.
    pub queue_capacity: usize,
    /// Constructor timeout for opening and configuring the device.
    pub start_timeout: Duration,
}

impl Default for AlsaAudioSinkConfig {
    fn default() -> Self {
        Self {
            device: "default".to_owned(),
            sample_rate: PLAYBACK_SAMPLE_RATE,
            channels: PLAYBACK_CHANNELS,
            period_frames: DEFAULT_PERIOD_FRAMES,
            buffer_periods: DEFAULT_BUFFER_PERIODS,
            queue_capacity: DEFAULT_QUEUE_CAPACITY,
            start_timeout: DEFAULT_START_TIMEOUT,
        }
    }
}

impl AlsaAudioSinkConfig {
    /// Config for a specific ALSA device, keeping the 48 kHz stereo defaults.
    pub fn for_device(device: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), PlaybackError> {
        if self.sample_rate != PLAYBACK_SAMPLE_RATE || self.channels != PLAYBACK_CHANNELS {
            return Err(PlaybackError::format_mismatch(
                format!("{PLAYBACK_SAMPLE_RATE} Hz {PLAYBACK_CHANNELS} ch S16"),
                format!("{} Hz {} ch S16", self.sample_rate, self.channels),
            ));
        }
        if self.period_frames == 0 || self.buffer_periods == 0 {
            return Err(PlaybackError::device_unavailable(
                self.device.clone(),
                "invalid ALSA configuration: period and buffer must be non-zero",
                "Use the default 960-frame period and four-period buffer.",
            ));
        }
        Ok(())
    }
}

/// A command sent to the blocking ALSA worker.
enum AudioCommand {
    Frame(Vec<i16>),
    Flush(std_mpsc::SyncSender<()>),
    Stop,
}

/// A live ALSA playback sink.
///
/// Construction through [`AlsaAudioSink::new`] validates the format without
/// touching the device; [`AlsaAudioSink::connect`] also opens it. Either way,
/// [`AudioSink::open`] reports a missing or misconfigured device as a structured
/// [`PlaybackError::DeviceUnavailable`] before playback starts.
pub struct AlsaAudioSink {
    config: AlsaAudioSinkConfig,
    commands: Option<std_mpsc::SyncSender<AudioCommand>>,
    stop: Arc<AtomicBool>,
    worker: Option<JoinHandle<()>>,
    opened: bool,
    frames_queued: u64,
}

impl std::fmt::Debug for AlsaAudioSink {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AlsaAudioSink")
            .field("device", &self.config.device)
            .field("sample_rate", &self.config.sample_rate)
            .field("channels", &self.config.channels)
            .field("opened", &self.opened)
            .field("frames_queued", &self.frames_queued)
            .finish()
    }
}

impl AlsaAudioSink {
    /// Validate a config without opening the device.
    pub fn new(config: AlsaAudioSinkConfig) -> Result<Self, PlaybackError> {
        config.validate()?;
        Ok(Self {
            config,
            commands: None,
            stop: Arc::new(AtomicBool::new(false)),
            worker: None,
            opened: false,
            frames_queued: 0,
        })
    }

    /// Validate the config and open the device immediately.
    pub fn connect(config: AlsaAudioSinkConfig) -> Result<Self, PlaybackError> {
        let mut sink = Self::new(config)?;
        sink.open()?;
        Ok(sink)
    }

    /// ALSA device string behind this sink.
    pub fn device(&self) -> &str {
        &self.config.device
    }

    /// Number of blocks accepted by [`AudioSink::play`].
    pub fn frames_queued(&self) -> u64 {
        self.frames_queued
    }
}

impl AudioSink for AlsaAudioSink {
    fn open(&mut self) -> Result<(), PlaybackError> {
        if self.opened {
            return Ok(());
        }
        self.config.validate()?;

        let id = self.config.device.clone();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (command_tx, command_rx) = std_mpsc::sync_channel(self.config.queue_capacity.max(1));
        let stop = Arc::new(AtomicBool::new(false));

        let worker_config = self.config.clone();
        let worker_stop = stop.clone();
        let worker = std::thread::Builder::new()
            .name(format!("ozma-playback-{}", sanitize_thread_name(&id)))
            .spawn(move || play_worker(worker_config, worker_stop, ready_tx, command_rx))
            .map_err(|error| {
                PlaybackError::device_unavailable(
                    id.clone(),
                    format!("failed to start the ALSA playback worker: {error}"),
                    "Check process/thread limits and retry.",
                )
            })?;

        match ready_rx.recv_timeout(self.config.start_timeout) {
            Ok(Ok(())) => {
                self.commands = Some(command_tx);
                self.stop = stop;
                self.worker = Some(worker);
                self.opened = true;
                Ok(())
            }
            Ok(Err(error)) => {
                // The worker exits immediately after reporting a setup error.
                let _ = worker.join();
                Err(error)
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // Do not join a thread that may be wedged inside ALSA; the
                // dropped command sender asks it to exit at its next check.
                stop.store(true, Ordering::Relaxed);
                Err(PlaybackError::device_unavailable(
                    id,
                    format!(
                        "timed out after {} ms opening the device",
                        self.config.start_timeout.as_millis()
                    ),
                    "Check that the device is present and not held by another process.",
                ))
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                Err(PlaybackError::device_unavailable(
                    id,
                    "the ALSA playback worker exited during setup",
                    "Check dmesg for a USB audio reset, then retry.",
                ))
            }
        }
    }

    fn play(&mut self, audio: &DecodedAudio) -> Result<(), PlaybackError> {
        // Reject a wrong format even before a device is open: format errors do
        // not depend on the sound card, and a client can fix them cheaply.
        validate_audio_format(audio)?;
        if !self.opened {
            return Err(PlaybackError::not_open("alsa-audio"));
        }
        let Some(commands) = self.commands.as_ref() else {
            return Err(PlaybackError::not_open("alsa-audio"));
        };

        let deadline = Instant::now() + PLAY_BACKPRESSURE_TIMEOUT;
        let mut command = AudioCommand::Frame(audio.samples.clone());
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(PlaybackError::Stopped);
            }
            match commands.try_send(command) {
                Ok(()) => {
                    self.frames_queued = self.frames_queued.saturating_add(1);
                    return Ok(());
                }
                Err(std_mpsc::TrySendError::Full(returned)) => {
                    command = returned;
                    if Instant::now() >= deadline {
                        return Err(PlaybackError::busy(
                            "alsa-audio",
                            "the device did not consume queued audio in time",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std_mpsc::TrySendError::Disconnected(_)) => {
                    self.opened = false;
                    return Err(PlaybackError::device_failed(
                        &self.config.device,
                        "the playback worker is gone",
                    ));
                }
            }
        }
    }

    fn flush(&mut self) -> Result<(), PlaybackError> {
        if !self.opened {
            return Ok(());
        }
        let Some(commands) = self.commands.as_ref() else {
            return Ok(());
        };
        let (ack_tx, ack_rx) = std_mpsc::sync_channel(1);
        let mut command = AudioCommand::Flush(ack_tx);
        let deadline = Instant::now() + FLUSH_TIMEOUT;
        loop {
            if self.stop.load(Ordering::Relaxed) {
                return Err(PlaybackError::Stopped);
            }
            match commands.try_send(command) {
                Ok(()) => break,
                Err(std_mpsc::TrySendError::Full(returned)) => {
                    command = returned;
                    if Instant::now() >= deadline {
                        return Err(PlaybackError::busy(
                            "alsa-audio",
                            "the device did not accept a flush in time",
                        ));
                    }
                    std::thread::sleep(Duration::from_millis(2));
                }
                Err(std_mpsc::TrySendError::Disconnected(_)) => {
                    return Err(PlaybackError::device_failed(
                        &self.config.device,
                        "the playback worker is gone",
                    ));
                }
            }
        }
        match ack_rx.recv_timeout(FLUSH_TIMEOUT) {
            Ok(()) => Ok(()),
            Err(std_mpsc::RecvTimeoutError::Timeout) => Err(PlaybackError::busy(
                "alsa-audio",
                "the device did not drain in time",
            )),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(PlaybackError::device_failed(
                &self.config.device,
                "the playback worker stopped before flushing",
            )),
        }
    }

    fn stop(&mut self) -> Result<(), PlaybackError> {
        if !self.opened {
            return Ok(());
        }
        self.opened = false;
        self.stop.store(true, Ordering::Relaxed);

        if let Some(commands) = self.commands.take() {
            let _ = commands.send(AudioCommand::Stop);
        }
        if let Some(worker) = self.worker.take() {
            worker.join().map_err(|_| {
                PlaybackError::device_failed(
                    &self.config.device,
                    "ALSA playback worker panicked while stopping",
                )
            })?;
        }
        Ok(())
    }

    fn is_open(&self) -> bool {
        self.opened
    }
}

impl Drop for AlsaAudioSink {
    fn drop(&mut self) {
        // A bare drop must not block on a wedged sound card. Dropping the
        // sender unblocks the worker's receive, and the stop flag is checked
        // before each write; the join handle is detached.
        self.stop.store(true, Ordering::Relaxed);
        self.commands.take();
        self.worker.take();
    }
}

/// Owns the blocking PCM handle for the life of the sink.
fn play_worker(
    config: AlsaAudioSinkConfig,
    stop: Arc<AtomicBool>,
    ready: std_mpsc::SyncSender<Result<(), PlaybackError>>,
    commands: std_mpsc::Receiver<AudioCommand>,
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
        "ALSA playback worker started"
    );

    let channels = usize::from(config.channels);
    let io = match pcm.io_i16() {
        Ok(io) => io,
        Err(error) => {
            warn!(device = %device, %error, "ALSA device does not support S16 I/O");
            return;
        }
    };

    loop {
        match commands.recv() {
            Ok(AudioCommand::Frame(samples)) => {
                if stop.load(Ordering::Relaxed) {
                    break;
                }
                if let Err(error) = write_all(&pcm, &io, &device, channels, &samples) {
                    warn!(device = %device, %error, "ALSA playback failed");
                    break;
                }
            }
            Ok(AudioCommand::Flush(ack)) => {
                // Drop queued audio, but honour a stop that raced with flush.
                let mut saw_stop = false;
                loop {
                    match commands.try_recv() {
                        Ok(AudioCommand::Frame(_)) => {}
                        Ok(AudioCommand::Flush(other)) => {
                            let _ = other.send(());
                        }
                        Ok(AudioCommand::Stop) => {
                            saw_stop = true;
                            break;
                        }
                        Err(std_mpsc::TryRecvError::Empty) => break,
                        Err(std_mpsc::TryRecvError::Disconnected) => {
                            saw_stop = true;
                            break;
                        }
                    }
                }
                if let Err(error) = pcm.drain() {
                    debug!(device = %device, %error, "ALSA drain during flush failed");
                    let _ = pcm.try_recover(error, true);
                }
                if let Err(error) = pcm.prepare() {
                    warn!(device = %device, %error, "ALSA prepare after flush failed");
                }
                let _ = ack.send(());
                if saw_stop {
                    break;
                }
            }
            Ok(AudioCommand::Stop) | Err(_) => {
                if let Err(error) = pcm.drain() {
                    debug!(device = %device, %error, "ALSA drain during stop failed");
                    let _ = pcm.try_recover(error, true);
                }
                break;
            }
        }
    }

    debug!(device = %device, "ALSA playback worker stopped");
}

/// Write a complete interleaved block, recovering from an underrun once.
fn write_all(
    pcm: &PCM,
    io: &alsa::pcm::IO<'_, i16>,
    device: &str,
    channels: usize,
    samples: &[i16],
) -> Result<(), PlaybackError> {
    let mut offset = 0usize;
    while offset < samples.len() {
        match io.writei(&samples[offset..]) {
            Ok(0) => {
                return Err(PlaybackError::device_failed(
                    device,
                    "ALSA accepted zero frames",
                ));
            }
            Ok(frames) => {
                let consumed = frames.saturating_mul(channels);
                if consumed == 0 {
                    return Err(PlaybackError::device_failed(
                        device,
                        "ALSA wrote zero samples",
                    ));
                }
                offset += consumed.min(samples.len() - offset);
            }
            Err(error) => {
                if pcm.try_recover(error, true).is_ok() {
                    debug!(device = %device, "recovered from an ALSA playback error");
                    continue;
                }
                return Err(PlaybackError::device_failed(
                    device,
                    format!("write failed: {error}"),
                ));
            }
        }
    }
    Ok(())
}

/// Open and configure a playback PCM at 48 kHz stereo S16_LE.
fn open_pcm(config: &AlsaAudioSinkConfig) -> Result<PCM, PlaybackError> {
    let device = config.device.clone();
    let pcm = PCM::new(&device, Direction::Playback, false).map_err(|error| {
        PlaybackError::device_unavailable(
            device.clone(),
            format!("cannot open ALSA playback device: {error}"),
            "Check the device name with `aplay -l`, that the service user is in the audio group, and that no other process holds it.",
        )
    })?;

    let hwp = HwParams::any(&pcm).map_err(|error| {
        PlaybackError::device_unavailable(
            device.clone(),
            format!("cannot read hardware parameters: {error}"),
            "The device may not support playback; check `aplay -l`.",
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
    drop(hwp);

    // Verify the kernel accepted exactly what the decoder produces. ALSA
    // plugins may silently resample; reporting that as an unavailable device is
    // more honest than labelling audio with a rate it does not have.
    let (rate, channels, format) = {
        let current = pcm.hw_params_current().map_err(|error| {
            PlaybackError::device_unavailable(
                device.clone(),
                format!("cannot read negotiated hardware parameters: {error}"),
                "Retry opening the device.",
            )
        })?;
        (
            current.get_rate().unwrap_or(0),
            current.get_channels().unwrap_or(0),
            current.get_format().ok(),
        )
    };
    if rate != config.sample_rate
        || channels != u32::from(config.channels)
        || format != Some(Format::S16LE)
    {
        warn!(
            device = %device,
            negotiated_rate = rate,
            negotiated_channels = channels,
            ?format,
            "ALSA device did not accept 48 kHz stereo S16_LE"
        );
        return Err(PlaybackError::device_unavailable(
            device,
            format!(
                "device negotiated {rate} Hz {channels} ch {format:?}, but playback requires {} Hz {} ch S16_LE",
                config.sample_rate, config.channels
            ),
            "Use a device or plugin that supports 48 kHz stereo S16_LE, for example `plug:default`.",
        ));
    }

    pcm.prepare().map_err(|error| {
        PlaybackError::device_unavailable(
            device.clone(),
            format!("cannot prepare the playback stream: {error}"),
            "Another process may be using the device; try again once it is free.",
        )
    })?;
    Ok(pcm)
}

fn unavailable_param(device: &str, parameter: &str, error: alsa::Error) -> PlaybackError {
    PlaybackError::device_unavailable(
        device.to_owned(),
        format!("device cannot provide the required {parameter}: {error}"),
        "Verify the device supports 48 kHz stereo S16_LE playback.",
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

    fn block(sample_rate: u32, channels: u8, samples: Vec<i16>) -> DecodedAudio {
        DecodedAudio {
            pts_micros: 20_000,
            sequence: 1,
            sample_rate,
            channels,
            samples,
        }
    }

    fn canonical() -> DecodedAudio {
        block(PLAYBACK_SAMPLE_RATE, PLAYBACK_CHANNELS, vec![1, -1, 2, -2])
    }

    #[test]
    fn null_sink_records_frames_and_bytes_and_stops_idempotently() {
        let mut sink = NullAudioSink::new();
        assert!(
            sink.play(&canonical()).is_err(),
            "play before open is rejected"
        );

        sink.open().unwrap();
        sink.open().unwrap();
        assert!(sink.is_open());
        sink.play(&canonical()).unwrap();
        sink.play(&canonical()).unwrap();
        assert_eq!(sink.records().len(), 2);
        assert_eq!(sink.frames_played(), 2);
        assert_eq!(sink.samples_played(), 8);
        assert_eq!(sink.bytes_played(), 16);
        assert_eq!(sink.last().unwrap().sequence, 1);

        sink.flush().unwrap();
        sink.flush().unwrap();
        assert_eq!(sink.flushes(), 2);

        sink.stop().unwrap();
        sink.stop().unwrap();
        assert!(!sink.is_open());
        assert_eq!(sink.stop_calls(), 2);
        assert!(sink.play(&canonical()).is_err());
    }

    #[test]
    fn structurally_invalid_blocks_are_rejected() {
        let mut sink = NullAudioSink::new();
        sink.open().unwrap();
        assert_eq!(
            sink.play(&block(48_000, 2, vec![1, 2, 3])).unwrap_err(),
            PlaybackError::invalid_audio("3 samples is not a whole number of 2-channel frames")
        );
        assert!(sink.play(&block(48_000, 0, vec![1])).is_err());
        assert!(sink.play(&block(0, 2, vec![1, 2])).is_err());
        assert!(sink.play(&block(48_000, 2, Vec::new())).is_err());
        assert!(sink.records().is_empty());
    }

    #[test]
    fn the_canonical_format_is_the_ozma_audio_format() {
        assert_eq!(PLAYBACK_SAMPLE_RATE, 48_000);
        assert_eq!(PLAYBACK_CHANNELS, 2);
        validate_audio_format(&canonical()).unwrap();
    }

    #[test]
    fn wrong_audio_format_is_rejected_without_a_device() {
        // `new` does not open ALSA, so this test never needs a sound card.
        let mut sink = AlsaAudioSink::new(AlsaAudioSinkConfig::default()).unwrap();
        let error = sink
            .play(&block(44_100, 2, vec![0; 1920]))
            .expect_err("44.1 kHz must be rejected");
        assert!(matches!(error, PlaybackError::FormatMismatch { .. }));
        assert_eq!(
            error.code(),
            crate::error::playback_codes::PLAYBACK_FORMAT_MISMATCH
        );
        assert!(error.remediation().is_some());

        let error = sink
            .play(&block(48_000, 1, vec![0; 960]))
            .expect_err("mono must be rejected");
        assert!(matches!(error, PlaybackError::FormatMismatch { .. }));

        let config = AlsaAudioSinkConfig {
            sample_rate: 44_100,
            ..AlsaAudioSinkConfig::default()
        };
        assert!(matches!(
            AlsaAudioSink::new(config).unwrap_err(),
            PlaybackError::FormatMismatch { .. }
        ));
    }

    #[test]
    fn invalid_alsa_configuration_is_rejected_before_opening() {
        let config = AlsaAudioSinkConfig {
            period_frames: 0,
            ..AlsaAudioSinkConfig::default()
        };
        assert!(matches!(
            AlsaAudioSink::new(config).unwrap_err(),
            PlaybackError::DeviceUnavailable { .. }
        ));
    }

    #[test]
    fn thread_names_are_safe_and_bounded() {
        assert_eq!(sanitize_thread_name("hw:1,0"), "hw-1-0");
        assert_eq!(sanitize_thread_name("default"), "default");
        assert!(sanitize_thread_name(&"x".repeat(100)).len() <= 24);
    }

    /// A clearly invalid device name must fail fast with a structured error
    /// rather than blocking the caller.
    #[test]
    fn missing_device_returns_a_structured_error_without_hanging() {
        let config = AlsaAudioSinkConfig {
            device: "ozma-playback-no-such-device".to_owned(),
            start_timeout: Duration::from_secs(2),
            ..AlsaAudioSinkConfig::default()
        };
        let started = Instant::now();
        let error = AlsaAudioSink::connect(config).expect_err("invalid ALSA device must not open");
        let elapsed = started.elapsed();
        assert!(
            elapsed < Duration::from_secs(5),
            "opening an invalid device took {elapsed:?}"
        );
        match &error {
            PlaybackError::DeviceUnavailable { device, .. } => {
                assert_eq!(device, "ozma-playback-no-such-device");
            }
            other => panic!("expected DeviceUnavailable, got {other:?}"),
        }
        assert_eq!(
            error.code(),
            crate::error::playback_codes::PLAYBACK_DEVICE_UNAVAILABLE
        );
        assert!(error.remediation().is_some());
    }

    /// Opt-in hardware test. Run with:
    ///
    /// ```text
    /// OZMA_PLAYBACK_TEST_DEVICE=default \
    ///     cargo test -p ozma-playback -- --ignored real_alsa_playback
    /// ```
    #[test]
    #[ignore = "requires a real ALSA playback device; set OZMA_PLAYBACK_TEST_DEVICE"]
    fn real_alsa_playback_accepts_canonical_blocks() {
        let Ok(device) = std::env::var("OZMA_PLAYBACK_TEST_DEVICE") else {
            eprintln!("skipping: OZMA_PLAYBACK_TEST_DEVICE is not set");
            return;
        };
        let mut sink = AlsaAudioSink::connect(AlsaAudioSinkConfig::for_device(device))
            .expect("open configured ALSA device");
        for sequence in 0..5 {
            let audio = block(
                PLAYBACK_SAMPLE_RATE,
                PLAYBACK_CHANNELS,
                vec![(sequence as i16) * 100; 960 * 2],
            );
            sink.play(&audio).expect("queue block");
        }
        sink.flush().expect("flush");
        sink.stop().expect("stop");
        sink.stop().expect("stop is idempotent");
    }
}
