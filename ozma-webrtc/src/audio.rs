// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Optional native Opus audio for the native WebRTC helper.
//!
//! Audio is opt-in: without `--audio-source` this module is not started and an
//! offered Opus m-line is answered `inactive`. With a source, the helper opens
//! either a native PipeWire capture stream (`pipewire`/`auto`) or a small
//! `parec` capture process (`pulse`), converts mono/stereo input to interleaved
//! stereo, resamples to 48 kHz with a bounded linear resampler, assembles exact
//! 20 ms frames, encodes them with libopus through `audiopus`, and writes them
//! to one shared `TrackLocalStaticSample`.
//!
//! No default source is opened implicitly. A named PipeWire target is checked
//! against the graph before opening; an unknown name is an error rather than a
//! silent fall back to the session default (which could be a microphone).
//!
//! Queues are bounded at every hand-off: the capture stream keeps eight
//! frames, the reframer holds at most four 20 ms frames, and encoded packets
//! pass through an eight-packet channel. A full queue drops the newest frame
//! and increments a counter.

use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use audiopus::coder::Encoder as OpusEncoder;
use audiopus::{Application, Bitrate, Channels as OpusChannels, SampleRate};
use tokio::io::AsyncReadExt;
use tokio::process::{Child, ChildStdout, Command};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};
use webrtc::api::media_engine::MIME_TYPE_OPUS;
use webrtc::media::Sample;
use webrtc::rtp_transceiver::rtp_codec::RTCRtpCodecCapability;
use webrtc::track::track_local::track_local_static_sample::TrackLocalStaticSample;

use crate::cli::AudioBackend;

/// Opus sample rate. Opus is defined at 48 kHz and everything is resampled to
/// this rate before encoding.
pub const SAMPLE_RATE: u32 = 48_000;
/// Opus channels. Mono capture is duplicated into both channels.
pub const CHANNELS: u8 = 2;
/// One Opus packet covers 20 ms.
pub const FRAME_MICROS: u64 = 20_000;
/// Samples per channel in one 20 ms frame at [`SAMPLE_RATE`].
pub const FRAME_SAMPLES: usize = 960;
/// Bounded encoder bitrate. Signaling bitrate changes remain video-only.
pub const AUDIO_BITRATE_BPS: i32 = 128_000;
/// Hard upper bound for one Opus packet (libopus max is 1275 bytes).
pub const MAX_PACKET_BYTES: usize = 4_000;
/// Encoded packets waiting for the RTP writer.
pub const PACKET_QUEUE_CAPACITY: usize = 8;
/// Maximum interleaved samples held before the encoder.
pub const MAX_PENDING_SAMPLES: usize = FRAME_SAMPLES * CHANNELS as usize * 4;
/// Capture frames buffered inside the PipeWire stream.
pub const CAPTURE_QUEUE_CAPACITY: usize = 8;
/// How long a named target waits for graph validation.
const TARGET_PROBE_TIMEOUT: Duration = Duration::from_secs(2);
/// How long a Pulse source may take to deliver the first 20 ms chunk.
const PULSE_START_TIMEOUT: Duration = Duration::from_secs(5);
/// How long a running Pulse source may go without data before it is an error.
const PULSE_READ_TIMEOUT: Duration = Duration::from_secs(5);

/// The audio source selection from the helper argv.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AudioConfig {
    pub source: String,
    pub backend: AudioBackend,
}

/// Where a PipeWire capture stream should connect.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Target {
    Default,
    Object(String),
    NodeId(u32),
}

impl Target {
    fn stream_target(&self) -> ozma_pipewire::StreamTarget {
        match self {
            Self::Default => ozma_pipewire::StreamTarget::Default,
            Self::Object(name) => ozma_pipewire::StreamTarget::Object(name.clone()),
            Self::NodeId(id) => ozma_pipewire::StreamTarget::NodeId(*id),
        }
    }
}

/// Parse the node's source token into a PipeWire target.
///
/// The node passes monitor names as bare `<sink>.monitor` strings, while the
/// `ozma-audio` vocabulary uses `monitor:<sink>`, `object:<name>`, and
/// `node:<id>`. Both forms are accepted. The bare `default` token is honoured
/// only when the operator supplied it explicitly.
fn parse_target(source: &str) -> Result<Target, String> {
    let raw = source.trim();
    if raw.is_empty() {
        return Err("audio source must not be empty".to_owned());
    }
    if raw.chars().any(char::is_control) {
        return Err("audio source must not contain control characters".to_owned());
    }
    if raw == "default" {
        return Ok(Target::Default);
    }
    if let Some(sink) = raw.strip_prefix("monitor:") {
        if sink.trim().is_empty() {
            return Err("monitor source must name a sink".to_owned());
        }
        return Ok(Target::Object(format!("{sink}.monitor")));
    }
    if let Some(name) = raw.strip_prefix("object:") {
        if name.trim().is_empty() {
            return Err("object source must name a PipeWire object".to_owned());
        }
        return Ok(Target::Object(name.to_owned()));
    }
    if let Some(id) = raw.strip_prefix("node:") {
        let id = id
            .parse::<u32>()
            .map_err(|_| format!("node source `{raw}` must be a u32 node id"))?;
        return Ok(Target::NodeId(id));
    }
    Ok(Target::Object(raw.to_owned()))
}

/// The PulseAudio device name for a source token.
fn pulse_source(source: &str) -> Result<String, String> {
    let raw = source.trim();
    if raw.is_empty() {
        return Err("audio source must not be empty".to_owned());
    }
    if raw.chars().any(char::is_control) {
        return Err("audio source must not contain control characters".to_owned());
    }
    if let Some(sink) = raw.strip_prefix("monitor:") {
        if sink.trim().is_empty() {
            return Err("monitor source must name a sink".to_owned());
        }
        return Ok(format!("{sink}.monitor"));
    }
    if let Some(name) = raw.strip_prefix("object:") {
        if name.trim().is_empty() {
            return Err("object source must name a PipeWire object".to_owned());
        }
        return Ok(name.to_owned());
    }
    Ok(raw.to_owned())
}

/// Check that a named PipeWire target exists before opening it.
///
/// This is deliberately strict: PipeWire's `target.object` is advisory, and a
/// typo could otherwise end up on the default source. A helper must never
/// silently capture a microphone the operator did not name.
async fn verify_target(target: &Target) -> Result<(), String> {
    if matches!(target, Target::Default) {
        return Ok(());
    }
    let status = ozma_pipewire::probe(TARGET_PROBE_TIMEOUT)
        .await
        .map_err(|error| format!("could not inspect the PipeWire graph: {error}"))?;
    match target {
        Target::Default => Ok(()),
        Target::Object(name) => {
            let present = status.sources.iter().any(|node| node.name == *name)
                || status.sinks.iter().any(|node| node.name == *name)
                || status
                    .sinks
                    .iter()
                    .any(|node| format!("{}.monitor", node.name) == *name);
            if present {
                Ok(())
            } else {
                Err(format!(
                    "audio source `{name}` is not present in the PipeWire graph; \
                     refusing to fall back to the default source"
                ))
            }
        }
        Target::NodeId(id) => {
            let present = status.sources.iter().any(|node| node.id == *id)
                || status.sinks.iter().any(|node| node.id == *id);
            if present {
                Ok(())
            } else {
                Err(format!("PipeWire node {id} is not an audio source or sink"))
            }
        }
    }
}

/// One arbitrary-size block of interleaved S16 samples from a capture source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Chunk {
    pts_micros: u64,
    sample_rate: u32,
    channels: u8,
    samples: Vec<i16>,
}

enum Capture {
    PipeWire(ozma_pipewire::PipeWireAudioSource),
    Pulse(PulseCapture),
    #[cfg(test)]
    Test(TestCapture),
}

impl Capture {
    async fn open(config: &AudioConfig) -> Result<(Self, String), String> {
        match config.backend {
            AudioBackend::Pulse => {
                let name = pulse_source(&config.source)?;
                let capture = PulseCapture::open(&name).await?;
                Ok((Self::Pulse(capture), format!("pulse `{name}`")))
            }
            AudioBackend::PipeWire => {
                let target = parse_target(&config.source)?;
                verify_target(&target).await?;
                let capture = open_pipewire(&target).await?;
                Ok((
                    Self::PipeWire(capture),
                    format!("pipewire {}", describe_target(&target)),
                ))
            }
            AudioBackend::Auto => {
                let target = parse_target(&config.source)?;
                match verify_target(&target).await {
                    Ok(()) => match open_pipewire(&target).await {
                        Ok(capture) => {
                            return Ok((
                                Self::PipeWire(capture),
                                format!("pipewire {}", describe_target(&target)),
                            ))
                        }
                        Err(pipewire_error) => {
                            debug!(error = %pipewire_error, "PipeWire capture failed; trying Pulse");
                        }
                    },
                    Err(error) => {
                        debug!(error = %error, "PipeWire target unavailable; trying Pulse");
                    }
                }
                let name = pulse_source(&config.source)?;
                let capture = PulseCapture::open(&name).await?;
                Ok((
                    Self::Pulse(capture),
                    format!("pulse `{name}` (auto fallback)"),
                ))
            }
        }
    }

    async fn next_chunk(&mut self) -> Result<Chunk, String> {
        match self {
            Self::PipeWire(source) => {
                let frame = source
                    .next_frame()
                    .await
                    .map_err(|error| error.to_string())?;
                Ok(Chunk {
                    pts_micros: frame.pts_micros,
                    sample_rate: frame.sample_rate,
                    channels: frame.channels as u8,
                    samples: frame.samples,
                })
            }
            Self::Pulse(source) => source.next_chunk().await,
            #[cfg(test)]
            Self::Test(source) => source.next_chunk().await,
        }
    }

    async fn stop(&mut self) {
        match self {
            Self::PipeWire(source) => source.stop(),
            Self::Pulse(source) => source.stop().await,
            #[cfg(test)]
            Self::Test(source) => source.stop(),
        }
    }

    fn dropped_frames(&self) -> u64 {
        match self {
            Self::PipeWire(source) => source.stats().dropped_frames,
            Self::Pulse(_) => 0,
            #[cfg(test)]
            Self::Test(source) => source.dropped,
        }
    }
}

fn describe_target(target: &Target) -> String {
    match target {
        Target::Default => "default source".to_owned(),
        Target::Object(name) => format!("object `{name}`"),
        Target::NodeId(id) => format!("node {id}"),
    }
}

async fn open_pipewire(target: &Target) -> Result<ozma_pipewire::PipeWireAudioSource, String> {
    let config = ozma_pipewire::CaptureConfig {
        target: target.stream_target(),
        stream_name: "ozma-webrtc-audio".to_owned(),
        sample_rate: SAMPLE_RATE,
        channels: u16::from(CHANNELS),
        queue_capacity: CAPTURE_QUEUE_CAPACITY,
        ..ozma_pipewire::CaptureConfig::default()
    };
    ozma_pipewire::PipeWireAudioSource::new(config)
        .await
        .map_err(|error| error.to_string())
}

/// A small `parec` capture process for the PulseAudio backend.
///
/// `parec` is part of the same PulseAudio compatibility stack the GStreamer
/// worker uses. The device is always passed explicitly, including `default`,
/// so the selected source is the operator's choice.
struct PulseCapture {
    child: Child,
    stdout: ChildStdout,
    started: Instant,
    first: Option<Vec<u8>>,
}

const PULSE_CHUNK_BYTES: usize = FRAME_SAMPLES * CHANNELS as usize * 2;

impl PulseCapture {
    async fn open(source: &str) -> Result<Self, String> {
        let mut child = Command::new("parec")
            .args([
                "--raw",
                "--format=s16le",
                "--rate=48000",
                "--channels=2",
                "--latency-msec=20",
                "--device",
                source,
            ])
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .spawn()
            .map_err(|error| format!("could not start `parec` for `{source}`: {error}"))?;
        let mut stdout = child
            .stdout
            .take()
            .ok_or_else(|| "parec did not expose its standard output".to_owned())?;
        let started = Instant::now();
        let mut first = vec![0u8; PULSE_CHUNK_BYTES];
        match tokio::time::timeout(PULSE_START_TIMEOUT, stdout.read_exact(&mut first)).await {
            Ok(Ok(_)) => {}
            Ok(Err(error)) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Pulse source `{source}` produced no PCM ({error}); it may not exist"
                ));
            }
            Err(_) => {
                let _ = child.kill().await;
                return Err(format!(
                    "Pulse source `{source}` produced no PCM within {}s; it may not exist",
                    PULSE_START_TIMEOUT.as_secs()
                ));
            }
        }
        Ok(Self {
            child,
            stdout,
            started,
            first: Some(first),
        })
    }

    async fn next_chunk(&mut self) -> Result<Chunk, String> {
        let bytes = match self.first.take() {
            Some(first) => first,
            None => {
                let mut buffer = vec![0u8; PULSE_CHUNK_BYTES];
                match tokio::time::timeout(PULSE_READ_TIMEOUT, self.stdout.read_exact(&mut buffer))
                    .await
                {
                    Ok(Ok(_)) => buffer,
                    Ok(Err(error)) => {
                        return Err(format!("Pulse capture ended: {error}"));
                    }
                    Err(_) => {
                        return Err(format!(
                            "Pulse capture delivered no PCM for {}s",
                            PULSE_READ_TIMEOUT.as_secs()
                        ));
                    }
                }
            }
        };
        let samples = bytes
            .chunks_exact(2)
            .map(|pair| i16::from_le_bytes([pair[0], pair[1]]))
            .collect();
        Ok(Chunk {
            pts_micros: self.started.elapsed().as_micros() as u64,
            sample_rate: SAMPLE_RATE,
            channels: CHANNELS,
            samples,
        })
    }

    async fn stop(&mut self) {
        let _ = self.child.start_kill();
        let _ = self.child.wait().await;
    }
}

/// Convert mono/stereo S16 at any supported rate into 48 kHz stereo.
///
/// The resampler is a bounded linear interpolator. It keeps only the previous
/// stereo pair and the fractional input position, so memory does not grow with
/// stream duration. A rate change resets the phase instead of mixing rates.
pub struct StereoResampler {
    input_rate: u32,
    step: f64,
    consumed: u64,
    next_position: f64,
    previous: [f64; 2],
    initialized: bool,
}

impl Default for StereoResampler {
    fn default() -> Self {
        Self::new()
    }
}

impl StereoResampler {
    pub fn new() -> Self {
        Self {
            input_rate: SAMPLE_RATE,
            step: 1.0,
            consumed: 0,
            next_position: 0.0,
            previous: [0.0; 2],
            initialized: false,
        }
    }

    /// Convert one capture chunk into interleaved stereo at [`SAMPLE_RATE`].
    pub fn push(&mut self, chunk: &Chunk) -> Result<Vec<i16>, String> {
        if chunk.sample_rate == 0 || !(8_000..=192_000).contains(&chunk.sample_rate) {
            return Err(format!("unsupported capture rate {} Hz", chunk.sample_rate));
        }
        if chunk.channels == 0 || chunk.channels > 2 {
            return Err(format!(
                "audio capture is mono/stereo, got {} channels",
                chunk.channels
            ));
        }
        if chunk.samples.len() % usize::from(chunk.channels) != 0 {
            return Err(format!(
                "{} samples is not a whole number of {}-channel frames",
                chunk.samples.len(),
                chunk.channels
            ));
        }
        let rate = chunk.sample_rate;
        if rate != self.input_rate {
            debug!(
                from = self.input_rate,
                to = rate,
                "audio capture rate changed"
            );
            self.input_rate = rate;
            self.step = f64::from(rate) / f64::from(SAMPLE_RATE);
            self.consumed = 0;
            self.next_position = 0.0;
            self.initialized = false;
        }

        let channels = usize::from(chunk.channels);
        let frames = chunk.samples.len() / channels;
        let mut stereo = Vec::with_capacity(frames * 2);
        for frame in chunk.samples.chunks_exact(channels) {
            let left = f64::from(frame[0]);
            let right = if channels == 2 {
                f64::from(frame[1])
            } else {
                left
            };
            stereo.push([left, right]);
        }

        let mut output = Vec::with_capacity(
            ((stereo.len() as f64 / self.step) as usize + 2) * usize::from(CHANNELS),
        );
        let base = self.consumed as f64;
        for (offset, current) in stereo.iter().enumerate() {
            let index = base + offset as f64;
            if !self.initialized {
                self.previous = *current;
                self.initialized = true;
                output.push(self.previous[0].round() as i16);
                output.push(self.previous[1].round() as i16);
                self.next_position = self.step;
                continue;
            }
            while self.next_position <= index {
                let t = (self.next_position - (index - 1.0)).clamp(0.0, 1.0);
                for channel in 0..2 {
                    let value = self.previous[channel] * (1.0 - t) + current[channel] * t;
                    output.push(
                        value
                            .round()
                            .clamp(f64::from(i16::MIN), f64::from(i16::MAX))
                            as i16,
                    );
                }
                self.next_position += self.step;
            }
            self.previous = *current;
        }
        self.consumed = self.consumed.saturating_add(stereo.len() as u64);
        Ok(output)
    }
}

/// Assemble arbitrary 48 kHz stereo blocks into exact 20 ms frames.
pub struct FrameAssembler {
    pending: Vec<i16>,
    next_pts_micros: u64,
    next_sequence: u64,
    dropped_samples: u64,
}

impl Default for FrameAssembler {
    fn default() -> Self {
        Self::new()
    }
}

impl FrameAssembler {
    pub fn new() -> Self {
        Self {
            pending: Vec::new(),
            next_pts_micros: 0,
            next_sequence: 0,
            dropped_samples: 0,
        }
    }

    pub fn dropped_samples(&self) -> u64 {
        self.dropped_samples
    }

    #[cfg(test)]
    pub fn buffered_samples(&self) -> usize {
        self.pending.len() / usize::from(CHANNELS)
    }

    /// Append stereo samples and return every complete 20 ms frame.
    pub fn push(&mut self, samples: &[i16], pts_micros: u64) -> Vec<(u64, Vec<i16>)> {
        debug_assert_eq!(samples.len() % usize::from(CHANNELS), 0);
        if self.pending.is_empty() {
            self.next_pts_micros = pts_micros;
        }
        if self.pending.len().saturating_add(samples.len()) > MAX_PENDING_SAMPLES {
            self.dropped_samples = self.dropped_samples.saturating_add(samples.len() as u64);
            return Vec::new();
        }
        self.pending.extend_from_slice(samples);

        let frame_len = FRAME_SAMPLES * usize::from(CHANNELS);
        let mut frames = Vec::new();
        while self.pending.len() >= frame_len {
            let frame: Vec<i16> = self.pending.drain(..frame_len).collect();
            frames.push((self.next_pts_micros, frame));
            self.next_pts_micros = self.next_pts_micros.saturating_add(FRAME_MICROS);
            self.next_sequence = self.next_sequence.wrapping_add(1);
        }
        frames
    }

    /// Zero-pad and return the final partial frame, if any.
    #[cfg(test)]
    pub fn flush(&mut self) -> Option<(u64, Vec<i16>)> {
        if self.pending.is_empty() {
            return None;
        }
        self.pending
            .resize(FRAME_SAMPLES * usize::from(CHANNELS), 0);
        let samples = std::mem::take(&mut self.pending);
        let frame = (self.next_pts_micros, samples);
        self.next_pts_micros = self.next_pts_micros.saturating_add(FRAME_MICROS);
        self.next_sequence = self.next_sequence.wrapping_add(1);
        Some(frame)
    }

    pub fn frames_assembled(&self) -> u64 {
        self.next_sequence
    }
}

/// The Opus capability advertised by the helper.
pub fn opus_capability() -> RTCRtpCodecCapability {
    RTCRtpCodecCapability {
        mime_type: MIME_TYPE_OPUS.to_owned(),
        clock_rate: 48_000,
        channels: 2,
        sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
        rtcp_feedback: vec![],
    }
}

fn build_encoder() -> Result<OpusEncoder, String> {
    let mut encoder = OpusEncoder::new(
        SampleRate::Hz48000,
        OpusChannels::Stereo,
        Application::Audio,
    )
    .map_err(|error| format!("could not initialise libopus: {error}"))?;
    encoder
        .set_bitrate(Bitrate::BitsPerSecond(AUDIO_BITRATE_BPS))
        .map_err(|error| format!("could not set the Opus bitrate: {error}"))?;
    encoder
        .set_complexity(5)
        .map_err(|error| format!("could not set Opus complexity: {error}"))?;
    encoder
        .set_inband_fec(true)
        .map_err(|error| format!("could not enable Opus FEC: {error}"))?;
    encoder
        .set_packet_loss_perc(10)
        .map_err(|error| format!("could not set Opus loss target: {error}"))?;
    Ok(encoder)
}

fn encode_frame(encoder: &OpusEncoder, samples: &[i16]) -> Result<Vec<u8>, String> {
    if samples.len() != FRAME_SAMPLES * usize::from(CHANNELS) {
        return Err(format!(
            "Opus frame is {} samples, expected {}",
            samples.len(),
            FRAME_SAMPLES * usize::from(CHANNELS)
        ));
    }
    let mut output = vec![0u8; MAX_PACKET_BYTES];
    let written = encoder
        .encode(samples, &mut output)
        .map_err(|error| format!("libopus encode failed: {error}"))?;
    if written == 0 {
        return Err("libopus produced an empty packet".to_owned());
    }
    output.truncate(written);
    Ok(output)
}

/// Counters surfaced in periodic logs and at shutdown.
#[derive(Debug, Default)]
pub struct AudioStats {
    capture_frames: AtomicU64,
    capture_dropped: AtomicU64,
    frames_assembled: AtomicU64,
    frames_dropped_idle: AtomicU64,
    packets_encoded: AtomicU64,
    encoded_bytes: AtomicU64,
    packets_written: AtomicU64,
    packets_dropped_full: AtomicU64,
    encode_errors: AtomicU64,
    write_errors: AtomicU64,
    source_errors: AtomicU64,
    pending_dropped_samples: AtomicU64,
}

/// One shared audio track and its capture/encode/write tasks.
pub struct AudioStream {
    track: Arc<TrackLocalStaticSample>,
    peers: Arc<AtomicUsize>,
    shutdown_flag: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    stats: Arc<AudioStats>,
    tasks: Mutex<Vec<JoinHandle<()>>>,
    description: String,
}

/// RAII active-peer counter for the audio pipeline.
pub struct AudioPeerGuard {
    stream: Arc<AudioStream>,
}

impl Drop for AudioPeerGuard {
    fn drop(&mut self) {
        self.stream
            .peers
            .fetch_update(Ordering::SeqCst, Ordering::SeqCst, |value| {
                Some(value.saturating_sub(1))
            })
            .ok();
    }
}

impl AudioStream {
    /// Open the configured capture source and start the pipeline.
    ///
    /// Returns a typed error when the source is missing or cannot produce PCM
    /// within the bounded startup window. The caller decides whether to keep
    /// running video-only; this function never falls back to a fake source.
    pub async fn start(config: AudioConfig) -> Result<Arc<Self>, String> {
        let (capture, description) = Capture::open(&config).await?;
        Self::start_with_capture(capture, description)
    }

    fn start_with_capture(capture: Capture, description: String) -> Result<Arc<Self>, String> {
        let track = Arc::new(TrackLocalStaticSample::new(
            opus_capability(),
            "audio".to_owned(),
            "ozma".to_owned(),
        ));
        let encoder = build_encoder()?;
        let peers = Arc::new(AtomicUsize::new(0));
        let shutdown_flag = Arc::new(AtomicBool::new(false));
        let shutdown = Arc::new(Notify::new());
        let stats = Arc::new(AudioStats::default());
        let (packet_tx, packet_rx) = mpsc::channel::<Vec<u8>>(PACKET_QUEUE_CAPACITY);

        let capture_task = tokio::spawn(run_capture(
            capture,
            encoder,
            packet_tx,
            peers.clone(),
            shutdown_flag.clone(),
            shutdown.clone(),
            stats.clone(),
        ));
        let writer_task = tokio::spawn(run_writer(
            track.clone(),
            packet_rx,
            shutdown_flag.clone(),
            shutdown.clone(),
            stats.clone(),
        ));

        info!(source = %description, bitrate_bps = AUDIO_BITRATE_BPS, "Opus audio ready (20 ms frames)");
        Ok(Arc::new(Self {
            track,
            peers,
            shutdown_flag,
            shutdown,
            stats,
            tasks: Mutex::new(vec![capture_task, writer_task]),
            description,
        }))
    }

    /// The shared Opus track attached to each peer that offers audio.
    pub fn track(&self) -> Arc<TrackLocalStaticSample> {
        self.track.clone()
    }

    /// Register a peer; encoding is paused while no peer is connected.
    pub fn peer_guard(self: &Arc<Self>) -> AudioPeerGuard {
        self.peers.fetch_add(1, Ordering::SeqCst);
        AudioPeerGuard {
            stream: self.clone(),
        }
    }

    pub fn description(&self) -> &str {
        &self.description
    }

    pub fn stats_snapshot(&self) -> String {
        format!(
            "capture_frames={} capture_dropped={} frames_assembled={} frames_dropped_idle={} packets_encoded={} encoded_bytes={} packets_written={} packets_dropped_full={} encode_errors={} write_errors={} source_errors={} pending_dropped_samples={}",
            self.stats.capture_frames.load(Ordering::Relaxed),
            self.stats.capture_dropped.load(Ordering::Relaxed),
            self.stats.frames_assembled.load(Ordering::Relaxed),
            self.stats.frames_dropped_idle.load(Ordering::Relaxed),
            self.stats.packets_encoded.load(Ordering::Relaxed),
            self.stats.encoded_bytes.load(Ordering::Relaxed),
            self.stats.packets_written.load(Ordering::Relaxed),
            self.stats.packets_dropped_full.load(Ordering::Relaxed),
            self.stats.encode_errors.load(Ordering::Relaxed),
            self.stats.write_errors.load(Ordering::Relaxed),
            self.stats.source_errors.load(Ordering::Relaxed),
            self.stats.pending_dropped_samples.load(Ordering::Relaxed),
        )
    }

    /// Stop capture and writer tasks within a bounded time.
    pub async fn shutdown(&self) {
        self.shutdown_flag.store(true, Ordering::SeqCst);
        self.shutdown.notify_waiters();
        let tasks: Vec<JoinHandle<()>> = {
            let mut tasks = self.tasks.lock().unwrap_or_else(|p| p.into_inner());
            std::mem::take(&mut *tasks)
        };
        for task in tasks {
            let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
        }
    }
}

fn enqueue_packet(tx: &mpsc::Sender<Vec<u8>>, packet: Vec<u8>, stats: &AudioStats) -> bool {
    match tx.try_send(packet) {
        Ok(()) => true,
        Err(mpsc::error::TrySendError::Full(_)) => {
            stats.packets_dropped_full.fetch_add(1, Ordering::Relaxed);
            false
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            stats.packets_dropped_full.fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

async fn run_capture(
    mut capture: Capture,
    encoder: OpusEncoder,
    packet_tx: mpsc::Sender<Vec<u8>>,
    peers: Arc<AtomicUsize>,
    shutdown_flag: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    stats: Arc<AudioStats>,
) {
    let mut resampler = StereoResampler::new();
    let mut assembler = FrameAssembler::new();
    loop {
        if shutdown_flag.load(Ordering::SeqCst) {
            break;
        }
        let chunk = tokio::select! {
            _ = shutdown.notified() => break,
            result = capture.next_chunk() => match result {
                Ok(chunk) => chunk,
                Err(error) => {
                    stats.source_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(error = %error, "audio capture stopped");
                    break;
                }
            }
        };
        stats.capture_frames.fetch_add(1, Ordering::Relaxed);
        stats
            .capture_dropped
            .store(capture.dropped_frames(), Ordering::Relaxed);

        let resampled = match resampler.push(&chunk) {
            Ok(samples) => samples,
            Err(error) => {
                stats.source_errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %error, "audio frame rejected");
                continue;
            }
        };
        let before = assembler.dropped_samples();
        let frames = assembler.push(&resampled, chunk.pts_micros);
        if assembler.dropped_samples() > before {
            stats
                .pending_dropped_samples
                .fetch_add(assembler.dropped_samples() - before, Ordering::Relaxed);
        }
        for (_, samples) in frames {
            stats.frames_assembled.fetch_add(1, Ordering::Relaxed);
            if peers.load(Ordering::SeqCst) == 0 {
                stats.frames_dropped_idle.fetch_add(1, Ordering::Relaxed);
                continue;
            }
            match encode_frame(&encoder, &samples) {
                Ok(packet) => {
                    stats.packets_encoded.fetch_add(1, Ordering::Relaxed);
                    stats
                        .encoded_bytes
                        .fetch_add(packet.len() as u64, Ordering::Relaxed);
                    enqueue_packet(&packet_tx, packet, &stats);
                }
                Err(error) => {
                    stats.encode_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(error = %error, "Opus encode failed; packet skipped");
                }
            }
        }
        if assembler.frames_assembled() % 50 == 0 {
            debug!(
                frames_assembled = assembler.frames_assembled(),
                "audio frames assembled"
            );
        }
    }
    capture.stop().await;
    debug!("audio capture task stopped");
}

async fn run_writer(
    track: Arc<TrackLocalStaticSample>,
    mut packet_rx: mpsc::Receiver<Vec<u8>>,
    shutdown_flag: Arc<AtomicBool>,
    shutdown: Arc<Notify>,
    stats: Arc<AudioStats>,
) {
    loop {
        let packet = tokio::select! {
            _ = shutdown.notified() => break,
            packet = packet_rx.recv() => match packet {
                Some(packet) => packet,
                None => break,
            },
        };
        if shutdown_flag.load(Ordering::SeqCst) {
            break;
        }
        let sample = Sample {
            data: packet.into(),
            duration: Duration::from_micros(FRAME_MICROS),
            ..Default::default()
        };
        match track.write_sample(&sample).await {
            Ok(()) => {
                stats.packets_written.fetch_add(1, Ordering::Relaxed);
            }
            Err(error) => {
                stats.write_errors.fetch_add(1, Ordering::Relaxed);
                debug!(error = %error, "Opus sample write failed");
            }
        }
    }
    debug!("audio writer task stopped");
}

#[cfg(test)]
#[derive(Default)]
struct TestCapture {
    rx: Option<mpsc::Receiver<Chunk>>,
    stopped: bool,
    dropped: u64,
}

#[cfg(test)]
impl TestCapture {
    fn new(rx: mpsc::Receiver<Chunk>) -> Self {
        Self {
            rx: Some(rx),
            stopped: false,
            dropped: 0,
        }
    }

    async fn next_chunk(&mut self) -> Result<Chunk, String> {
        match self.rx.as_mut() {
            Some(rx) => rx
                .recv()
                .await
                .ok_or_else(|| "test source ended".to_owned()),
            None => Err("test source stopped".to_owned()),
        }
    }

    fn stop(&mut self) {
        self.stopped = true;
        self.rx = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use audiopus::coder::Decoder;
    use audiopus::packet::Packet;

    fn sine_chunk(rate: u32, channels: u8, frames: usize) -> Chunk {
        let mut samples = Vec::with_capacity(frames * usize::from(channels));
        for index in 0..frames {
            let value = ((index as f64 * 2.0 * std::f64::consts::PI * 440.0 / f64::from(rate))
                .sin()
                * 12_000.0) as i16;
            for _ in 0..channels {
                samples.push(value);
            }
        }
        Chunk {
            pts_micros: 0,
            sample_rate: rate,
            channels,
            samples,
        }
    }

    #[test]
    fn target_tokens_map_without_implicit_defaults() {
        assert_eq!(parse_target("default").unwrap(), Target::Default);
        assert_eq!(
            parse_target("ozma-vm1.monitor").unwrap(),
            Target::Object("ozma-vm1.monitor".to_owned())
        );
        assert_eq!(
            parse_target("monitor:ozma-vm1").unwrap(),
            Target::Object("ozma-vm1.monitor".to_owned())
        );
        assert_eq!(
            parse_target("object:alsa_input.usb-mic").unwrap(),
            Target::Object("alsa_input.usb-mic".to_owned())
        );
        assert_eq!(parse_target("node:42").unwrap(), Target::NodeId(42));
        assert!(parse_target("").is_err());
        assert!(parse_target("  ").is_err());
        assert!(parse_target("monitor:").is_err());
        assert!(parse_target("node:not-a-number").is_err());
        assert!(parse_target("bad\u{7}name").is_err());
    }

    #[test]
    fn pulse_names_strip_the_helper_prefixes() {
        assert_eq!(
            pulse_source("ozma-vm1.monitor").unwrap(),
            "ozma-vm1.monitor"
        );
        assert_eq!(
            pulse_source("monitor:ozma-vm1").unwrap(),
            "ozma-vm1.monitor"
        );
        assert_eq!(
            pulse_source("object:alsa_input.mic").unwrap(),
            "alsa_input.mic"
        );
        assert_eq!(pulse_source("default").unwrap(), "default");
        assert!(pulse_source("").is_err());
    }

    #[test]
    fn resampler_produces_the_opus_rate_and_upmixes_mono() {
        let mut resampler = StereoResampler::new();
        let input = sine_chunk(44_100, 1, 4_410); // 100 ms mono
        let output = resampler.push(&input).unwrap();
        assert_eq!(output.len() % 2, 0);
        // 100 ms at 48 kHz is 4800 stereo frames, within one frame of the
        // linear interpolator's phase bookkeeping.
        assert!(
            (output.len() / 2).abs_diff(4_800) <= 1,
            "got {} stereo frames",
            output.len() / 2
        );
        // Mono is duplicated, not left silent.
        assert!(output.chunks_exact(2).all(|pair| pair[0] == pair[1]));
        assert!(output.iter().any(|sample| *sample != 0));
    }

    #[test]
    fn resampler_rejects_unsupported_channels_and_rates() {
        let mut resampler = StereoResampler::new();
        let mut chunk = sine_chunk(48_000, 2, 960);
        chunk.channels = 3;
        assert!(resampler.push(&chunk).is_err());
        chunk.channels = 2;
        chunk.sample_rate = 4_000;
        assert!(resampler.push(&chunk).is_err());
    }

    #[test]
    fn assembler_emits_exact_20ms_frames_and_pts() {
        let mut assembler = FrameAssembler::new();
        let samples = vec![7i16; FRAME_SAMPLES * 2];
        let frames = assembler.push(&samples, 1_000_000);
        assert_eq!(frames.len(), 1);
        assert_eq!(frames[0].0, 1_000_000);
        assert_eq!(frames[0].1.len(), FRAME_SAMPLES * 2);
        assert_eq!(assembler.buffered_samples(), 0);

        // A 1024-frame block makes one 960-frame packet and leaves 64 frames
        // buffered; the next block makes another packet and leaves 128.
        let mut assembler = FrameAssembler::new();
        let frames = assembler.push(&vec![0i16; 1_024 * 2], 0);
        assert_eq!(frames.len(), 1);
        assert_eq!(assembler.buffered_samples(), 64);
        let frames = assembler.push(&vec![0i16; 1_024 * 2], 20_000);
        assert_eq!(frames.len(), 1);
        assert_eq!(assembler.buffered_samples(), 128);
    }

    #[test]
    fn assembler_bounds_pending_samples_and_counts_drops() {
        let mut assembler = FrameAssembler::new();
        let oversized = vec![0i16; MAX_PENDING_SAMPLES + 2];
        assert!(assembler.push(&oversized, 0).is_empty());
        assert_eq!(assembler.dropped_samples(), oversized.len() as u64);
        assert_eq!(assembler.buffered_samples(), 0);

        let frames = assembler.push(&vec![0i16; FRAME_SAMPLES * 2], 5);
        assert_eq!(frames.len(), 1);
    }

    #[test]
    fn flush_pads_the_final_partial_frame() {
        let mut assembler = FrameAssembler::new();
        assert!(assembler.push(&vec![1i16; 100], 9).is_empty());
        let frame = assembler.flush().expect("partial frame");
        assert_eq!(frame.0, 9);
        assert_eq!(frame.1.len(), FRAME_SAMPLES * 2);
        assert!(frame.1[100..].iter().all(|sample| *sample == 0));
        assert!(assembler.flush().is_none());
    }

    #[test]
    fn opus_encoder_emits_one_decodable_20ms_packet_per_frame() {
        let encoder = build_encoder().expect("encoder");
        let mut decoder = Decoder::new(SampleRate::Hz48000, OpusChannels::Stereo).unwrap();
        for index in 0..3 {
            let chunk = sine_chunk(48_000, 2, FRAME_SAMPLES);
            let packet = encode_frame(&encoder, &chunk.samples).expect("encode");
            assert!(!packet.is_empty());
            assert!(packet.len() < 1_275, "packet is {} bytes", packet.len());
            let input = Packet::try_from(packet.as_slice()).expect("packet");
            let mut decoded = vec![0i16; FRAME_SAMPLES * 2];
            let output = audiopus::MutSignals::try_from(decoded.as_mut_slice()).expect("signals");
            let frames = decoder.decode(Some(input), output, false).expect("decode");
            assert_eq!(frames, FRAME_SAMPLES);
            assert!(
                decoded.iter().any(|sample| *sample != 0),
                "packet {index} decoded silent"
            );
        }
        assert!(encode_frame(&encoder, &vec![0i16; 10]).is_err());
    }

    #[tokio::test]
    async fn packet_queue_is_bounded_and_counts_drops() {
        let (tx, _rx) = mpsc::channel::<Vec<u8>>(1);
        let stats = AudioStats::default();
        assert!(enqueue_packet(&tx, vec![1], &stats));
        assert!(!enqueue_packet(&tx, vec![2], &stats));
        assert_eq!(stats.packets_dropped_full.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn audio_stream_stops_capture_and_writer_tasks() {
        let (tx, rx) = mpsc::channel::<Chunk>(2);
        let capture = Capture::Test(TestCapture::new(rx));
        let stream = AudioStream::start_with_capture(capture, "test".to_owned()).unwrap();
        let _peer = stream.peer_guard();
        tx.send(sine_chunk(48_000, 2, FRAME_SAMPLES)).await.unwrap();
        tokio::time::sleep(Duration::from_millis(50)).await;
        stream.shutdown().await;
        let stats = stream.stats_snapshot();
        assert!(stats.contains("capture_frames="), "{stats}");
        assert!(stats.contains("packets_encoded=0") || stats.contains("packets_encoded=1"));
    }

    #[test]
    fn pulse_chunk_size_is_exactly_one_20ms_stereo_frame() {
        assert_eq!(PULSE_CHUNK_BYTES, 3_840);
        assert_eq!(PULSE_CHUNK_BYTES / 4, FRAME_SAMPLES);
    }
}
