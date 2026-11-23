// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Browser camera/microphone uplink.
//!
//! A browser sends encoded H.264 access units on the [`ChannelKind::Camera`]
//! datagram channel and raw Opus packets on the existing [`ChannelKind::Mic`]
//! channel. This module owns the per-session receive side:
//!
//! * [`ClientCaptureHub`] is a cloneable, bounded, thread-safe handle that
//!   decodes camera and microphone media on background workers so the session
//!   loop never blocks on a codec.
//! * Latest decoded video is kept as a single I420 frame; decoded audio is
//!   kept in a small ring. Neither grows with the number of packets received.
//! * [`ClientCaptureSource`] adapts the latest decoded camera frame to the
//!   [`ozma_media::CaptureSource`] boundary so a track can select the browser
//!   camera instead of a local device.
//!
//! # Deliberate limits
//!
//! * The camera path accepts Annex-B H.264 only, matching
//!   [`ozma_decode::H264Decoder`]. Other browser codecs are counted as decode
//!   errors and never faked.
//! * Decoded microphone PCM is stored and counted; it is not injected into the
//!   node's UAC2/ALSA target. Routing it belongs to the audio bridge.
//! * [`ClientCaptureSource`] converts the decoded I420 picture to NV12 at the
//!   `ozma-media` boundary because `FrameFormat` has no I420 variant. It does
//!   not invent a frame before the browser sends one; it waits instead.
//! * USB UVC injection is not implemented here.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use bytes::Bytes;
use ozma_api::state::{CaptureAudioStats, CaptureReport, CaptureVideoStats};
use ozma_decode::{
    AudioDecoder, DecodeMeta, DecodedAudio, DecodedVideoFrame, H264Decoder, VideoDecoder,
};
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{ApiError, TrackId};
use tokio::sync::{mpsc, Notify};
use tokio::task::JoinHandle;

/// Stable source id used when the browser camera is selected as a track.
pub const BROWSER_CAPTURE_ID: &str = "browser-camera";
/// Track id advertised for the browser camera channel.
pub const BROWSER_CAMERA_TRACK: &str = "camera0";
/// Largest accepted reassembled camera access unit. A single fragment is
/// limited by the channel's `max_payload`; this bounds the reassembled frame.
pub const MAX_CAMERA_ACCESS_UNIT: usize = 2 * 1024 * 1024;
/// Largest accepted Opus packet. libopus packets are far smaller; the limit
/// keeps a hostile client from growing the audio worker's queue.
pub const MAX_AUDIO_PACKET: usize = 8 * 1024;
/// Stable protocol code for a browser media payload rejected before decode.
pub const CLIENT_CAPTURE_REJECTED: &str = "capture.rejected";
/// Stable protocol code for an audio decode failure.
pub const AUDIO_DECODE_FAILED: &str = "decode.audio_failed";

const CAMERA_QUEUE: usize = 4;
const AUDIO_QUEUE: usize = 16;
const AUDIO_RING: usize = 32;
const FRAME_INTERVAL: Duration = Duration::from_millis(33);
const SOURCE_POLL: Duration = Duration::from_millis(10);
const DEFAULT_CODEC: &str = "h264";

struct VideoWork {
    sequence: u64,
    keyframe: bool,
    pts_micros: u64,
    data: Bytes,
}

struct AudioWork {
    sequence: u64,
    pts_micros: u64,
    data: Vec<u8>,
}

struct Inner {
    video_started: AtomicBool,
    audio_started: AtomicBool,

    video_packets: AtomicU64,
    video_frames: AtomicU64,
    video_bytes: AtomicU64,
    video_decode_errors: AtomicU64,
    video_dropped: AtomicU64,
    last_sequence: AtomicU64,
    width: AtomicU32,
    height: AtomicU32,
    codec: Mutex<String>,

    audio_packets: AtomicU64,
    audio_bytes: AtomicU64,
    audio_samples: AtomicU64,
    audio_decode_errors: AtomicU64,
    audio_dropped: AtomicU64,

    rejected: AtomicU64,
    latest_frame: Mutex<Option<Arc<DecodedVideoFrame>>>,
    audio_ring: Mutex<VecDeque<DecodedAudio>>,
    last_error: Mutex<Option<ApiError>>,
    frame_notify: Notify,

    video_tx: Mutex<Option<mpsc::Sender<VideoWork>>>,
    video_task: Mutex<Option<JoinHandle<()>>>,
    audio_tx: Mutex<Option<mpsc::Sender<AudioWork>>>,
    audio_task: Mutex<Option<JoinHandle<()>>>,
}

impl Inner {
    fn new() -> Self {
        Self {
            video_started: AtomicBool::new(false),
            audio_started: AtomicBool::new(false),
            video_packets: AtomicU64::new(0),
            video_frames: AtomicU64::new(0),
            video_bytes: AtomicU64::new(0),
            video_decode_errors: AtomicU64::new(0),
            video_dropped: AtomicU64::new(0),
            last_sequence: AtomicU64::new(0),
            width: AtomicU32::new(0),
            height: AtomicU32::new(0),
            codec: Mutex::new(DEFAULT_CODEC.to_owned()),
            audio_packets: AtomicU64::new(0),
            audio_bytes: AtomicU64::new(0),
            audio_samples: AtomicU64::new(0),
            audio_decode_errors: AtomicU64::new(0),
            audio_dropped: AtomicU64::new(0),
            rejected: AtomicU64::new(0),
            latest_frame: Mutex::new(None),
            audio_ring: Mutex::new(VecDeque::with_capacity(AUDIO_RING)),
            last_error: Mutex::new(None),
            frame_notify: Notify::new(),
            video_tx: Mutex::new(None),
            video_task: Mutex::new(None),
            audio_tx: Mutex::new(None),
            audio_task: Mutex::new(None),
        }
    }

    fn record_error(&self, error: ApiError) {
        *self.last_error.lock().expect("client capture error lock") = Some(error);
    }
}

/// A per-session browser camera/microphone capture hub.
///
/// Clones share the same decoders, queues, and counters. The handle is cheap
/// to pass to the transport, the HTTP capture sink, and a track source.
#[derive(Clone)]
pub struct ClientCaptureHub {
    inner: Arc<Inner>,
}

impl Default for ClientCaptureHub {
    fn default() -> Self {
        Self::new()
    }
}

impl std::fmt::Debug for ClientCaptureHub {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let report = self.report();
        formatter
            .debug_struct("ClientCaptureHub")
            .field("video_started", &report.video.started)
            .field("audio_started", &report.audio.started)
            .field("video_frames", &report.video.frames)
            .field("audio_packets", &report.audio.packets)
            .finish()
    }
}

impl ClientCaptureHub {
    /// Create an idle hub. Workers are created lazily by [`Self::start`] or the
    /// first accepted packet.
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner::new()),
        }
    }

    /// Record a client `CaptureStart` and create the requested workers.
    pub fn start(&self, video: bool, audio: bool) -> Result<(), ApiError> {
        if video {
            self.inner.video_started.store(true, Ordering::Release);
            self.ensure_video_worker()?;
        }
        if audio {
            self.inner.audio_started.store(true, Ordering::Release);
            self.ensure_audio_worker()?;
        }
        Ok(())
    }

    /// Record a client `CaptureStop` and stop the requested workers.
    ///
    /// Counters are preserved so the HTTP capture endpoint keeps reporting the
    /// final totals after a stop.
    pub fn stop(&self, video: bool, audio: bool) {
        if video {
            self.inner.video_started.store(false, Ordering::Release);
            if let Some(task) = self
                .inner
                .video_task
                .lock()
                .expect("client capture video task lock")
                .take()
            {
                task.abort();
            }
            self.inner
                .video_tx
                .lock()
                .expect("client capture video sender lock")
                .take();
            self.inner.frame_notify.notify_waiters();
        }
        if audio {
            self.inner.audio_started.store(false, Ordering::Release);
            if let Some(task) = self
                .inner
                .audio_task
                .lock()
                .expect("client capture audio task lock")
                .take()
            {
                task.abort();
            }
            self.inner
                .audio_tx
                .lock()
                .expect("client capture audio sender lock")
                .take();
        }
    }

    /// Apply a client-reported `CaptureState`.
    pub fn apply_state(
        &self,
        video: bool,
        audio: bool,
        codec: Option<&str>,
        width: Option<u16>,
        height: Option<u16>,
    ) {
        self.inner.video_started.store(video, Ordering::Release);
        self.inner.audio_started.store(audio, Ordering::Release);
        if let Some(codec) = codec.filter(|codec| !codec.trim().is_empty()) {
            *self.inner.codec.lock().expect("client capture codec lock") = codec.to_owned();
        }
        if let Some(width) = width {
            self.inner.width.store(u32::from(width), Ordering::Relaxed);
        }
        if let Some(height) = height {
            self.inner
                .height
                .store(u32::from(height), Ordering::Relaxed);
        }
    }

    /// Record a client-reported capture error. It is surfaced through
    /// [`Self::last_error`] and does not tear down the session.
    pub fn record_error(&self, error: ApiError) {
        self.inner.record_error(error);
    }

    /// Count a payload rejected before decode (for example a malformed
    /// fragment that never became an access unit).
    pub fn record_rejected(&self) {
        self.inner.rejected.fetch_add(1, Ordering::Relaxed);
    }

    /// Submit one complete camera access unit.
    ///
    /// `frame_sequence` and `keyframe` come from the
    /// [`ozma_protocol::media::VideoFragmentHeader`] reassembly. A full decode
    /// queue drops the newest unit and increments a counter; the caller is
    /// never blocked.
    pub fn push_video(
        &self,
        frame_sequence: u64,
        keyframe: bool,
        access_unit: Bytes,
    ) -> Result<(), ApiError> {
        if access_unit.is_empty() {
            self.inner.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::new(
                CLIENT_CAPTURE_REJECTED,
                "The browser camera sent an empty access unit.",
            ));
        }
        if access_unit.len() > MAX_CAMERA_ACCESS_UNIT {
            self.inner.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::new(
                CLIENT_CAPTURE_REJECTED,
                "The browser camera access unit is too large.",
            )
            .with_details(format!(
                "{} bytes exceeds the {} byte limit",
                access_unit.len(),
                MAX_CAMERA_ACCESS_UNIT
            )));
        }

        self.ensure_video_worker()?;
        self.inner.video_packets.fetch_add(1, Ordering::Relaxed);
        self.inner
            .video_bytes
            .fetch_add(access_unit.len() as u64, Ordering::Relaxed);
        self.inner
            .last_sequence
            .store(frame_sequence, Ordering::Relaxed);

        let work = VideoWork {
            sequence: frame_sequence,
            keyframe,
            pts_micros: now_micros(),
            data: access_unit,
        };
        let sender = self
            .inner
            .video_tx
            .lock()
            .expect("client capture video sender lock")
            .clone();
        let Some(sender) = sender else {
            return Err(ApiError::internal(
                "The browser camera decode worker is not running.",
            ));
        };
        match sender.try_send(work) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.inner.video_dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                *self
                    .inner
                    .video_tx
                    .lock()
                    .expect("client capture video sender lock") = None;
                Err(ApiError::internal(
                    "The browser camera decode worker stopped unexpectedly.",
                ))
            }
        }
    }

    /// Submit one raw Opus packet from the browser microphone.
    pub fn push_audio(&self, packet: Bytes) -> Result<(), ApiError> {
        if packet.is_empty() {
            self.inner.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::new(
                CLIENT_CAPTURE_REJECTED,
                "The browser microphone sent an empty Opus packet.",
            ));
        }
        if packet.len() > MAX_AUDIO_PACKET {
            self.inner.rejected.fetch_add(1, Ordering::Relaxed);
            return Err(ApiError::new(
                CLIENT_CAPTURE_REJECTED,
                "The browser microphone packet is too large.",
            )
            .with_details(format!(
                "{} bytes exceeds the {} byte limit",
                packet.len(),
                MAX_AUDIO_PACKET
            )));
        }

        self.ensure_audio_worker()?;
        self.inner.audio_packets.fetch_add(1, Ordering::Relaxed);
        self.inner
            .audio_bytes
            .fetch_add(packet.len() as u64, Ordering::Relaxed);

        let sequence = self.inner.audio_packets.load(Ordering::Relaxed);
        let work = AudioWork {
            sequence,
            pts_micros: now_micros(),
            data: packet.to_vec(),
        };
        let sender = self
            .inner
            .audio_tx
            .lock()
            .expect("client capture audio sender lock")
            .clone();
        let Some(sender) = sender else {
            return Err(ApiError::internal(
                "The browser microphone decode worker is not running.",
            ));
        };
        match sender.try_send(work) {
            Ok(()) => Ok(()),
            Err(mpsc::error::TrySendError::Full(_)) => {
                self.inner.audio_dropped.fetch_add(1, Ordering::Relaxed);
                Ok(())
            }
            Err(mpsc::error::TrySendError::Closed(_)) => {
                *self
                    .inner
                    .audio_tx
                    .lock()
                    .expect("client capture audio sender lock") = None;
                Err(ApiError::internal(
                    "The browser microphone decode worker stopped unexpectedly.",
                ))
            }
        }
    }

    /// A snapshot of the counters and negotiated metadata.
    pub fn report(&self) -> CaptureReport {
        CaptureReport {
            video: CaptureVideoStats {
                started: self.inner.video_started.load(Ordering::Acquire),
                packets: self.inner.video_packets.load(Ordering::Relaxed),
                frames: self.inner.video_frames.load(Ordering::Relaxed),
                bytes: self.inner.video_bytes.load(Ordering::Relaxed),
                width: self.inner.width.load(Ordering::Relaxed),
                height: self.inner.height.load(Ordering::Relaxed),
                codec: self
                    .inner
                    .codec
                    .lock()
                    .expect("client capture codec lock")
                    .clone(),
                decode_errors: self.inner.video_decode_errors.load(Ordering::Relaxed),
                last_sequence: self.inner.last_sequence.load(Ordering::Relaxed),
            },
            audio: CaptureAudioStats {
                started: self.inner.audio_started.load(Ordering::Acquire),
                packets: self.inner.audio_packets.load(Ordering::Relaxed),
                bytes: self.inner.audio_bytes.load(Ordering::Relaxed),
                samples: self.inner.audio_samples.load(Ordering::Relaxed),
                decode_errors: self.inner.audio_decode_errors.load(Ordering::Relaxed),
            },
        }
    }

    /// The most recently decoded camera frame, if one has arrived.
    pub fn latest_frame(&self) -> Option<Arc<DecodedVideoFrame>> {
        self.inner
            .latest_frame
            .lock()
            .expect("client capture frame lock")
            .clone()
    }

    /// The latest decoded frame as a binary PPM (`P6`), if one has arrived.
    pub fn latest_ppm(&self) -> Option<Vec<u8>> {
        self.latest_frame().map(|frame| frame.to_ppm())
    }

    /// The last decoded microphone block, if any.
    pub fn latest_audio(&self) -> Option<DecodedAudio> {
        self.inner
            .audio_ring
            .lock()
            .expect("client capture audio ring lock")
            .back()
            .cloned()
    }

    /// Remove up to `max` decoded microphone blocks in arrival order.
    pub fn drain_audio(&self, max: usize) -> Vec<DecodedAudio> {
        let mut ring = self
            .inner
            .audio_ring
            .lock()
            .expect("client capture audio ring lock");
        let take = max.min(ring.len());
        ring.drain(..take).collect()
    }

    /// Packet drops caused by a full decode queue.
    pub fn video_dropped(&self) -> u64 {
        self.inner.video_dropped.load(Ordering::Relaxed)
    }

    /// Audio packet drops caused by a full decode queue.
    pub fn audio_dropped(&self) -> u64 {
        self.inner.audio_dropped.load(Ordering::Relaxed)
    }

    /// Payloads rejected before decode (empty, oversized, or malformed).
    pub fn rejected(&self) -> u64 {
        self.inner.rejected.load(Ordering::Relaxed)
    }

    /// The most recent typed capture/decode error, if any.
    pub fn last_error(&self) -> Option<ApiError> {
        self.inner
            .last_error
            .lock()
            .expect("client capture error lock")
            .clone()
    }

    /// Build a capture source that follows this hub's latest camera frame.
    pub fn source(&self) -> ClientCaptureSource {
        ClientCaptureSource::new(self.clone())
    }

    fn ensure_video_worker(&self) -> Result<(), ApiError> {
        let mut slot = self
            .inner
            .video_tx
            .lock()
            .expect("client capture video sender lock");
        if slot.is_some() {
            return Ok(());
        }
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| ApiError::internal("The browser camera decoder needs a Tokio runtime."))?;
        let (sender, receiver) = mpsc::channel(CAMERA_QUEUE);
        let inner = Arc::clone(&self.inner);
        let task = runtime.spawn(video_worker(inner, receiver));
        *slot = Some(sender);
        *self
            .inner
            .video_task
            .lock()
            .expect("client capture video task lock") = Some(task);
        Ok(())
    }

    fn ensure_audio_worker(&self) -> Result<(), ApiError> {
        let mut slot = self
            .inner
            .audio_tx
            .lock()
            .expect("client capture audio sender lock");
        if slot.is_some() {
            return Ok(());
        }
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            ApiError::internal("The browser microphone decoder needs a Tokio runtime.")
        })?;
        let (sender, receiver) = mpsc::channel(AUDIO_QUEUE);
        let inner = Arc::clone(&self.inner);
        let task = runtime.spawn(audio_worker(inner, receiver));
        *slot = Some(sender);
        *self
            .inner
            .audio_task
            .lock()
            .expect("client capture audio task lock") = Some(task);
        Ok(())
    }
}

async fn video_worker(inner: Arc<Inner>, mut receiver: mpsc::Receiver<VideoWork>) {
    let mut decoder = match H264Decoder::new() {
        Ok(decoder) => decoder,
        Err(error) => {
            inner.video_decode_errors.fetch_add(1, Ordering::Relaxed);
            inner.record_error(error.to_api_error());
            return;
        }
    };

    while let Some(work) = receiver.recv().await {
        let meta = DecodeMeta::new(work.pts_micros, work.sequence, work.keyframe);
        match decoder.decode(&work.data, meta).await {
            Ok(Some(frame)) => {
                inner.width.store(frame.width, Ordering::Relaxed);
                inner.height.store(frame.height, Ordering::Relaxed);
                *inner
                    .latest_frame
                    .lock()
                    .expect("client capture frame lock") = Some(Arc::new(frame));
                inner.video_frames.fetch_add(1, Ordering::Relaxed);
                inner.frame_notify.notify_waiters();
            }
            Ok(None) => {
                // A parameter-set-only access unit is normal and produces no
                // picture. It is not an error.
            }
            Err(error) => {
                inner.video_decode_errors.fetch_add(1, Ordering::Relaxed);
                inner.record_error(error.to_api_error());
            }
        }
    }
}

async fn audio_worker(inner: Arc<Inner>, mut receiver: mpsc::Receiver<AudioWork>) {
    let mut decoder = match AudioDecoder::new() {
        Ok(decoder) => decoder,
        Err(error) => {
            inner.audio_decode_errors.fetch_add(1, Ordering::Relaxed);
            inner.record_error(ApiError::new(
                AUDIO_DECODE_FAILED,
                format!("The browser microphone decoder is unavailable: {error}"),
            ));
            return;
        }
    };

    while let Some(work) = receiver.recv().await {
        match decoder.decode_bytes(&work.data, work.pts_micros, work.sequence) {
            Ok(decoded) => {
                inner
                    .audio_samples
                    .fetch_add(decoded.samples.len() as u64, Ordering::Relaxed);
                let mut ring = inner
                    .audio_ring
                    .lock()
                    .expect("client capture audio ring lock");
                if ring.len() >= AUDIO_RING {
                    ring.pop_front();
                }
                ring.push_back(decoded);
            }
            Err(error) => {
                inner.audio_decode_errors.fetch_add(1, Ordering::Relaxed);
                inner.record_error(ApiError::new(
                    AUDIO_DECODE_FAILED,
                    format!("The browser microphone packet could not be decoded: {error}"),
                ));
            }
        }
    }
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

/// A capture source that repeats the latest decoded browser camera frame at a
/// 30 Hz pace. It waits for the first frame rather than fabricating one.
///
/// The media `FrameFormat` has no I420 variant, so the decoded I420 picture is
/// converted to NV12 for this boundary. The source format follows the first
/// decoded frame's dimensions; select/configure it before starting a pipeline.
pub struct ClientCaptureSource {
    hub: ClientCaptureHub,
    format: FrameFormat,
    emitted: u64,
    last_emit: Option<Instant>,
    next_pts_micros: u64,
    stopped: bool,
}

impl ClientCaptureSource {
    /// Create a source with the conventional 1280x720 NV12 format.
    pub fn new(hub: ClientCaptureHub) -> Self {
        Self::with_format(hub, FrameFormat::new(PixelFormat::Nv12, 1280, 720))
    }

    /// Create a source with an explicit initial format.
    pub fn with_format(hub: ClientCaptureHub, format: FrameFormat) -> Self {
        Self {
            hub,
            format,
            emitted: 0,
            last_emit: None,
            next_pts_micros: 0,
            stopped: false,
        }
    }

    /// The hub this source reads from.
    pub fn hub(&self) -> &ClientCaptureHub {
        &self.hub
    }
}

impl std::fmt::Debug for ClientCaptureSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientCaptureSource")
            .field("format", &self.format)
            .field("emitted", &self.emitted)
            .field("stopped", &self.stopped)
            .finish()
    }
}

#[async_trait]
impl CaptureSource for ClientCaptureSource {
    fn id(&self) -> &str {
        BROWSER_CAPTURE_ID
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        loop {
            if self.stopped {
                return Err(MediaError::Closed {
                    id: BROWSER_CAPTURE_ID.to_owned(),
                });
            }

            if let Some(frame) = self.hub.latest_frame() {
                let due = self
                    .last_emit
                    .map(|last| last.elapsed() >= FRAME_INTERVAL)
                    .unwrap_or(true);
                if due {
                    let data = i420_to_nv12(&frame)?;
                    let format = FrameFormat::new(PixelFormat::Nv12, frame.width, frame.height);
                    self.format = format;
                    let pts_micros = self.next_pts_micros.max(frame.pts_micros);
                    self.next_pts_micros = pts_micros.saturating_add(33_333);
                    let sequence = self.emitted;
                    self.emitted = self.emitted.saturating_add(1);
                    self.last_emit = Some(Instant::now());
                    return Frame::with_data(format, pts_micros, sequence, true, data);
                }
            }

            // No frame yet, or the repeat interval has not elapsed. Sleeping
            // keeps the source cancellable through the pipeline's `stop` hook
            // and never fabricates a picture.
            tokio::time::sleep(SOURCE_POLL).await;
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        self.stopped = true;
        Ok(())
    }
}

fn i420_to_nv12(frame: &DecodedVideoFrame) -> Result<Vec<u8>, MediaError> {
    frame
        .yuv
        .validate(frame.width, frame.height)
        .map_err(|error| MediaError::InvalidFormat(error.to_string()))?;

    let width = frame.width as usize;
    let height = frame.height as usize;
    let chroma_width = width.div_ceil(2);
    let chroma_height = height.div_ceil(2);
    let mut nv12 = Vec::with_capacity(width * height + chroma_width * chroma_height * 2);
    nv12.extend_from_slice(&frame.yuv.y);
    for row in 0..chroma_height {
        for column in 0..chroma_width {
            let index = row * chroma_width + column;
            nv12.push(frame.yuv.u[index]);
            nv12.push(frame.yuv.v[index]);
        }
    }
    Ok(nv12)
}

/// The track id advertised by the camera channel descriptor.
pub fn camera_track_id() -> TrackId {
    TrackId::new(BROWSER_CAMERA_TRACK)
}

/// The protocol error code used when a camera payload is rejected before decode.
pub fn rejected_code() -> &'static str {
    CLIENT_CAPTURE_REJECTED
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn oversized_camera_units_are_rejected_without_panicking() {
        let hub = ClientCaptureHub::new();
        hub.start(true, false).unwrap();
        let error = hub
            .push_video(1, true, Bytes::from(vec![0u8; MAX_CAMERA_ACCESS_UNIT + 1]))
            .unwrap_err();
        assert_eq!(error.code, CLIENT_CAPTURE_REJECTED);
        assert_eq!(hub.rejected(), 1);
        assert_eq!(hub.report().video.decode_errors, 0);
    }

    #[tokio::test]
    async fn audio_packets_are_bounded_and_counted() {
        let hub = ClientCaptureHub::new();
        hub.start(false, true).unwrap();
        let error = hub
            .push_audio(Bytes::from(vec![0u8; MAX_AUDIO_PACKET + 1]))
            .unwrap_err();
        assert_eq!(error.code, CLIENT_CAPTURE_REJECTED);
        assert_eq!(hub.rejected(), 1);
    }

    #[tokio::test]
    async fn a_source_waits_instead_of_fabricating_a_frame() {
        let hub = ClientCaptureHub::new();
        let mut source = hub.source();
        let result = tokio::time::timeout(Duration::from_millis(50), source.next_frame()).await;
        assert!(
            result.is_err(),
            "the source must wait until the browser sends a frame"
        );
        source.stop().await.unwrap();
        assert!(matches!(
            source.next_frame().await,
            Err(MediaError::Closed { .. })
        ));
    }

    #[tokio::test]
    async fn report_uses_the_http_shape() {
        let hub = ClientCaptureHub::new();
        hub.start(true, true).unwrap();
        let json = serde_json::to_value(hub.report()).unwrap();
        assert_eq!(json["video"]["codec"], "h264");
        assert_eq!(json["video"]["packets"], 0);
        assert!(json["video"]["started"].as_bool().unwrap());
        assert_eq!(json["audio"]["samples"], 0);
    }
}
