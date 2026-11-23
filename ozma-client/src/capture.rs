// SPDX-License-Identifier: AGPL-3.0-only
//! Native camera and microphone uplink.
//!
//! The browser and native clients publish the same protocol: encoded H.264
//! access units on the [`ChannelKind::Camera`] datagram channel and raw Opus
//! packets on [`ChannelKind::Mic`]. This module owns the client half of that
//! contract:
//!
//! * [`ClientCapture::start`] opens a real capture source through
//!   `ozma-capture`, selects an encoder through `ozma-encode`, and publishes
//!   fragments until stopped.
//! * Video and audio run in independent halves. A failure in one updates the
//!   shared state and stops only that half; the other keeps running.
//! * Media is handed to the transport through small bounded queues. A slow or
//!   disconnected transport drops frames instead of blocking capture or
//!   growing memory.
//!
//! [`ClientCaptureOptions::camera`] is `None` to use the first available capture
//! source and `Some(id)` to open a specific device. Microphone capture is off
//! unless [`ClientCaptureOptions::mic`] is set.
//!
//! The transport only carries a microphone channel when the session was created
//! with `audio.mic = true`; [`ClientCapture::start`] refuses to start audio
//! without it rather than streaming packets into nothing.

use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use bytes::Bytes;
#[cfg(target_os = "linux")]
use ozma_audio::PipeWireCapture;
use ozma_audio::{AlsaCapture, AlsaCaptureConfig};
use ozma_audio::{AudioEncoder, AudioSource, EncoderConfig, OpusEncoder};
use ozma_capture::CaptureRegistry;
use ozma_encode::EncoderRegistry;
use ozma_media::{CaptureSource, Encoder, FrameFormat, MediaError};
use ozma_protocol::media::VideoFragmentHeader;
use ozma_protocol::{codes, ApiError, ChannelKind, Codec, MediaFormat};
use ozma_transport::{ChannelId, QuicSender};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinHandle;
use tokio::time::{timeout_at, Instant};

use crate::error;

/// Largest fragment payload published on the camera channel. This matches the
/// browser client and leaves room for the 15-byte fragment header inside a
/// channel payload of 1200 bytes.
pub const CAMERA_FRAGMENT_PAYLOAD: usize = 1_100;

/// Logical track index for the single camera upload track.
pub const CAMERA_TRACK_ID: u8 = 0;

/// Bounded queue of encoded access units before they are fragmented and sent.
const VIDEO_QUEUE: usize = 4;

/// Bounded queue of raw Opus packets before they are sent.
const AUDIO_QUEUE: usize = 8;

/// Default bitrate when the caller leaves it at zero.
const DEFAULT_BITRATE_KBPS: u32 = 4_000;

/// How long [`ClientCapture::stop`] waits for workers before aborting them.
const STOP_TIMEOUT: Duration = Duration::from_secs(5);

/// Capture settings requested by the caller.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ClientCaptureOptions {
    /// Camera source id. `None` disables the camera; `Some("")` or
    /// `Some("default")` opens the first available source;
    /// `Some("pipewire:...")` or `Some("/dev/video0")` opens that device.
    pub camera: Option<String>,
    /// Microphone source. `None` disables audio; `Some("default")` opens the
    /// system default, and any other value is treated as an ALSA device id.
    pub mic: Option<String>,
    /// Requested camera width in pixels. Used for encoder rate control; the
    /// capture device chooses the actual geometry it can deliver.
    pub width: u32,
    /// Requested camera height in pixels.
    pub height: u32,
    /// Requested camera frame rate in Hz.
    pub fps: u32,
    /// H.264 target bitrate in kbps. Zero selects a conservative default.
    pub bitrate_kbps: u32,
}

impl Default for ClientCaptureOptions {
    fn default() -> Self {
        Self {
            camera: None,
            mic: None,
            width: 1280,
            height: 720,
            fps: 30,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
        }
    }
}

/// Snapshot of the capture runtime's counters and negotiated metadata.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientCaptureState {
    /// True while the video half is running.
    pub video: bool,
    /// True while the audio half is running.
    pub audio: bool,
    /// Negotiated video codec, for example `h264`.
    pub video_codec: Option<String>,
    /// Encoded width, if a video pipeline started.
    pub width: Option<u16>,
    /// Encoded height, if a video pipeline started.
    pub height: Option<u16>,
    /// Complete camera access units handed to the transport.
    pub frames_sent: u64,
    /// Encoded camera bytes handed to the transport.
    pub bytes_sent: u64,
    /// Media dropped because a queue was full or the transport rejected it.
    pub dropped: u64,
    /// Capture or encode errors observed by either half.
    pub errors: u64,
    /// Opus packets handed to the transport.
    pub mic_packets: u64,
    /// Opus packet bytes handed to the transport.
    pub mic_bytes: u64,
}

struct Counters {
    video: AtomicBool,
    audio: AtomicBool,
    video_codec: Mutex<Option<String>>,
    width: AtomicU32,
    height: AtomicU32,
    frames_sent: AtomicU64,
    bytes_sent: AtomicU64,
    dropped: AtomicU64,
    errors: AtomicU64,
    mic_packets: AtomicU64,
    mic_bytes: AtomicU64,
}

impl Counters {
    fn new() -> Self {
        Self {
            video: AtomicBool::new(false),
            audio: AtomicBool::new(false),
            video_codec: Mutex::new(None),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            frames_sent: AtomicU64::new(0),
            bytes_sent: AtomicU64::new(0),
            dropped: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            mic_packets: AtomicU64::new(0),
            mic_bytes: AtomicU64::new(0),
        }
    }

    fn snapshot(&self) -> ClientCaptureState {
        let u16_or_none = |value: u32| u16::try_from(value).ok().filter(|value| *value != 0);
        ClientCaptureState {
            video: self.video.load(Ordering::Acquire),
            audio: self.audio.load(Ordering::Acquire),
            video_codec: self
                .video_codec
                .lock()
                .expect("client capture codec lock")
                .clone(),
            width: u16_or_none(self.width.load(Ordering::Relaxed)),
            height: u16_or_none(self.height.load(Ordering::Relaxed)),
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            dropped: self.dropped.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            mic_packets: self.mic_packets.load(Ordering::Relaxed),
            mic_bytes: self.mic_bytes.load(Ordering::Relaxed),
        }
    }
}

struct VideoWork {
    sequence: u64,
    keyframe: bool,
    access_unit: Bytes,
}

/// A running native capture pipeline.
///
/// Dropping the handle requests a stop and aborts workers; call
/// [`ClientCapture::stop`] for a bounded, joined stop that also runs each
/// source's own [`CaptureSource::stop`] / [`AudioSource::stop`].
pub struct ClientCapture {
    state: Arc<Counters>,
    force_keyframe: Arc<AtomicBool>,
    stops: Vec<watch::Sender<bool>>,
    tasks: Vec<JoinHandle<()>>,
}

impl std::fmt::Debug for ClientCapture {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientCapture")
            .field("state", &self.state.snapshot())
            .field("workers", &self.tasks.len())
            .finish()
    }
}

impl ClientCapture {
    /// Open the requested camera and microphone and start publishing them.
    ///
    /// The caller must first send `CaptureStart` so the node creates the decode
    /// workers.
    pub async fn start(
        sender: QuicSender,
        options: ClientCaptureOptions,
    ) -> Result<Self, ApiError> {
        let video = match options.camera.as_deref() {
            Some(target) => {
                let source = open_camera_source(target)?;
                let format = source.format();
                let encoder = build_camera_encoder(&options, format).await?;
                Some((source, encoder))
            }
            None => None,
        };
        let audio = match options.mic.as_deref() {
            Some(device) => Some(open_audio(device).await?),
            None => None,
        };
        Self::start_with_media(sender, options, video, audio).await
    }

    /// Start with already constructed media components.
    ///
    /// This is the test seam: integration tests can pass
    /// [`ozma_media::FakeCapture`] plus a real
    /// [`ozma_encode::H264SoftwareEncoder`], and an
    /// [`ozma_audio::FakeAudioSource`] plus a real
    /// [`ozma_audio::OpusEncoder`], without capture hardware. Passing `None`
    /// for a half disables it. Production callers should use
    /// [`ClientCapture::start`].
    #[doc(hidden)]
    pub async fn start_with_media(
        sender: QuicSender,
        options: ClientCaptureOptions,
        video: Option<(Box<dyn CaptureSource>, Box<dyn Encoder>)>,
        audio: Option<(Box<dyn AudioSource>, Box<dyn AudioEncoder>)>,
    ) -> Result<Self, ApiError> {
        if video.is_none() && audio.is_none() {
            return Err(error::config(
                "No camera or microphone was supplied to the capture runtime",
            ));
        }

        let camera_channel = match video.is_some() {
            true => Some(
                sender
                    .channel_for_kind(ChannelKind::Camera)
                    .ok_or_else(|| {
                        error::config(
                            "The negotiated session has no camera channel for client capture",
                        )
                        .with_remediation(
                            "Use a node and session offer that advertise ChannelKind::Camera.",
                        )
                    })?,
            ),
            false => None,
        };
        let mic_channel = match audio.is_some() {
            true => Some(
                sender
                    .channel_for_kind(ChannelKind::Mic)
                    .ok_or_else(|| mic_channel_missing())?,
            ),
            false => None,
        };

        let state = Arc::new(Counters::new());
        let force_keyframe = Arc::new(AtomicBool::new(false));
        let mut stops = Vec::new();
        let mut tasks = Vec::new();

        if let Some((source, encoder)) = video {
            let channel = camera_channel.expect("camera channel resolved for video");
            let output = encoder.output_format();
            *state.video_codec.lock().expect("client capture codec lock") =
                Some(output.codec.as_str().to_owned());
            state
                .width
                .store(output.width.min(u32::from(u16::MAX)), Ordering::Relaxed);
            state
                .height
                .store(output.height.min(u32::from(u16::MAX)), Ordering::Relaxed);
            state.video.store(true, Ordering::Release);

            let (stop_tx, stop_rx) = watch::channel(false);
            stops.push(stop_tx.clone());
            let (queue_tx, queue_rx) = mpsc::channel::<VideoWork>(VIDEO_QUEUE);
            tasks.push(tokio::spawn(video_sender(
                sender.clone(),
                channel,
                queue_rx,
                Arc::clone(&state),
                stop_tx,
            )));
            tasks.push(tokio::spawn(video_capture(
                source,
                encoder,
                queue_tx,
                stop_rx,
                Arc::clone(&state),
                Arc::clone(&force_keyframe),
            )));
            tracing::debug!(
                codec = %output.label(),
                "native camera capture started"
            );
        }

        if let Some((source, encoder)) = audio {
            let channel = mic_channel.expect("mic channel resolved for audio");
            state.audio.store(true, Ordering::Release);

            let (stop_tx, stop_rx) = watch::channel(false);
            stops.push(stop_tx.clone());
            let (queue_tx, queue_rx) = mpsc::channel::<Bytes>(AUDIO_QUEUE);
            tasks.push(tokio::spawn(audio_sender(
                sender.clone(),
                channel,
                queue_rx,
                Arc::clone(&state),
                stop_tx,
            )));
            tasks.push(tokio::spawn(audio_capture(
                source,
                encoder,
                queue_tx,
                stop_rx,
                Arc::clone(&state),
            )));
            tracing::debug!(
                mic = options.mic.as_deref().unwrap_or("default"),
                "native microphone capture started"
            );
        }

        Ok(Self {
            state,
            force_keyframe,
            stops,
            tasks,
        })
    }

    /// Current counters and negotiated metadata.
    pub fn state(&self) -> ClientCaptureState {
        self.state.snapshot()
    }

    /// Ask the camera encoder for an IDR on its next frame.
    pub fn request_keyframe(&self) {
        self.force_keyframe.store(true, Ordering::SeqCst);
    }

    /// Stop both halves and wait (bounded) for their workers.
    ///
    /// Calling this after a half has already failed is safe, and dropping the
    /// handle without calling it still stops the workers.
    pub async fn stop(mut self) {
        self.shutdown().await;
    }

    /// The non-consuming stop used by [`crate::NativeSessionClient`] so it can
    /// read the final counters after the workers exit.
    pub(crate) async fn shutdown(&mut self) {
        for stop in &self.stops {
            let _ = stop.send(true);
        }
        let deadline = Instant::now() + STOP_TIMEOUT;
        for mut task in self.tasks.drain(..) {
            if timeout_at(deadline, &mut task).await.is_err() {
                task.abort();
                let _ = task.await;
            }
        }
    }
}

impl Drop for ClientCapture {
    fn drop(&mut self) {
        for stop in &self.stops {
            let _ = stop.send(true);
        }
        for task in self.tasks.drain(..) {
            task.abort();
        }
    }
}

async fn video_capture(
    mut source: Box<dyn CaptureSource>,
    mut encoder: Box<dyn Encoder>,
    queue: mpsc::Sender<VideoWork>,
    mut stop: watch::Receiver<bool>,
    state: Arc<Counters>,
    force_keyframe: Arc<AtomicBool>,
) {
    loop {
        if *stop.borrow() {
            break;
        }
        let frame = tokio::select! {
            changed = stop.changed() => {
                let _ = changed;
                break;
            }
            frame = source.next_frame() => frame,
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(MediaError::EndOfStream) | Err(MediaError::Closed { .. }) => break,
            Err(error) => {
                state.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %error, "camera capture stopped after a source error");
                break;
            }
        };

        if force_keyframe.swap(false, Ordering::SeqCst) {
            if let Err(error) = encoder.request_keyframe().await {
                state.errors.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(error = %error, "camera encoder rejected a keyframe request");
            }
        }

        let encoded = match encoder.encode(frame).await {
            Ok(encoded) => encoded,
            Err(error) => {
                state.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %error, "camera capture stopped after an encode error");
                break;
            }
        };
        let work = VideoWork {
            sequence: encoded.sequence(),
            keyframe: encoded.is_keyframe(),
            access_unit: encoded.into_data(),
        };
        if queue.try_send(work).is_err() {
            state.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }
    let _ = source.stop().await;
    state.video.store(false, Ordering::Release);
}

async fn video_sender(
    sender: QuicSender,
    channel: ChannelId,
    mut queue: mpsc::Receiver<VideoWork>,
    state: Arc<Counters>,
    stop: watch::Sender<bool>,
) {
    while let Some(work) = queue.recv().await {
        let mut failed = false;
        for datagram in fragment_access_unit(work.keyframe, work.sequence, &work.access_unit) {
            if let Err(error) = sender.send(channel, Bytes::from(datagram)).await {
                state.errors.fetch_add(1, Ordering::Relaxed);
                tracing::debug!(error = %error, "camera fragment send failed");
                failed = true;
                break;
            }
        }
        if failed {
            let _ = stop.send(true);
            break;
        }
        state.frames_sent.fetch_add(1, Ordering::Relaxed);
        state
            .bytes_sent
            .fetch_add(work.access_unit.len() as u64, Ordering::Relaxed);
    }
}

async fn audio_capture(
    mut source: Box<dyn AudioSource>,
    mut encoder: Box<dyn AudioEncoder>,
    queue: mpsc::Sender<Bytes>,
    mut stop: watch::Receiver<bool>,
    state: Arc<Counters>,
) {
    let mut reframer = match ozma_audio::AudioReframer::from_config(&encoder.config()) {
        Ok(reframer) => reframer,
        Err(error) => {
            state.errors.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(error = %error, "microphone reframer could not be created");
            let _ = source.stop().await;
            state.audio.store(false, Ordering::Release);
            return;
        }
    };
    loop {
        if *stop.borrow() {
            break;
        }
        let frame = tokio::select! {
            changed = stop.changed() => {
                let _ = changed;
                break;
            }
            frame = source.next_frame() => frame,
        };
        let frame = match frame {
            Ok(frame) => frame,
            Err(ozma_audio::AudioError::EndOfStream)
            | Err(ozma_audio::AudioError::Closed { .. }) => break,
            Err(error) => {
                state.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %error, "microphone capture stopped after a source error");
                break;
            }
        };
        let frames = match reframer.push(frame) {
            Ok(frames) => frames,
            Err(error) => {
                state.errors.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(error = %error, "microphone capture stopped after a reframe error");
                break;
            }
        };
        for frame in frames {
            let packet = match encoder.encode(frame).await {
                Ok(packet) => packet,
                Err(error) => {
                    state.errors.fetch_add(1, Ordering::Relaxed);
                    tracing::warn!(error = %error, "microphone encode failed");
                    continue;
                }
            };
            if queue.try_send(Bytes::from(packet.data)).is_err() {
                state.dropped.fetch_add(1, Ordering::Relaxed);
            }
        }
    }
    let _ = source.stop().await;
    state.audio.store(false, Ordering::Release);
}

async fn audio_sender(
    sender: QuicSender,
    channel: ChannelId,
    mut queue: mpsc::Receiver<Bytes>,
    state: Arc<Counters>,
    stop: watch::Sender<bool>,
) {
    while let Some(packet) = queue.recv().await {
        let len = packet.len() as u64;
        if let Err(error) = sender.send(channel, packet).await {
            state.errors.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(error = %error, "microphone packet send failed");
            let _ = stop.send(true);
            break;
        }
        state.mic_packets.fetch_add(1, Ordering::Relaxed);
        state.mic_bytes.fetch_add(len, Ordering::Relaxed);
    }
}

/// Split one access unit into camera-channel datagrams.
fn fragment_access_unit(keyframe: bool, sequence: u64, data: &[u8]) -> Vec<Vec<u8>> {
    if data.is_empty() {
        return Vec::new();
    }
    let count = data.len().div_ceil(CAMERA_FRAGMENT_PAYLOAD) as u16;
    data.chunks(CAMERA_FRAGMENT_PAYLOAD)
        .enumerate()
        .map(|(index, chunk)| {
            VideoFragmentHeader::new(keyframe, CAMERA_TRACK_ID, sequence, index as u16, count)
                .encode_into(chunk)
        })
        .collect()
}

fn open_camera_source(camera: &str) -> Result<Box<dyn CaptureSource>, ApiError> {
    let registry = CaptureRegistry::system_with_pipewire();
    if !camera.is_empty() && camera != "default" {
        return registry.open_id(camera);
    }
    let capabilities = registry.enumerate()?;
    let capability = capabilities.into_iter().next().ok_or_else(|| {
        ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            "No camera capture source is available on this machine",
        )
        .with_remediation(
            "Connect a camera, start a PipeWire session, or pass an explicit source id.",
        )
    })?;
    registry.open(&capability)
}

async fn build_camera_encoder(
    options: &ClientCaptureOptions,
    input: FrameFormat,
) -> Result<Box<dyn Encoder>, ApiError> {
    let fps = options.fps.max(1);
    let bitrate = if options.bitrate_kbps == 0 {
        DEFAULT_BITRATE_KBPS
    } else {
        options.bitrate_kbps
    };
    let request = MediaFormat::new(Codec::H264, input.width, input.height, fps, bitrate);
    let registry = EncoderRegistry::with_detected_encoders().await;
    let selection = registry.select(&request)?;
    let encoder = selection.build_encoder(input)?;
    if !encoder.input_format().feeds(&input) {
        return Err(ApiError::new(
            codes::ENCODER_UNAVAILABLE,
            format!(
                "The selected encoder accepts {} but the camera produces {input}",
                encoder.input_format()
            ),
        )
        .with_details(input.to_string())
        .with_remediation("Choose a camera format the selected encoder supports."));
    }
    Ok(Box::new(encoder))
}

async fn open_audio(
    device: &str,
) -> Result<(Box<dyn AudioSource>, Box<dyn AudioEncoder>), ApiError> {
    let source: Box<dyn AudioSource> = if device.is_empty() || device == "default" {
        open_default_audio().await?
    } else {
        Box::new(
            AlsaCapture::open(AlsaCaptureConfig::for_device(device))
                .map_err(|error| error.to_api_error())?,
        )
    };

    let format = source.format();
    if format.sample_rate != ozma_audio::SAMPLE_RATE || format.channels != ozma_audio::CHANNELS {
        return Err(ApiError::new(
            ozma_audio::audio_codes::AUDIO_UNAVAILABLE,
            format!(
                "The microphone produces {} Hz {} channel audio, but the Opus uplink requires {} Hz stereo",
                format.sample_rate,
                format.channels,
                ozma_audio::SAMPLE_RATE
            ),
        )
        .with_details(device.to_owned())
        .with_remediation("Use a device or PipeWire target that provides 48 kHz stereo capture."));
    }
    let encoder = OpusEncoder::new(EncoderConfig::opus()).map_err(|error| error.to_api_error())?;
    Ok((source, Box::new(encoder)))
}

#[cfg(target_os = "linux")]
async fn open_default_audio() -> Result<Box<dyn AudioSource>, ApiError> {
    match PipeWireCapture::default_source().await {
        Ok(source) => Ok(Box::new(source)),
        Err(error) => {
            tracing::warn!(
                error = %error,
                "PipeWire microphone unavailable; falling back to the default ALSA device"
            );
            Ok(Box::new(
                AlsaCapture::open(AlsaCaptureConfig::default())
                    .map_err(|error| error.to_api_error())?,
            ))
        }
    }
}

#[cfg(not(target_os = "linux"))]
async fn open_default_audio() -> Result<Box<dyn AudioSource>, ApiError> {
    Ok(Box::new(
        AlsaCapture::open(AlsaCaptureConfig::default()).map_err(|error| error.to_api_error())?,
    ))
}

/// The error used when audio is requested without a negotiated mic channel.
pub(crate) fn mic_channel_missing() -> ApiError {
    error::config("The session has no microphone channel, so microphone capture cannot start")
        .with_remediation("Create the session with `audio.mic = true`, then reconnect and retry.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::video::VideoAssembler;
    use ozma_media::{FakeCapture, FakeEncoder, FrameFormat};
    use ozma_transport::{
        ChannelMap, QuicClient, QuicConfig, QuicServer, QuicTransport, SessionTransport,
    };
    use std::net::SocketAddr;
    use tokio::time::timeout;

    async fn test_sender(map: ChannelMap) -> (QuicTransport, tokio::task::JoinHandle<()>) {
        let server = QuicServer::bind_with_config(
            SocketAddr::from(([127, 0, 0, 1], 0)),
            QuicConfig::default(),
            map.clone(),
        )
        .unwrap();
        let address = server.local_addr().unwrap();
        let certificate = server.certificate_der().to_vec();
        let accept = tokio::spawn(async move {
            let mut transport = match server.accept().await {
                Ok(transport) => transport,
                Err(_) => return,
            };
            loop {
                match transport.recv().await {
                    ozma_transport::TransportEvent::Closed { .. } => break,
                    _ => {}
                }
            }
        });
        let client = QuicClient::connect(address, "localhost", certificate, map)
            .await
            .unwrap();
        // Opening the control stream needs one write before the server can
        // accept it; keep the accepted side alive for the test's lifetime.
        client
            .sender()
            .send(ChannelId::CONTROL, Bytes::from_static(b"ready"))
            .await
            .unwrap();
        (client, accept)
    }

    fn camera_media() -> (Box<dyn CaptureSource>, Box<dyn Encoder>) {
        let format = FrameFormat::rgb24(32, 24);
        let source = FakeCapture::new(format).with_fps(1_000);
        let encoder = FakeEncoder::h264(format, 500);
        (Box::new(source), Box::new(encoder))
    }

    fn audio_media() -> (Box<dyn AudioSource>, Box<dyn AudioEncoder>) {
        // A finite source keeps the current-thread test runtime free: an
        // unlimited fake source never awaits and would starve the executor.
        let source = ozma_audio::FakeAudioSource::new().with_frame_limit(32);
        let encoder = OpusEncoder::new(EncoderConfig::opus()).unwrap();
        (Box::new(source), Box::new(encoder))
    }

    #[test]
    fn fragmentation_roundtrips_through_the_video_assembler() {
        assert_eq!(CAMERA_FRAGMENT_PAYLOAD, 1_100);
        let data: Vec<u8> = (0..(CAMERA_FRAGMENT_PAYLOAD * 2 + 37))
            .map(|index| (index % 251) as u8)
            .collect();
        let fragments = fragment_access_unit(true, 7, &data);
        assert_eq!(fragments.len(), 3);
        let mut assembler = VideoAssembler::default();
        let mut complete = None;
        for fragment in &fragments {
            let (header, payload) = VideoFragmentHeader::parse(fragment).expect("valid fragment");
            assert_eq!(header.track_id, CAMERA_TRACK_ID);
            assert_eq!(header.frame_sequence, 7);
            assert!(header.keyframe);
            assert!(payload.len() <= CAMERA_FRAGMENT_PAYLOAD);
            complete = assembler.push(fragment).unwrap().or(complete);
        }
        let frame = complete.expect("every fragment was supplied");
        assert!(frame.keyframe);
        assert_eq!(frame.sequence, 7);
        assert_eq!(frame.data, data);
        assert_eq!(assembler.pending_len(), 0);
    }

    #[tokio::test]
    async fn state_counts_frames_bytes_and_mic_packets() {
        let (client, _server) = test_sender(ChannelMap::default_session()).await;
        let capture = ClientCapture::start_with_media(
            client.sender(),
            ClientCaptureOptions {
                camera: Some("fake".into()),
                mic: Some("default".into()),
                ..ClientCaptureOptions::default()
            },
            Some(camera_media()),
            Some(audio_media()),
        )
        .await
        .unwrap();
        assert!(capture.state().audio, "audio must report running at start");

        timeout(Duration::from_secs(5), async {
            loop {
                let state = capture.state();
                if state.frames_sent >= 2 && state.mic_packets >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("capture counters did not advance");

        let state = capture.state();
        assert!(state.video || state.frames_sent > 0);
        assert_eq!(state.video_codec.as_deref(), Some("h264"));
        assert_eq!(state.width, Some(32));
        assert_eq!(state.height, Some(24));
        assert!(state.bytes_sent > 0);
        assert!(state.mic_bytes > 0);
        assert!(state.errors == 0, "unexpected capture errors: {state:?}");
        capture.stop().await;
    }

    #[tokio::test]
    async fn stop_is_idempotent() {
        let (client, _server) = test_sender(ChannelMap::default_session()).await;
        let format = FrameFormat::rgb24(16, 16);
        let source = FakeCapture::new(format).with_fps(1_000);
        let encoder = FakeEncoder::h264(format, 500);
        let mut capture = ClientCapture::start_with_media(
            client.sender(),
            ClientCaptureOptions {
                camera: Some("fake".into()),
                ..ClientCaptureOptions::default()
            },
            Some((Box::new(source), Box::new(encoder))),
            None,
        )
        .await
        .unwrap();

        capture.shutdown().await;
        let stopped = capture.state();
        capture.shutdown().await;
        capture.shutdown().await;
        assert_eq!(capture.state().frames_sent, stopped.frames_sent);
        assert!(!capture.state().video);
    }

    #[tokio::test]
    async fn stop_after_source_end_is_safe() {
        let (client, _server) = test_sender(ChannelMap::default_session()).await;
        let format = FrameFormat::rgb24(16, 16);
        let source = FakeCapture::new(format).with_fps(1_000).with_frame_limit(1);
        let encoder = FakeEncoder::h264(format, 500);
        let capture = ClientCapture::start_with_media(
            client.sender(),
            ClientCaptureOptions {
                camera: Some("fake".into()),
                ..ClientCaptureOptions::default()
            },
            Some((Box::new(source), Box::new(encoder))),
            None,
        )
        .await
        .unwrap();

        timeout(Duration::from_secs(5), async {
            while capture.state().video {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("capture did not notice end of stream");

        // The video half already stopped itself; stop must still complete.
        capture.stop().await;
    }

    #[tokio::test]
    #[ignore = "requires a real camera; set OZMA_TEST_CAPTURE_DEVICE"]
    async fn real_camera_publishes_frames() {
        let Ok(device) = std::env::var("OZMA_TEST_CAPTURE_DEVICE") else {
            eprintln!("skipping: OZMA_TEST_CAPTURE_DEVICE is not set");
            return;
        };
        let (client, _server) = test_sender(ChannelMap::default_session()).await;
        let capture = ClientCapture::start(
            client.sender(),
            ClientCaptureOptions {
                camera: Some(device),
                mic: None,
                width: 640,
                height: 480,
                fps: 30,
                bitrate_kbps: 1_000,
            },
        )
        .await
        .expect("open the real camera and encoder");
        timeout(Duration::from_secs(15), async {
            loop {
                if capture.state().frames_sent >= 2 {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("real camera did not publish frames");
        let state = capture.state();
        assert_eq!(state.video_codec.as_deref(), Some("h264"));
        assert!(state.frames_sent >= 2);
        capture.stop().await;
    }

    #[tokio::test]
    async fn microphone_without_a_negotiated_channel_is_a_config_error() {
        let map =
            ChannelMap::for_tracks(&[ozma_protocol::TrackId::new("video0")], true, false).unwrap();
        assert!(map.channel_for_kind(ChannelKind::Mic).is_none());
        let (client, _server) = test_sender(map).await;
        let error = ClientCapture::start_with_media(
            client.sender(),
            ClientCaptureOptions {
                camera: None,
                mic: Some("default".into()),
                ..ClientCaptureOptions::default()
            },
            None,
            Some(audio_media()),
        )
        .await
        .expect_err("audio must not start without a mic channel");
        assert_eq!(error.code, crate::error::codes::CLIENT_CONFIG);
        assert!(error
            .remediation
            .as_deref()
            .is_some_and(|text| text.contains("audio.mic")));
    }
}
