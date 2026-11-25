// SPDX-License-Identifier: AGPL-3.0-only
//! A bounded log-sweep measurement path for room correction.
//!
//! This is intentionally a *measurement* engine, not a room-correction DSP:
//! it generates a logarithmic sine sweep, plays it through one existing sink,
//! captures one existing source (normally `<sink>.monitor`), and computes a
//! basic peak/RMS/noise/impulse/RT60 summary from the captured PCM. It never
//! returns a profile when no audio was captured, and it never derives or
//! applies equalisation bands.
//!
//! The engine owns no global state. Playback and capture are behind the
//! [`SweepIo`] boundary so tests can supply deterministic sessions, and the
//! production [`SystemSweepIo`] runs `pw-play`/`paplay` plus
//! `pw-record`/`parec` directly (argv only, no shell). Children are killed on
//! completion, on error, and best-effort on cancellation via `kill_on_drop`.
//!
//! # Bounds
//!
//! | Field | Range |
//! |---|---|
//! | start/end frequency | 10 Hz to 22 kHz, end higher than start |
//! | duration | 0.5 s to 20 s |
//! | sample rate | 8 kHz to 192 kHz |
//! | capture channels | 1 or 2 |
//! | tail | 0.1 s to 5 s |
//!
//! A request outside these bounds is [`AudioError::InvalidSpec`] and no
//! process is spawned.

use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, Command};

use crate::error::AudioError;
use crate::router::{map_spawn_error, validate_token, PACTL_REMEDIATION};

/// Lowest accepted sweep frequency.
pub const MIN_SWEEP_HZ: f64 = 10.0;
/// Highest accepted sweep frequency.
pub const MAX_SWEEP_HZ: f64 = 22_000.0;
/// Shortest accepted sweep.
pub const MIN_SWEEP_DURATION_SECS: f64 = 0.5;
/// Longest accepted sweep.
pub const MAX_SWEEP_DURATION_SECS: f64 = 20.0;
/// Lowest accepted capture sample rate.
pub const MIN_SWEEP_SAMPLE_RATE: u32 = 8_000;
/// Highest accepted capture sample rate.
pub const MAX_SWEEP_SAMPLE_RATE: u32 = 192_000;
/// Highest accepted capture channel count.
pub const MAX_SWEEP_CHANNELS: u16 = 2;
/// Shortest accepted reverb tail after playback.
pub const MIN_SWEEP_TAIL_SECS: f64 = 0.1;
/// Longest accepted reverb tail after playback.
pub const MAX_SWEEP_TAIL_SECS: f64 = 5.0;
/// Largest accepted capture, in bytes, before analysis.
const MAX_CAPTURE_BYTES: u64 = 32 * 1024 * 1024;
/// Peak below this is treated as a failed/silent capture, not a measurement.
const SILENT_CAPTURE_DBFS: f32 = -60.0;

static WORKSPACE_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// A bounded sweep request.
#[derive(Clone, Debug, PartialEq)]
pub struct SweepRequest {
    /// Existing source (or `<sink>.monitor`) to capture.
    pub source: String,
    /// Existing sink to play the generated sweep through.
    pub sink: String,
    /// Sweep start frequency in Hz.
    pub start_hz: f64,
    /// Sweep end frequency in Hz.
    pub end_hz: f64,
    /// Sweep duration in seconds.
    pub duration_secs: f64,
    /// Capture/playback sample rate.
    pub sample_rate: u32,
    /// Capture channel count (the generated WAV is mono).
    pub channels: u16,
    /// Generated sweep amplitude in full scale, 0.05 to 0.95.
    pub amplitude: f64,
    /// Delay between starting capture and starting playback, in milliseconds.
    pub start_delay_ms: u64,
    /// Extra capture time after playback for the room tail.
    pub tail_secs: f64,
    /// Controller mic/profile metadata, echoed into the profile.
    pub phone_model: String,
    /// Controller target curve name, echoed into the profile.
    pub target_curve: String,
    /// Controller room label, echoed into the profile.
    pub room_name: String,
    /// Controller node id, echoed into the profile.
    pub node_id: String,
    /// Controller mic type, echoed into the profile.
    pub mic_type: String,
    /// Controller mic model, echoed into the profile.
    pub mic_model: String,
}

impl SweepRequest {
    /// Build a request with the controller's documented defaults.
    pub fn new(source: impl Into<String>, sink: impl Into<String>) -> Self {
        Self {
            source: source.into(),
            sink: sink.into(),
            start_hz: 20.0,
            end_hz: 20_000.0,
            duration_secs: 5.0,
            sample_rate: 48_000,
            channels: 1,
            amplitude: 0.5,
            start_delay_ms: 250,
            tail_secs: 0.5,
            phone_model: "generic".to_owned(),
            target_curve: "harman".to_owned(),
            room_name: String::new(),
            node_id: String::new(),
            mic_type: "phone".to_owned(),
            mic_model: String::new(),
        }
    }

    /// Validate every bound and both graph names before any process starts.
    pub fn validate(&self) -> Result<(), AudioError> {
        validate_token("sweep source", &self.source)?;
        validate_token("sweep sink", &self.sink)?;

        if !(MIN_SWEEP_HZ..=MAX_SWEEP_HZ).contains(&self.start_hz) {
            return Err(invalid(
                "start_hz",
                format!("must be between {MIN_SWEEP_HZ} and {MAX_SWEEP_HZ} Hz"),
            ));
        }
        if !(MIN_SWEEP_HZ..=MAX_SWEEP_HZ).contains(&self.end_hz) {
            return Err(invalid(
                "end_hz",
                format!("must be between {MIN_SWEEP_HZ} and {MAX_SWEEP_HZ} Hz"),
            ));
        }
        if self.end_hz <= self.start_hz {
            return Err(invalid("end_hz", "must be greater than `start_hz`"));
        }
        if !(MIN_SWEEP_DURATION_SECS..=MAX_SWEEP_DURATION_SECS).contains(&self.duration_secs) {
            return Err(invalid(
                "duration_secs",
                format!(
                    "must be between {MIN_SWEEP_DURATION_SECS} and \
                     {MAX_SWEEP_DURATION_SECS} seconds"
                ),
            ));
        }
        if !(MIN_SWEEP_SAMPLE_RATE..=MAX_SWEEP_SAMPLE_RATE).contains(&self.sample_rate) {
            return Err(invalid(
                "sample_rate",
                format!(
                    "must be between {MIN_SWEEP_SAMPLE_RATE} and \
                     {MAX_SWEEP_SAMPLE_RATE} Hz"
                ),
            ));
        }
        let nyquist = f64::from(self.sample_rate) / 2.0;
        if self.end_hz >= nyquist {
            return Err(invalid(
                "end_hz",
                format!("must stay below the Nyquist frequency ({nyquist} Hz) of the capture rate"),
            ));
        }
        if !(1..=MAX_SWEEP_CHANNELS).contains(&self.channels) {
            return Err(invalid(
                "channels",
                format!("must be between 1 and {MAX_SWEEP_CHANNELS}"),
            ));
        }
        if !(0.05..=0.95).contains(&self.amplitude) {
            return Err(invalid("amplitude", "must be between 0.05 and 0.95"));
        }
        if !(MIN_SWEEP_TAIL_SECS..=MAX_SWEEP_TAIL_SECS).contains(&self.tail_secs) {
            return Err(invalid(
                "tail_secs",
                format!("must be between {MIN_SWEEP_TAIL_SECS} and {MAX_SWEEP_TAIL_SECS} seconds"),
            ));
        }
        if self.start_delay_ms > 2_000 {
            return Err(invalid("start_delay_ms", "must be at most 2000"));
        }
        for (field, value) in [
            ("phone_model", &self.phone_model),
            ("target_curve", &self.target_curve),
            ("room_name", &self.room_name),
            ("node_id", &self.node_id),
            ("mic_type", &self.mic_type),
            ("mic_model", &self.mic_model),
        ] {
            if value.chars().count() > 128 {
                return Err(invalid(field, "must be at most 128 characters"));
            }
            if value.chars().any(char::is_control) {
                return Err(invalid(field, "must not contain control characters"));
            }
        }
        Ok(())
    }

    /// Total capture time: startup delay, sweep, and tail.
    pub fn capture_secs(&self) -> f64 {
        self.start_delay_ms as f64 / 1000.0 + self.duration_secs + self.tail_secs
    }

    /// Expected raw S16 capture size, bounded.
    fn expected_capture_bytes(&self) -> u64 {
        let samples = self.capture_secs() * f64::from(self.sample_rate);
        let bytes = (samples * f64::from(self.channels) * 2.0).ceil() as u64;
        bytes.min(MAX_CAPTURE_BYTES)
    }
}

/// One parametric EQ band in the profile shape the controller expects.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepBand {
    /// Centre frequency in Hz.
    pub freq: f64,
    /// Gain in dB.
    pub gain: f64,
    /// Q factor.
    pub q: f64,
    /// `peaking`, `low_shelf`, or `high_shelf`.
    #[serde(rename = "type")]
    pub band_type: String,
}

/// The generated sweep's parameters.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepDescriptor {
    /// Start frequency in Hz.
    pub start_hz: f64,
    /// End frequency in Hz.
    pub end_hz: f64,
    /// Duration in seconds.
    pub duration_secs: f64,
}

/// Basic, honest measurement derived from captured PCM.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepMeasurement {
    /// Capture sample rate.
    pub sample_rate: u32,
    /// Capture channel count.
    pub channels: u16,
    /// Captured frames.
    pub captured_frames: u64,
    /// Captured duration in seconds.
    pub captured_secs: f64,
    /// Peak absolute sample, normalized to full scale.
    pub peak: f32,
    /// Peak in dBFS.
    pub peak_dbfs: f32,
    /// RMS over the capture in dBFS.
    pub rms_dbfs: f32,
    /// Noise-floor estimate in dBFS (quietest 10% of 20 ms windows).
    pub noise_floor_dbfs: f32,
    /// Absolute impulse peak, normalized to full scale.
    pub impulse_peak: f32,
    /// Frame index of the peak.
    pub impulse_index: u64,
    /// Time of the peak from capture start, in seconds.
    pub impulse_index_secs: f64,
    /// Estimated RT60 in seconds, when the decay spans enough range.
    pub rt60_secs: Option<f32>,
    /// Fitted decay slope in dB per second, when available.
    pub decay_db_per_sec: Option<f32>,
    /// The generated sweep that produced this capture.
    pub sweep: SweepDescriptor,
}

/// The controller-compatible profile subset returned by a sweep.
///
/// `bands` is always empty: this engine measures, it does not fit or apply EQ.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepProfile {
    /// Stable id derived from the measurement time.
    pub id: String,
    /// Human name, from `room_name` when supplied.
    pub name: String,
    /// Unix timestamp in seconds.
    pub created_at: f64,
    /// Empty: no EQ fitting is performed.
    pub bands: Vec<SweepBand>,
    /// Equal to `bands.len()`.
    pub band_count: usize,
    /// Controller mic type, echoed.
    pub mic_type: String,
    /// Controller phone model, echoed.
    pub phone_model: String,
    /// Controller mic model, echoed.
    pub mic_model: String,
    /// Controller target curve, echoed.
    pub target_curve: String,
    /// Controller room label, echoed.
    pub room_name: String,
    /// Controller node id, echoed.
    pub node_id: String,
    /// Always false: nothing was applied to the graph.
    pub active: bool,
    /// The measured response.
    pub measurement: SweepMeasurement,
}

/// A successful sweep.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepOutcome {
    /// The measured profile.
    pub profile: SweepProfile,
}

/// One capture session, already running when returned.
#[async_trait]
pub trait CaptureSession: Send {
    /// Stop capture, reap the process, and return captured raw S16 bytes.
    async fn finish(self: Box<Self>) -> Result<Vec<u8>, AudioError>;
    /// Stop and reap without returning audio.
    async fn cancel(self: Box<Self>);
}

/// One playback session, already running when returned.
#[async_trait]
pub trait PlaybackSession: Send {
    /// Wait for playback to finish, bounded by `timeout`.
    async fn wait(self: Box<Self>, timeout: Duration) -> Result<(), AudioError>;
    /// Stop and reap without requiring success.
    async fn cancel(self: Box<Self>);
}

/// The injectable playback/capture boundary for a sweep.
#[async_trait]
pub trait SweepIo: Send + Sync {
    /// Start capturing `request.source` into raw S16 PCM.
    async fn start_capture(
        &self,
        request: &SweepRequest,
        capture_secs: f64,
    ) -> Result<Box<dyn CaptureSession>, AudioError>;

    /// Start playing `wav` to `request.sink`.
    async fn start_playback(
        &self,
        request: &SweepRequest,
        wav: &Path,
    ) -> Result<Box<dyn PlaybackSession>, AudioError>;
}

/// Production playback/capture using the host's PulseAudio-compatible tools.
pub struct SystemSweepIo {
    playback_program: &'static str,
    capture_program: &'static str,
}

impl SystemSweepIo {
    /// PipeWire tooling: `pw-play` and `pw-record`.
    pub fn pipewire() -> Self {
        Self {
            playback_program: "pw-play",
            capture_program: "pw-record",
        }
    }

    /// PulseAudio tooling: `paplay` and `parec`.
    pub fn pulseaudio() -> Self {
        Self {
            playback_program: "paplay",
            capture_program: "parec",
        }
    }
}

#[async_trait]
impl SweepIo for SystemSweepIo {
    async fn start_capture(
        &self,
        request: &SweepRequest,
        capture_secs: f64,
    ) -> Result<Box<dyn CaptureSession>, AudioError> {
        let mut argv = vec![self.capture_program.to_owned()];
        if self.capture_program == "pw-record" {
            argv.extend([
                "--target".to_owned(),
                request.source.clone(),
                "--rate".to_owned(),
                request.sample_rate.to_string(),
                "--channels".to_owned(),
                request.channels.to_string(),
                "--format".to_owned(),
                "s16".to_owned(),
                "--raw".to_owned(),
                "-".to_owned(),
            ]);
        } else {
            argv.extend([
                format!("--device={}", request.source),
                "--format=s16le".to_owned(),
                format!("--rate={}", request.sample_rate),
                format!("--channels={}", request.channels),
                "--latency-msec=50".to_owned(),
                "--raw".to_owned(),
            ]);
        }

        let program = argv[0].clone();
        let mut child = Command::new(&program)
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| map_spawn_error(&program, error))?;

        let expected = request.expected_capture_bytes();
        let max_wait = Duration::from_secs_f64(capture_secs.clamp(1.0, 30.0) + 5.0);
        let task = tokio::spawn(async move {
            let Some(mut stdout) = child.stdout.take() else {
                return Err(AudioError::Capture {
                    device: "sweep capture".to_owned(),
                    message: "capture process has no stdout pipe".to_owned(),
                });
            };
            let mut buffer = Vec::with_capacity((expected as usize).min(1024 * 1024));
            let read = tokio::time::timeout(max_wait, async {
                let mut limited = (&mut stdout).take(expected);
                limited.read_to_end(&mut buffer).await
            })
            .await;
            let _ = child.kill().await;
            let _ = child.wait().await;
            match read {
                Ok(Ok(_)) => Ok(buffer),
                Ok(Err(error)) => Err(AudioError::Capture {
                    device: "sweep capture".to_owned(),
                    message: error.to_string(),
                }),
                // A timeout still returns whatever arrived; the caller checks
                // the length and never claims a measurement from silence.
                Err(_) => Ok(buffer),
            }
        });

        Ok(Box::new(SystemCaptureSession { task: Some(task) }))
    }

    async fn start_playback(
        &self,
        request: &SweepRequest,
        wav: &Path,
    ) -> Result<Box<dyn PlaybackSession>, AudioError> {
        let wav = wav.to_string_lossy().into_owned();
        let argv = if self.playback_program == "pw-play" {
            vec![
                self.playback_program.to_owned(),
                "--target".to_owned(),
                request.sink.clone(),
                wav,
            ]
        } else {
            vec![
                self.playback_program.to_owned(),
                format!("--device={}", request.sink),
                wav,
            ]
        };
        let program = argv[0].clone();
        let child = Command::new(&program)
            .args(&argv[1..])
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| map_spawn_error(&program, error))?;
        Ok(Box::new(SystemPlaybackSession { child }))
    }
}

struct SystemCaptureSession {
    task: Option<tokio::task::JoinHandle<Result<Vec<u8>, AudioError>>>,
}

impl Drop for SystemCaptureSession {
    fn drop(&mut self) {
        if let Some(task) = self.task.take() {
            task.abort();
        }
    }
}

#[async_trait]
impl CaptureSession for SystemCaptureSession {
    async fn finish(mut self: Box<Self>) -> Result<Vec<u8>, AudioError> {
        let Some(task) = self.task.take() else {
            return Err(AudioError::Capture {
                device: "sweep capture".to_owned(),
                message: "capture session was already finished".to_owned(),
            });
        };
        task.await.map_err(|error| AudioError::Capture {
            device: "sweep capture".to_owned(),
            message: format!("capture task failed: {error}"),
        })?
    }

    async fn cancel(mut self: Box<Self>) {
        if let Some(task) = self.task.take() {
            task.abort();
            let _ = task.await;
        }
    }
}

struct SystemPlaybackSession {
    child: Child,
}

#[async_trait]
impl PlaybackSession for SystemPlaybackSession {
    async fn wait(mut self: Box<Self>, timeout: Duration) -> Result<(), AudioError> {
        match tokio::time::timeout(timeout, self.child.wait()).await {
            Ok(Ok(status)) if status.success() => Ok(()),
            Ok(Ok(status)) => Err(AudioError::CommandFailed {
                command: "sweep playback".to_owned(),
                status: status.code().unwrap_or(-1),
                stderr: String::new(),
            }),
            Ok(Err(error)) => Err(AudioError::Process {
                command: "sweep playback".to_owned(),
                message: error.to_string(),
            }),
            Err(_) => {
                let _ = self.child.kill().await;
                let _ = self.child.wait().await;
                Err(AudioError::Process {
                    command: "sweep playback".to_owned(),
                    message: format!("timed out after {}s", timeout.as_secs()),
                })
            }
        }
    }

    async fn cancel(mut self: Box<Self>) {
        let _ = self.child.kill().await;
        let _ = self.child.wait().await;
    }
}

/// Run one sweep through `io`, returning a measured profile.
///
/// The sequence is: validate, generate a WAV in a private temporary directory,
/// start capture, wait for the configured startup delay, play the sweep, wait
/// for playback, collect the tail, then analyse. Any transport failure is a
/// typed error; a short or silent capture is [`AudioError::Capture`], never a
/// zeroed or invented profile.
pub async fn run_sweep(
    request: SweepRequest,
    io: &dyn SweepIo,
) -> Result<SweepOutcome, AudioError> {
    request.validate()?;
    let workspace = Workspace::new()?;
    let wav_path = workspace.path().join("sweep.wav");
    write_sweep_wav(&wav_path, &request)?;

    let capture = io.start_capture(&request, request.capture_secs()).await?;
    if request.start_delay_ms > 0 {
        tokio::time::sleep(Duration::from_millis(request.start_delay_ms)).await;
    }

    let playback = match io.start_playback(&request, &wav_path).await {
        Ok(playback) => playback,
        Err(error) => {
            capture.cancel().await;
            return Err(error);
        }
    };

    let played = playback
        .wait(Duration::from_secs_f64(
            request.duration_secs + request.tail_secs + 5.0,
        ))
        .await;
    if let Err(error) = played {
        capture.cancel().await;
        return Err(error);
    }

    let captured = capture.finish().await?;
    let measurement = analyse_capture(&captured, &request)?;
    Ok(SweepOutcome {
        profile: profile_for(&request, measurement),
    })
}

fn profile_for(request: &SweepRequest, measurement: SweepMeasurement) -> SweepProfile {
    let created_at = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_secs_f64())
        .unwrap_or(0.0);
    let id = format!("sweep-{}", (created_at * 1000.0) as u64);
    let name = if request.room_name.trim().is_empty() {
        format!("Room sweep {}", (created_at * 1000.0) as u64)
    } else {
        request.room_name.trim().to_owned()
    };
    SweepProfile {
        id,
        name,
        created_at,
        bands: Vec::new(),
        band_count: 0,
        mic_type: request.mic_type.clone(),
        phone_model: request.phone_model.clone(),
        mic_model: request.mic_model.clone(),
        target_curve: request.target_curve.clone(),
        room_name: request.room_name.clone(),
        node_id: request.node_id.clone(),
        active: false,
        measurement,
    }
}

fn analyse_capture(bytes: &[u8], request: &SweepRequest) -> Result<SweepMeasurement, AudioError> {
    let channels = usize::from(request.channels.max(1));
    let frame_bytes = channels * 2;
    let frames = bytes.len() / frame_bytes;
    if frames == 0 {
        return Err(AudioError::Capture {
            device: request.source.clone(),
            message: "capture returned no audio frames".to_owned(),
        });
    }

    let mut samples = Vec::with_capacity(frames);
    for frame in bytes.chunks_exact(frame_bytes) {
        let mut sum = 0.0f32;
        for channel in 0..channels {
            let offset = channel * 2;
            let sample = i16::from_le_bytes([frame[offset], frame[offset + 1]]);
            sum += f32::from(sample) / 32_768.0;
        }
        samples.push(sum / channels as f32);
    }

    let mut peak = 0.0f32;
    let mut peak_index = 0usize;
    let mut energy = 0.0f64;
    for (index, sample) in samples.iter().enumerate() {
        let magnitude = sample.abs();
        if magnitude > peak {
            peak = magnitude;
            peak_index = index;
        }
        energy += f64::from(*sample) * f64::from(*sample);
    }
    let peak_dbfs = dbfs(peak);
    if peak_dbfs < SILENT_CAPTURE_DBFS {
        return Err(AudioError::Capture {
            device: request.source.clone(),
            message: format!(
                "captured {} frames but the peak was {:.1} dBFS; no measurement is claimed",
                samples.len(),
                peak_dbfs
            ),
        });
    }

    let rms = (energy / samples.len() as f64).sqrt() as f32;
    let rms_dbfs = dbfs(rms);
    let window = (f64::from(request.sample_rate) * 0.02).round().max(1.0) as usize;
    let windows = window_dbfs(&samples, window);
    let noise_floor_dbfs = noise_floor(&windows).unwrap_or(-120.0);
    let (rt60_secs, decay_db_per_sec) = decay_estimate(
        &windows,
        peak_index / window,
        f64::from(request.sample_rate) / window as f64,
    );

    let captured_secs = samples.len() as f64 / f64::from(request.sample_rate);
    Ok(SweepMeasurement {
        sample_rate: request.sample_rate,
        channels: request.channels,
        captured_frames: samples.len() as u64,
        captured_secs,
        peak,
        peak_dbfs,
        rms_dbfs,
        noise_floor_dbfs,
        impulse_peak: peak,
        impulse_index: peak_index as u64,
        impulse_index_secs: peak_index as f64 / f64::from(request.sample_rate),
        rt60_secs,
        decay_db_per_sec,
        sweep: SweepDescriptor {
            start_hz: request.start_hz,
            end_hz: request.end_hz,
            duration_secs: request.duration_secs,
        },
    })
}

fn window_dbfs(samples: &[f32], window: usize) -> Vec<f32> {
    samples
        .chunks(window)
        .filter(|chunk| !chunk.is_empty())
        .map(|chunk| {
            let energy = chunk
                .iter()
                .map(|sample| f64::from(*sample) * f64::from(*sample))
                .sum::<f64>();
            dbfs((energy / chunk.len() as f64).sqrt() as f32)
        })
        .collect()
}

fn noise_floor(windows: &[f32]) -> Option<f32> {
    if windows.is_empty() {
        return None;
    }
    let mut sorted = windows.to_vec();
    sorted.sort_by(|a, b| a.total_cmp(b));
    let count = (sorted.len() / 10).max(1);
    Some(sorted[..count].iter().sum::<f32>() / count as f32)
}

fn decay_estimate(
    windows: &[f32],
    peak_window: usize,
    windows_per_sec: f64,
) -> (Option<f32>, Option<f32>) {
    if windows.len() < 3 || peak_window >= windows.len() {
        return (None, None);
    }
    let peak = windows[peak_window];
    if !peak.is_finite() {
        return (None, None);
    }
    let start = (peak_window..windows.len()).find(|index| windows[*index] <= peak - 5.0);
    let Some(start) = start else {
        return (None, None);
    };
    let end = (start..windows.len()).find(|index| windows[*index] <= peak - 35.0);
    let Some(end) = end else {
        return (None, None);
    };
    if end <= start + 2 {
        return (None, None);
    }

    let values = &windows[start..=end];
    let n = values.len() as f64;
    let mean_x = (n - 1.0) / 2.0;
    let mean_y = values.iter().map(|value| f64::from(*value)).sum::<f64>() / n;
    let mut numerator = 0.0;
    let mut denominator = 0.0;
    for (index, value) in values.iter().enumerate() {
        let x = index as f64 - mean_x;
        numerator += x * (f64::from(*value) - mean_y);
        denominator += x * x;
    }
    if denominator == 0.0 || numerator >= 0.0 {
        return (None, None);
    }
    let db_per_window = numerator / denominator;
    let db_per_sec = db_per_window * windows_per_sec;
    if !db_per_sec.is_finite() || db_per_sec >= 0.0 {
        return (None, None);
    }
    let rt60 = (-60.0 / db_per_sec).clamp(0.0, 10.0) as f32;
    (
        Some((rt60 * 1000.0).round() / 1000.0),
        Some((db_per_sec * 10.0).round() as f32 / 10.0),
    )
}

fn dbfs(value: f32) -> f32 {
    if value <= 0.0 {
        -120.0
    } else {
        (20.0 * value.log10()).max(-120.0)
    }
}

fn write_sweep_wav(path: &Path, request: &SweepRequest) -> Result<(), AudioError> {
    let sample_rate = request.sample_rate;
    let frames = (f64::from(sample_rate) * request.duration_secs).round() as u32;
    let k = (request.end_hz / request.start_hz).ln();
    let fade_frames = (f64::from(sample_rate) * 0.05).round() as u32;
    let bytes = frames.saturating_mul(2);

    let file = std::fs::File::create(path).map_err(|error| AudioError::Process {
        command: path.display().to_string(),
        message: error.to_string(),
    })?;
    let mut writer = BufWriter::new(file);

    let write = |writer: &mut BufWriter<std::fs::File>, data: &[u8]| {
        writer.write_all(data).map_err(|error| AudioError::Process {
            command: path.display().to_string(),
            message: error.to_string(),
        })
    };

    write(&mut writer, b"RIFF")?;
    write(&mut writer, &(36 + bytes).to_le_bytes())?;
    write(&mut writer, b"WAVEfmt ")?;
    write(&mut writer, &16u32.to_le_bytes())?;
    write(&mut writer, &1u16.to_le_bytes())?;
    write(&mut writer, &1u16.to_le_bytes())?;
    write(&mut writer, &sample_rate.to_le_bytes())?;
    write(&mut writer, &(sample_rate * 2).to_le_bytes())?;
    write(&mut writer, &2u16.to_le_bytes())?;
    write(&mut writer, &16u16.to_le_bytes())?;
    write(&mut writer, b"data")?;
    write(&mut writer, &bytes.to_le_bytes())?;

    for index in 0..frames {
        let t = f64::from(index) / f64::from(sample_rate);
        let phase = 2.0 * std::f64::consts::PI * request.start_hz * request.duration_secs / k
            * ((k * t / request.duration_secs).exp() - 1.0);
        let mut envelope = 1.0;
        if index < fade_frames {
            envelope = f64::from(index) / f64::from(fade_frames.max(1));
        } else if index + fade_frames > frames {
            envelope = f64::from(frames - index) / f64::from(fade_frames.max(1));
        }
        let sample = phase.sin() * request.amplitude * envelope.clamp(0.0, 1.0);
        let value = (sample * 32_000.0).round().clamp(-32_768.0, 32_767.0) as i16;
        write(&mut writer, &value.to_le_bytes())?;
    }
    writer.flush().map_err(|error| AudioError::Process {
        command: path.display().to_string(),
        message: error.to_string(),
    })?;
    Ok(())
}

struct Workspace {
    path: PathBuf,
}

impl Workspace {
    fn new() -> Result<Self, AudioError> {
        let sequence = WORKSPACE_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ozma-sweep-{}-{}-{}",
            std::process::id(),
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|duration| duration.as_nanos())
                .unwrap_or(0),
            sequence
        ));
        std::fs::create_dir_all(&path).map_err(|error| AudioError::Process {
            command: path.display().to_string(),
            message: format!("could not create sweep workspace: {error}"),
        })?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

fn invalid(field: &str, reason: impl Into<String>) -> AudioError {
    AudioError::InvalidSpec {
        field: format!("sweep {field}"),
        reason: reason.into(),
    }
}

/// A typed unsupported error for backends that cannot run a sweep.
pub fn sweep_unsupported() -> AudioError {
    AudioError::Unsupported {
        feature: "room-correction sweep".to_owned(),
        remediation: format!(
            "Use a router backed by a PulseAudio-compatible server with pw-play/pw-record or \
             paplay/parec installed ({PACTL_REMEDIATION}). No capture or playback was started."
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, AtomicUsize};
    use std::sync::Arc;

    struct FakeIo {
        capture: Vec<u8>,
        fail_playback: bool,
        capture_started: Arc<AtomicUsize>,
        capture_cancelled: Arc<AtomicBool>,
        playback_cancelled: Arc<AtomicBool>,
    }

    impl FakeIo {
        fn new(capture: Vec<u8>) -> Self {
            Self {
                capture,
                fail_playback: false,
                capture_started: Arc::new(AtomicUsize::new(0)),
                capture_cancelled: Arc::new(AtomicBool::new(false)),
                playback_cancelled: Arc::new(AtomicBool::new(false)),
            }
        }
    }

    struct FakeCapture {
        data: Vec<u8>,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait]
    impl CaptureSession for FakeCapture {
        async fn finish(self: Box<Self>) -> Result<Vec<u8>, AudioError> {
            Ok(self.data)
        }

        async fn cancel(self: Box<Self>) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    struct FakePlayback {
        fail: bool,
        cancelled: Arc<AtomicBool>,
    }

    #[async_trait]
    impl PlaybackSession for FakePlayback {
        async fn wait(self: Box<Self>, _timeout: Duration) -> Result<(), AudioError> {
            if self.fail {
                Err(AudioError::CommandFailed {
                    command: "fake-play".to_owned(),
                    status: 1,
                    stderr: "no sink".to_owned(),
                })
            } else {
                Ok(())
            }
        }

        async fn cancel(self: Box<Self>) {
            self.cancelled.store(true, Ordering::SeqCst);
        }
    }

    #[async_trait]
    impl SweepIo for FakeIo {
        async fn start_capture(
            &self,
            _request: &SweepRequest,
            _capture_secs: f64,
        ) -> Result<Box<dyn CaptureSession>, AudioError> {
            self.capture_started.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeCapture {
                data: self.capture.clone(),
                cancelled: self.capture_cancelled.clone(),
            }))
        }

        async fn start_playback(
            &self,
            _request: &SweepRequest,
            _wav: &Path,
        ) -> Result<Box<dyn PlaybackSession>, AudioError> {
            Ok(Box::new(FakePlayback {
                fail: self.fail_playback,
                cancelled: self.playback_cancelled.clone(),
            }))
        }
    }

    fn capture_with_decay(sample_rate: u32) -> Vec<u8> {
        let mut samples = Vec::new();
        let frames = sample_rate as usize;
        for index in 0..frames {
            let t = index as f32 / sample_rate as f32;
            let amplitude = if index < 100 {
                0.0
            } else {
                0.5 * (-8.0 * (t - 0.1)).exp()
            };
            let sample =
                (amplitude * (2.0 * std::f32::consts::PI * 440.0 * t).sin() * 32_000.0) as i16;
            samples.extend_from_slice(&sample.to_le_bytes());
        }
        samples
    }

    fn test_request() -> SweepRequest {
        let mut request = SweepRequest::new("speakers.monitor", "speakers");
        request.start_delay_ms = 0;
        request.duration_secs = 0.5;
        request.tail_secs = 0.5;
        request
    }

    #[tokio::test]
    async fn sweep_reports_a_measurement_only_when_audio_was_captured() {
        let io = FakeIo::new(capture_with_decay(48_000));
        let outcome = run_sweep(test_request(), &io)
            .await
            .expect("measured sweep");
        let measurement = &outcome.profile.measurement;
        assert!(measurement.captured_frames > 40_000);
        assert!(measurement.peak > 0.4);
        assert!(measurement.peak_dbfs > -10.0);
        assert!(measurement.noise_floor_dbfs < measurement.peak_dbfs);
        assert!(measurement.rt60_secs.is_some());
        assert!(outcome.profile.id.starts_with("sweep-"));
        assert!(outcome.profile.bands.is_empty());
        assert_eq!(outcome.profile.band_count, 0);
        assert!(!outcome.profile.active);
        assert_eq!(io.capture_started.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn silent_capture_is_a_typed_error_not_a_zeroed_profile() {
        let io = FakeIo::new(vec![0; 48_000 * 2]);
        let error = run_sweep(test_request(), &io).await.unwrap_err();
        assert!(matches!(error, AudioError::Capture { .. }));
    }

    #[tokio::test]
    async fn empty_capture_is_a_typed_error() {
        let io = FakeIo::new(Vec::new());
        let error = run_sweep(test_request(), &io).await.unwrap_err();
        assert!(matches!(error, AudioError::Capture { .. }));
    }

    #[tokio::test]
    async fn playback_failure_cancels_capture_and_returns_the_playback_error() {
        let mut io = FakeIo::new(capture_with_decay(48_000));
        io.fail_playback = true;
        let error = run_sweep(test_request(), &io).await.unwrap_err();
        assert!(matches!(error, AudioError::CommandFailed { .. }));
        assert!(io.capture_cancelled.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn invalid_bounds_are_rejected_before_any_io() {
        let io = FakeIo::new(capture_with_decay(48_000));
        let mut request = test_request();
        request.end_hz = 30_000.0;
        let error = run_sweep(request, &io).await.unwrap_err();
        assert!(matches!(error, AudioError::InvalidSpec { .. }));
        assert_eq!(io.capture_started.load(Ordering::SeqCst), 0);

        let mut request = test_request();
        request.sample_rate = 192_000;
        request.end_hz = 22_000.0;
        assert!(run_sweep(request, &io).await.is_ok());
    }

    #[test]
    fn generated_wav_has_a_bounded_log_sweep_header() {
        let workspace = Workspace::new().unwrap();
        let path = workspace.path().join("sweep.wav");
        write_sweep_wav(&path, &test_request()).unwrap();
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(&bytes[..4], b"RIFF");
        assert_eq!(&bytes[8..12], b"WAVE");
        let rate = u32::from_le_bytes(bytes[24..28].try_into().unwrap());
        let channels = u16::from_le_bytes(bytes[22..24].try_into().unwrap());
        let data = u32::from_le_bytes(bytes[40..44].try_into().unwrap());
        assert_eq!(rate, 48_000);
        assert_eq!(channels, 1);
        assert_eq!(data, 48_000);
    }

    #[test]
    fn validation_bounds_are_documented_and_enforced() {
        let mut request = test_request();
        request.start_hz = 5.0;
        assert!(matches!(
            request.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));
        let mut request = test_request();
        request.duration_secs = 0.1;
        assert!(matches!(
            request.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));
        let mut request = test_request();
        request.source = "bad source".to_owned();
        assert!(matches!(
            request.validate(),
            Err(AudioError::InvalidSpec { .. })
        ));
        assert!(super::tests::test_request().validate().is_ok());
    }
}
