// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded MJPEG `/video` stream support.
//!
//! The Python soft node served `multipart/x-mixed-replace` at
//! `/display/mjpeg`; the Rust node previously advertised a typed 501. This
//! module supplies the real stream while keeping the same honesty rules as the
//! snapshot path:
//!
//! * frames come from [`crate::display_compat::SnapshotControl`], so every
//!   JPEG is encoded (or passed through) by the same capture registry source as
//!   `GET /display/snapshot`;
//! * no frame is invented when no source exists — the HTTP layer returns the
//!   typed snapshot error before a 200 response is written;
//! * each stream owns one cached capture source and is stopped when the client
//!   disconnects;
//! * at most [`DEFAULT_MAX_STREAMS`] streams run at once; a slow client keeps a
//!   single pending frame and newer frames are dropped instead of buffered;
//! * frame pacing is bounded by the requested `fps` (clamped to
//!   [`MIN_FPS`]..=[`MAX_FPS`]) so a fast fake/file source cannot spin.
//!
//! `?console=` is resolved by the caller through the same bounded console
//! broker used by `/display/snapshot`.

use std::convert::Infallible;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use crate::display_compat::{SnapshotControl, SnapshotFrame};
use crate::hls::{
    start_mpegts_stream, validate_stream_encoder, EncoderProbe, HlsCodec, HlsConfig, HlsEncoder,
    HlsError, HlsPixelFormat, MpegTsSpawner, MpegTsStream, SystemEncoderProbe, SystemMpegTsSpawner,
};
use ozma_media::{CaptureSource, FrameFormat, PixelFormat};

/// Default `/video` frame rate.
pub const DEFAULT_FPS: u32 = 10;
/// Lowest accepted `/video` frame rate.
pub const MIN_FPS: u32 = 1;
/// Highest accepted `/video` frame rate.
pub const MAX_FPS: u32 = 30;
/// Default JPEG quality for `/video` frames.
pub const DEFAULT_QUALITY: u8 = 85;
/// Lowest accepted JPEG quality.
pub const MIN_QUALITY: u8 = 1;
/// Highest accepted JPEG quality.
pub const MAX_QUALITY: u8 = 100;
/// Default H.264/HEVC bitrate in kbps.
pub const DEFAULT_BITRATE_KBPS: u32 = 4_000;
/// Lowest accepted H.264/HEVC bitrate in kbps.
pub const MIN_BITRATE_KBPS: u32 = 100;
/// Highest accepted H.264/HEVC bitrate in kbps.
pub const MAX_BITRATE_KBPS: u32 = 50_000;
/// Maximum number of simultaneous `/video` streams.
pub const DEFAULT_MAX_STREAMS: usize = 4;

/// MIME boundary used by every `/video` MJPEG response.
pub const BOUNDARY: &str = "ozma-frame";
/// `Content-Type` header value for the MJPEG response.
pub const CONTENT_TYPE: &str = "multipart/x-mixed-replace; boundary=ozma-frame";
/// `Content-Type` header value for an H.264/HEVC MPEG-TS response.
pub const MPEGTS_CONTENT_TYPE: &str = "video/mp2t";
/// Canonical stream route.
pub const ROUTE: &str = "/video";
/// Alias matching the Python soft node's MJPEG path.
pub const ALIAS: &str = "/display/mjpeg";
/// Default `/video` codec (keeps the historical MJPEG behavior).
pub const DEFAULT_CODEC: VideoCodec = VideoCodec::Mjpeg;

/// A `/video` frame/stream codec.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    /// `multipart/x-mixed-replace` JPEG frames.
    Mjpeg,
    /// H.264 in MPEG-TS.
    H264,
    /// HEVC/H.265 in MPEG-TS.
    Hevc,
}

impl VideoCodec {
    /// Stable query/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Mjpeg => "mjpeg",
            Self::H264 => "h264",
            Self::Hevc => "hevc",
        }
    }

    /// Parse `?codec=`; unknown values are a typed query error.
    pub fn parse(value: &str) -> Result<Self, VideoQueryError> {
        match value.trim().to_ascii_lowercase().as_str() {
            "mjpeg" | "mjpg" | "jpeg" => Ok(Self::Mjpeg),
            "h264" | "avc" | "h.264" => Ok(Self::H264),
            "hevc" | "h265" | "h.265" => Ok(Self::Hevc),
            other => Err(VideoQueryError {
                field: "codec",
                value: other.to_owned(),
                message: format!("`codec` must be mjpeg, h264, or hevc, not `{other}`."),
            }),
        }
    }

    /// True for the encoded MPEG-TS codecs.
    pub fn is_encoded(self) -> bool {
        !matches!(self, Self::Mjpeg)
    }
}

/// Pending frames per stream.
///
/// One slot is enough to hand the first frame to hyper. If a client stops
/// reading, `try_send` reports the channel as full and the newer frame is
/// dropped. Buffering more than one frame only adds latency and unbounded
/// memory growth.
const STREAM_QUEUE_DEPTH: usize = 1;

/// Raw `/video` query fields.
#[derive(Debug, Default, Deserialize)]
pub struct VideoParams {
    /// Console index, label, `Console_N`, or capture source id.
    #[serde(default)]
    pub console: Option<String>,
    /// Requested stream codec (`mjpeg`, `h264`, or `hevc`).
    #[serde(default)]
    pub codec: Option<String>,
    /// Requested frame rate, clamped to [`MIN_FPS`]..=[`MAX_FPS`].
    #[serde(default)]
    pub fps: Option<String>,
    /// Requested JPEG quality, clamped to [`MIN_QUALITY`]..=[`MAX_QUALITY`].
    #[serde(default)]
    pub quality: Option<String>,
    /// Requested H.264/HEVC bitrate in kbps, clamped to
    /// [`MIN_BITRATE_KBPS`]..=[`MAX_BITRATE_KBPS`].
    #[serde(default)]
    pub bitrate: Option<String>,
}

/// Resolved stream settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoOptions {
    /// Selected codec.
    pub codec: VideoCodec,
    /// Frames per second after clamping.
    pub fps: u32,
    /// JPEG quality after clamping (ignored by H.264/HEVC).
    pub quality: u8,
    /// H.264/HEVC bitrate after clamping (ignored by MJPEG).
    pub bitrate_kbps: u32,
}

impl Default for VideoOptions {
    fn default() -> Self {
        Self {
            codec: DEFAULT_CODEC,
            fps: DEFAULT_FPS,
            quality: DEFAULT_QUALITY,
            bitrate_kbps: DEFAULT_BITRATE_KBPS,
        }
    }
}

/// A non-numeric `fps`/`quality` query value.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct VideoQueryError {
    /// Query field name (`fps` or `quality`).
    pub field: &'static str,
    /// Raw value supplied by the caller.
    pub value: String,
    /// Human-readable explanation.
    pub message: String,
}

impl std::fmt::Display for VideoQueryError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for VideoQueryError {}

impl VideoParams {
    /// Resolve defaults and bounds for this request.
    pub fn options(&self) -> Result<VideoOptions, VideoQueryError> {
        let codec = match self.codec.as_deref() {
            Some(value) => VideoCodec::parse(value)?,
            None => DEFAULT_CODEC,
        };
        let fps = parse_bounded(
            "fps",
            self.fps.as_deref(),
            u64::from(DEFAULT_FPS),
            u64::from(MIN_FPS),
            u64::from(MAX_FPS),
        )?;
        let quality = parse_bounded(
            "quality",
            self.quality.as_deref(),
            u64::from(DEFAULT_QUALITY),
            u64::from(MIN_QUALITY),
            u64::from(MAX_QUALITY),
        )?;
        let bitrate = parse_bounded(
            "bitrate",
            self.bitrate.as_deref(),
            u64::from(DEFAULT_BITRATE_KBPS),
            u64::from(MIN_BITRATE_KBPS),
            u64::from(MAX_BITRATE_KBPS),
        )?;
        Ok(VideoOptions {
            codec,
            fps: fps as u32,
            quality: quality as u8,
            bitrate_kbps: bitrate as u32,
        })
    }
}

fn parse_bounded(
    field: &'static str,
    raw: Option<&str>,
    default: u64,
    min: u64,
    max: u64,
) -> Result<u64, VideoQueryError> {
    let Some(raw) = raw else {
        return Ok(default);
    };
    let value = raw.trim().parse::<u64>().map_err(|_| VideoQueryError {
        field,
        value: raw.to_owned(),
        message: format!("`{field}` must be an integer between {min} and {max}."),
    })?;
    Ok(value.clamp(min, max))
}

struct StreamsInner {
    active: AtomicUsize,
    encoded: AtomicUsize,
    limit: usize,
}

/// Shared, bounded stream accounting for `/video`.
#[derive(Clone)]
pub struct VideoStreams {
    inner: Arc<StreamsInner>,
}

impl Default for VideoStreams {
    fn default() -> Self {
        Self::new(DEFAULT_MAX_STREAMS)
    }
}

impl std::fmt::Debug for VideoStreams {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("VideoStreams")
            .field("active", &self.active())
            .field("encoded", &self.active_encoded())
            .field("limit", &self.limit())
            .finish()
    }
}

impl VideoStreams {
    /// Create accounting with a hard stream cap (clamped to at least one).
    pub fn new(limit: usize) -> Self {
        Self {
            inner: Arc::new(StreamsInner {
                active: AtomicUsize::new(0),
                encoded: AtomicUsize::new(0),
                limit: limit.max(1),
            }),
        }
    }

    /// Maximum simultaneous streams.
    pub fn limit(&self) -> usize {
        self.inner.limit
    }

    /// Streams currently being served.
    pub fn active(&self) -> usize {
        self.inner.active.load(Ordering::SeqCst)
    }

    /// H.264/HEVC MPEG-TS streams currently being served.
    pub fn active_encoded(&self) -> usize {
        self.inner.encoded.load(Ordering::SeqCst)
    }

    /// Reserve an MJPEG stream slot, or return `None` when the cap is reached.
    pub fn try_start(&self) -> Option<VideoStreamGuard> {
        self.reserve(false)
    }

    /// Reserve an H.264/HEVC stream slot.
    ///
    /// The encoded-stream count is additive status; the cap is shared with
    /// MJPEG so `/video` never exceeds [`Self::limit`] concurrent sources.
    pub fn try_start_encoded(&self) -> Option<VideoStreamGuard> {
        self.reserve(true)
    }

    fn reserve(&self, encoded: bool) -> Option<VideoStreamGuard> {
        let mut current = self.inner.active.load(Ordering::SeqCst);
        loop {
            if current >= self.inner.limit {
                return None;
            }
            match self.inner.active.compare_exchange(
                current,
                current + 1,
                Ordering::SeqCst,
                Ordering::SeqCst,
            ) {
                Ok(_) => {
                    if encoded {
                        self.inner.encoded.fetch_add(1, Ordering::SeqCst);
                    }
                    return Some(VideoStreamGuard {
                        streams: self.clone(),
                        encoded,
                    });
                }
                Err(observed) => current = observed,
            }
        }
    }
}

/// RAII reservation for one active stream.
///
/// Dropping the guard (normally when the response body/task ends) returns the
/// slot to [`VideoStreams`].
#[derive(Debug)]
pub struct VideoStreamGuard {
    streams: VideoStreams,
    encoded: bool,
}

impl VideoStreamGuard {
    /// Accounting shared with the owning state.
    pub fn streams(&self) -> &VideoStreams {
        &self.streams
    }

    /// True for an H.264/HEVC MPEG-TS reservation.
    pub fn is_encoded(&self) -> bool {
        self.encoded
    }
}

impl Drop for VideoStreamGuard {
    fn drop(&mut self) {
        if self.encoded {
            self.streams.inner.encoded.fetch_sub(1, Ordering::SeqCst);
        }
        self.streams.inner.active.fetch_sub(1, Ordering::SeqCst);
    }
}

/// `/video` facts for `/api/v1/connection`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct VideoStatus {
    /// The route is implemented on this node.
    pub supported: bool,
    /// A snapshot/capture control is configured, so the route can try to open
    /// a real source. It still returns a typed 503 when enumeration finds none.
    pub available: bool,
    /// Canonical stream route.
    pub route: &'static str,
    /// MJPEG alias.
    pub alias: &'static str,
    /// Default frame encoding (`jpeg`).
    pub encoding: &'static str,
    /// Accepted `?codec=` values. `h264`/`hevc` are omitted when this node has
    /// no encoder configuration.
    pub codecs: Vec<&'static str>,
    /// Codec used when `?codec=` is absent.
    pub default_codec: &'static str,
    /// Number of streams currently being served (MJPEG + encoded).
    pub active_streams: usize,
    /// H.264/HEVC MPEG-TS streams currently being served.
    pub active_encoded_streams: usize,
    /// Hard concurrent-stream cap shared by every codec.
    pub max_streams: usize,
    /// Default frames per second.
    pub default_fps: u32,
    /// Lowest accepted frames per second.
    pub min_fps: u32,
    /// Highest accepted frames per second.
    pub max_fps: u32,
    /// Default JPEG quality.
    pub default_quality: u8,
    /// Default H.264/HEVC bitrate in kbps.
    pub default_bitrate_kbps: u32,
    /// Lowest accepted H.264/HEVC bitrate in kbps.
    pub min_bitrate_kbps: u32,
    /// Highest accepted H.264/HEVC bitrate in kbps.
    pub max_bitrate_kbps: u32,
}

impl VideoStatus {
    /// Describe the route without claiming an active stream when idle.
    pub fn new(available: bool, encoding_available: bool, streams: &VideoStreams) -> Self {
        let mut codecs = vec![VideoCodec::Mjpeg.as_str()];
        if encoding_available {
            codecs.push(VideoCodec::H264.as_str());
            codecs.push(VideoCodec::Hevc.as_str());
        }
        Self {
            supported: true,
            available,
            route: ROUTE,
            alias: ALIAS,
            encoding: "jpeg",
            codecs,
            default_codec: DEFAULT_CODEC.as_str(),
            active_streams: streams.active(),
            active_encoded_streams: streams.active_encoded(),
            max_streams: streams.limit(),
            default_fps: DEFAULT_FPS,
            min_fps: MIN_FPS,
            max_fps: MAX_FPS,
            default_quality: DEFAULT_QUALITY,
            default_bitrate_kbps: DEFAULT_BITRATE_KBPS,
            min_bitrate_kbps: MIN_BITRATE_KBPS,
            max_bitrate_kbps: MAX_BITRATE_KBPS,
        }
    }
}

/// A started H.264/HEVC `/video` stream plus the encoder that is actually
/// running (which may be a software fallback from a configured hardware
/// backend).
pub struct StartedVideoStream {
    /// Live MPEG-TS stream.
    pub stream: MpegTsStream,
    /// ffmpeg encoder selected for this stream (or the source family for a
    /// compressed pass-through).
    pub encoder: HlsEncoder,
    /// True when the source bitstream is muxed without re-encoding.
    pub stream_copy: bool,
}

/// Encoder configuration for `GET /video?codec=h264|hevc`.
///
/// It inherits the node's HLS encoder/backend preference, pixel format
/// validation, ffmpeg binary, VAAPI device, and NVENC GPU selection. MJPEG
/// continues to use [`SnapshotControl`] and never enters this path.
#[derive(Clone)]
pub struct VideoEncoding {
    template: HlsConfig,
    probe: Arc<dyn EncoderProbe>,
    spawner: Arc<dyn MpegTsSpawner>,
}

impl VideoEncoding {
    /// Production configuration: system ffmpeg spawner and encoder probe.
    pub fn system(template: HlsConfig) -> Self {
        Self {
            template,
            probe: Arc::new(SystemEncoderProbe::new()),
            spawner: Arc::new(SystemMpegTsSpawner),
        }
    }

    /// Test/embedding constructor with an injected spawner and probe.
    pub fn with_components(
        template: HlsConfig,
        probe: Arc<dyn EncoderProbe>,
        spawner: Arc<dyn MpegTsSpawner>,
    ) -> Self {
        Self {
            template,
            probe,
            spawner,
        }
    }

    /// Base HLS-style configuration (fps/bitrate are overridden per request).
    pub fn template(&self) -> &HlsConfig {
        &self.template
    }

    /// Start an encoded MPEG-TS stream from an already-opened source.
    ///
    /// A compressed H.264/HEVC source whose family matches `codec` is copied
    /// without running an encoder. Otherwise the requested codec is encoded
    /// with the configured backend when available, falling back to the
    /// matching software encoder. A missing encoder is returned before any
    /// response is committed, and the source is stopped on every error path.
    pub async fn start_encoded(
        &self,
        codec: VideoCodec,
        options: &VideoOptions,
        source: Box<dyn CaptureSource>,
    ) -> Result<StartedVideoStream, HlsError> {
        let mut source = source;
        let Some(family) = codec_family(codec) else {
            let error = HlsError::UnsupportedStreamCodec {
                encoder: codec.as_str().to_owned(),
            };
            let _ = source.stop().await;
            return Err(error);
        };
        let declared = source.format();
        let stream_copy = source_stream_copy(codec, declared);

        let mut config = self.template.clone();
        config.fps = options.fps;
        config.bitrate_kbps = options.bitrate_kbps;
        // `/video` is the browser-compatible 8-bit SDR path; the HLS
        // configuration may be a 10-bit/HDR target, which is not inherited
        // here.
        config.pixel_format = HlsPixelFormat::Yuv420p;
        config.hdr = false;
        config.color_primaries = None;
        config.color_transfer = None;
        config.color_matrix = None;
        config.width = None;
        config.height = None;

        let preferred = preferred_encoder(self.template.video_encoder, family);
        let encoder = if stream_copy {
            preferred
        } else {
            match self.resolve_encoder(preferred, family, &config) {
                Ok(encoder) => encoder,
                Err(error) => {
                    let _ = source.stop().await;
                    return Err(error);
                }
            }
        };
        config.video_encoder = encoder;
        let stream =
            start_mpegts_stream(source, config, self.spawner.clone(), self.probe.clone()).await?;
        Ok(StartedVideoStream {
            stream,
            encoder,
            stream_copy,
        })
    }

    fn resolve_encoder(
        &self,
        preferred: HlsEncoder,
        family: HlsCodec,
        config: &HlsConfig,
    ) -> Result<HlsEncoder, HlsError> {
        let mut candidate = config.clone();
        candidate.video_encoder = preferred;
        match validate_stream_encoder(&candidate, self.probe.as_ref()) {
            Ok(()) => Ok(preferred),
            Err(_error) if preferred.is_hardware() => {
                let fallback = preferred
                    .software_fallback()
                    .unwrap_or_else(|| HlsEncoder::software_for_codec(family));
                candidate.video_encoder = fallback;
                validate_stream_encoder(&candidate, self.probe.as_ref())?;
                Ok(fallback)
            }
            Err(error) => Err(error),
        }
    }
}

fn codec_family(codec: VideoCodec) -> Option<HlsCodec> {
    match codec {
        VideoCodec::Mjpeg => None,
        VideoCodec::H264 => Some(HlsCodec::H264),
        VideoCodec::Hevc => Some(HlsCodec::Hevc),
    }
}

/// Keep the configured hardware backend but switch to the requested codec
/// family. A software/AV1/MJPEG preference falls back to the matching software
/// encoder.
fn preferred_encoder(preferred: HlsEncoder, family: HlsCodec) -> HlsEncoder {
    match (preferred.backend_label(), family) {
        ("vaapi", HlsCodec::H264) => HlsEncoder::VaapiH264,
        ("vaapi", HlsCodec::Hevc) => HlsEncoder::VaapiHevc,
        ("nvenc", HlsCodec::H264) => HlsEncoder::NvencH264,
        ("nvenc", HlsCodec::Hevc) => HlsEncoder::NvencHevc,
        ("qsv", HlsCodec::H264) => HlsEncoder::QsvH264,
        ("qsv", HlsCodec::Hevc) => HlsEncoder::QsvHevc,
        ("v4l2m2m", HlsCodec::H264) => HlsEncoder::V4l2m2mH264,
        ("v4l2m2m", HlsCodec::Hevc) => HlsEncoder::V4l2m2mHevc,
        _ => HlsEncoder::software_for_codec(family),
    }
}

fn source_stream_copy(codec: VideoCodec, format: FrameFormat) -> bool {
    matches!(
        (codec, format.pixel_format),
        (VideoCodec::H264, PixelFormat::H264) | (VideoCodec::Hevc, PixelFormat::Hevc)
    )
}
pub fn part(jpeg: &[u8]) -> Bytes {
    let length = jpeg.len().to_string();
    let mut out = Vec::with_capacity(jpeg.len() + BOUNDARY.len() + length.len() + 64);
    out.extend_from_slice(b"--");
    out.extend_from_slice(BOUNDARY.as_bytes());
    out.extend_from_slice(b"\r\nContent-Type: image/jpeg\r\nContent-Length: ");
    out.extend_from_slice(length.as_bytes());
    out.extend_from_slice(b"\r\n\r\n");
    out.extend_from_slice(jpeg);
    out.extend_from_slice(b"\r\n");
    Bytes::from(out)
}

/// Start the frame-producing task for one stream.
///
/// `first` must already be a real encoded frame; the HTTP handler returns a
/// typed error instead of calling this when capture fails. The returned
/// receiver yields encoded multipart parts. When the receiver is dropped, the
/// task stops the cached capture source and releases the stream slot.
pub fn spawn_stream(
    control: SnapshotControl,
    first: SnapshotFrame,
    options: VideoOptions,
    guard: VideoStreamGuard,
) -> (mpsc::Receiver<Result<Bytes, Infallible>>, JoinHandle<()>) {
    let (sender, receiver) = mpsc::channel(STREAM_QUEUE_DEPTH);
    let period = Duration::from_secs_f64(1.0 / f64::from(options.fps.max(MIN_FPS)));
    let task = tokio::spawn(async move {
        let _guard = guard;
        if sender.try_send(Ok(part(&first.jpeg))).is_err() {
            control.shutdown().await;
            return;
        }

        let mut deadline = tokio::time::Instant::now() + period;
        loop {
            tokio::select! {
                _ = sender.closed() => break,
                _ = tokio::time::sleep_until(deadline) => {}
            }

            let frame = tokio::select! {
                result = control.capture() => result,
                _ = sender.closed() => break,
            };
            match frame {
                Ok(snapshot) => {
                    // A slow reader gets the newest frame that fits in the
                    // single pending slot. `try_send` drops this frame instead
                    // of growing an unbounded queue.
                    let _ = sender.try_send(Ok(part(&snapshot.jpeg)));
                }
                Err(error) => {
                    tracing::debug!(error = %error, "MJPEG stream ended");
                    break;
                }
            }

            let now = tokio::time::Instant::now();
            deadline += period;
            if deadline <= now {
                deadline = now + period;
            }
        }

        control.shutdown().await;
    });
    (receiver, task)
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ozma_capture::{CaptureProvider, CaptureRegistry, FakeCaptureProvider};
    use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
    use ozma_protocol::{ApiError, CaptureCapability, CaptureKind};

    fn jpeg_body(part: &[u8]) -> &[u8] {
        let marker = b"\r\n\r\n";
        let start = part
            .windows(marker.len())
            .position(|window| window == marker)
            .expect("part has a header terminator")
            + marker.len();
        assert!(part.ends_with(b"\r\n"), "part ends with CRLF");
        &part[start..part.len() - 2]
    }

    fn jpeg_dimensions(jpeg: &[u8]) -> (u16, u16) {
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(jpeg));
        let pixels = decoder.decode().expect("valid JPEG");
        assert!(!pixels.is_empty());
        let info = decoder.info().expect("JPEG info");
        (info.width, info.height)
    }

    struct StaticFrameProvider {
        id: String,
        format: FrameFormat,
        payload: Vec<u8>,
    }

    impl CaptureProvider for StaticFrameProvider {
        fn kind(&self) -> CaptureKind {
            CaptureKind::Fake
        }

        fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
            Ok(vec![CaptureCapability {
                id: self.id.clone(),
                name: "Static test frame".to_owned(),
                kind: CaptureKind::Fake,
                formats: vec![self.format.pixel_format.as_str().to_owned()],
                max_width: self.format.width,
                max_height: self.format.height,
                max_fps: 1,
                compressed: self.format.pixel_format.is_compressed(),
                hdr: false,
                has_audio: false,
            }])
        }

        fn open(
            &self,
            _capability: &CaptureCapability,
        ) -> Result<Box<dyn CaptureSource>, ApiError> {
            Ok(Box::new(StaticFrameSource {
                id: self.id.clone(),
                format: self.format,
                payload: self.payload.clone(),
            }))
        }
    }

    struct StaticFrameSource {
        id: String,
        format: FrameFormat,
        payload: Vec<u8>,
    }

    #[async_trait]
    impl CaptureSource for StaticFrameSource {
        fn id(&self) -> &str {
            &self.id
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            Frame::with_data(self.format, 0, 0, true, self.payload.clone())
        }
    }

    #[tokio::test]
    async fn mjpeg_source_streams_the_original_jpeg_bytes() {
        let jpeg = ozma_encode::encode_jpeg_rgb(&[200, 30, 40, 10, 220, 90], 2, 1, 95).unwrap();
        let registry = CaptureRegistry::new().with(StaticFrameProvider {
            id: "mjpeg0".to_owned(),
            format: FrameFormat::new(PixelFormat::Mjpeg, 2, 1),
            payload: jpeg.clone(),
        });
        let control = SnapshotControl::new(registry).streaming(None);
        let first = control.capture().await.expect("MJPEG first frame");
        assert_eq!(first.jpeg, jpeg, "snapshot must pass MJPEG through");

        let streams = VideoStreams::new(1);
        let guard = streams.try_start().expect("stream slot");
        let (mut receiver, task) = spawn_stream(
            control,
            first,
            VideoOptions {
                codec: VideoCodec::Mjpeg,
                fps: 10,
                quality: 85,
                bitrate_kbps: DEFAULT_BITRATE_KBPS,
            },
            guard,
        );
        let part = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
            .await
            .expect("first part before timeout")
            .expect("part")
            .expect("infallible");
        assert_eq!(jpeg_body(&part), jpeg, "stream must pass MJPEG through");
        assert_eq!(jpeg_dimensions(jpeg_body(&part)), (2, 1));

        drop(receiver);
        tokio::time::timeout(Duration::from_secs(5), task)
            .await
            .expect("stream task stops after disconnect")
            .expect("stream task joins");
    }

    #[tokio::test]
    async fn yuyv_and_nv12_sources_stream_real_jpeg_parts() {
        for format in [
            FrameFormat::new(PixelFormat::Yuyv, 16, 8),
            FrameFormat::new(PixelFormat::Nv12, 16, 8),
        ] {
            let registry =
                CaptureRegistry::new().with(FakeCaptureProvider::new().with_format(format));
            let control = SnapshotControl::new(registry).streaming(None);
            let first = control
                .capture()
                .await
                .unwrap_or_else(|error| panic!("{format:?} first frame failed: {error}"));
            let streams = VideoStreams::new(1);
            let guard = streams.try_start().expect("stream slot");
            let (mut receiver, task) = spawn_stream(
                control,
                first,
                VideoOptions {
                    codec: VideoCodec::Mjpeg,
                    fps: 10,
                    quality: 85,
                    bitrate_kbps: DEFAULT_BITRATE_KBPS,
                },
                guard,
            );

            let part = tokio::time::timeout(Duration::from_secs(5), receiver.recv())
                .await
                .expect("first part before timeout")
                .expect("part")
                .expect("infallible");
            let body = jpeg_body(&part);
            assert!(body.starts_with(&[0xFF, 0xD8]), "{format:?}");
            assert_eq!(jpeg_dimensions(body), (16, 8), "{format:?}");

            drop(receiver);
            tokio::time::timeout(Duration::from_secs(5), task)
                .await
                .expect("stream task stops after disconnect")
                .expect("stream task joins");
            assert_eq!(streams.active(), 0);
        }
    }

    #[test]
    fn defaults_are_bounded() {
        let options = VideoParams::default().options().unwrap();
        assert_eq!(options.codec, DEFAULT_CODEC);
        assert_eq!(options.fps, DEFAULT_FPS);
        assert_eq!(options.quality, DEFAULT_QUALITY);
        assert_eq!(options.bitrate_kbps, DEFAULT_BITRATE_KBPS);
    }

    #[test]
    fn fps_and_quality_are_clamped_to_their_bounds() {
        let low = VideoParams {
            fps: Some("0".into()),
            quality: Some("0".into()),
            codec: None,
            bitrate: Some("0".into()),
            console: None,
        }
        .options()
        .unwrap();
        assert_eq!(low.fps, MIN_FPS);
        assert_eq!(low.quality, MIN_QUALITY);
        assert_eq!(low.bitrate_kbps, MIN_BITRATE_KBPS);

        let high = VideoParams {
            fps: Some("9999".into()),
            quality: Some("9999".into()),
            codec: None,
            bitrate: Some("999999".into()),
            console: None,
        }
        .options()
        .unwrap();
        assert_eq!(high.fps, MAX_FPS);
        assert_eq!(high.quality, MAX_QUALITY);
        assert_eq!(high.bitrate_kbps, MAX_BITRATE_KBPS);
    }

    #[test]
    fn non_numeric_values_are_typed_errors() {
        let error = VideoParams {
            fps: Some("fast".into()),
            quality: None,
            codec: None,
            bitrate: None,
            console: None,
        }
        .options()
        .unwrap_err();
        assert_eq!(error.field, "fps");
        assert!(error.message.contains("integer"));

        let error = VideoParams {
            fps: None,
            quality: Some("best".into()),
            codec: None,
            bitrate: None,
            console: None,
        }
        .options()
        .unwrap_err();
        assert_eq!(error.field, "quality");

        let error = VideoParams {
            fps: None,
            quality: None,
            codec: None,
            bitrate: Some("fast".into()),
            console: None,
        }
        .options()
        .unwrap_err();
        assert_eq!(error.field, "bitrate");

        let error = VideoParams {
            fps: None,
            quality: None,
            codec: Some("vp9".into()),
            bitrate: None,
            console: None,
        }
        .options()
        .unwrap_err();
        assert_eq!(error.field, "codec");
    }

    #[test]
    fn encoder_selection_keeps_the_hardware_backend_and_codec_family() {
        assert_eq!(
            preferred_encoder(HlsEncoder::VaapiHevc, HlsCodec::H264),
            HlsEncoder::VaapiH264
        );
        assert_eq!(
            preferred_encoder(HlsEncoder::NvencH264, HlsCodec::Hevc),
            HlsEncoder::NvencHevc
        );
        assert_eq!(
            preferred_encoder(HlsEncoder::Mjpeg, HlsCodec::Hevc),
            HlsEncoder::Hevc
        );
        assert!(source_stream_copy(
            VideoCodec::H264,
            FrameFormat::new(PixelFormat::H264, 320, 240)
        ));
        assert!(source_stream_copy(
            VideoCodec::Hevc,
            FrameFormat::new(PixelFormat::Hevc, 320, 240)
        ));
        assert!(!source_stream_copy(
            VideoCodec::H264,
            FrameFormat::new(PixelFormat::Hevc, 320, 240)
        ));
        assert!(!source_stream_copy(
            VideoCodec::H264,
            FrameFormat::rgb24(320, 240)
        ));
    }

    #[test]
    fn stream_slots_are_capped_and_released_on_drop() {
        let streams = VideoStreams::new(2);
        let first = streams.try_start().expect("first slot");
        let second = streams.try_start().expect("second slot");
        assert_eq!(streams.active(), 2);
        assert!(streams.try_start().is_none());
        drop(first);
        assert_eq!(streams.active(), 1);
        assert!(streams.try_start().is_some());
        drop(second);
    }

    #[test]
    fn multipart_part_has_a_length_and_jpeg_body() {
        let jpeg = [0xFF, 0xD8, 0xFF, 0xD9];
        let part = part(&jpeg);
        let text = String::from_utf8_lossy(&part);
        assert!(text.starts_with("--ozma-frame\r\n"));
        assert!(text.contains("Content-Type: image/jpeg\r\n"));
        assert!(text.contains("Content-Length: 4\r\n"));
        assert!(part.ends_with(b"\r\n"));
        assert!(part.windows(4).any(|window| window == jpeg));
    }
}
