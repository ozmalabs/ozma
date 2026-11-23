// SPDX-License-Identifier: AGPL-3.0-only
//! PipeWire video capture provider for Linux.
//!
//! The provider discovers the graph's `Video/Source` nodes and opens the chosen
//! one through [`ozma_pipewire::PipeWireVideoSource`].  The adapter normalises
//! the negotiated raw layout into the byte layout the rest of the pipeline
//! understands:
//!
//! | PipeWire | [`PixelFormat`] | Adaptation |
//! |---|---|---|
//! | `BGRA` | `Bgra` | unchanged |
//! | `BGRx` | `Bgra` | fourth byte forced to opaque |
//! | `RGBx` | `Rgb24` | padding dropped |
//! | `I420` | `Nv12` | U/V planes interleaved |
//! | `NV12` | `Nv12` | unchanged |
//! | `YUYV` | `Yuyv` | unchanged |
//!
//! Discovery uses a bounded PipeWire sync round-trip.  A machine with no
//! PipeWire session yields an empty list rather than making the whole capture
//! registry fail; use [`ozma_pipewire::probe_video_status`] when a diagnostic
//! reason is needed.
//!
//! ## Screen-cast portal adapter point
//!
//! The desktop portal is a permission/selection front-end, not a media path:
//! after `org.freedesktop.portal.ScreenCast` returns a stream, the selected
//! monitor/window is a normal PipeWire `Video/Source` node.  A portal adapter
//! therefore belongs *above* this provider: perform the portal handshake, take
//! the returned node id/serial/name, and open it with
//! `pipewire:node:<id>` or `pipewire:name:<name>`.  This module never fakes a
//! portal success: with no real node, enumeration is empty and opening fails
//! with a typed error.
//!
//! This module contains no `unsafe` code.

use std::{sync::Arc, time::Duration};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_pipewire::{
    probe_video_sources_blocking, PipeWireError, PipeWireVideoSource, StreamTarget,
    VideoCaptureConfig, VideoFrame, VideoNode, VideoPixelFormat, VideoProbeConfig,
};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use tracing::debug;

use crate::provider::CaptureProvider;

/// Defaults for every source opened by [`PipeWireCaptureProvider`].
#[derive(Debug, Clone)]
pub struct PipeWireOptions {
    /// Optional configured source.
    ///
    /// `Default` advertises every discovered node.  A named object or node id
    /// filters enumeration to that source and is also used when opening the
    /// synthetic `pipewire:default` capability id.
    pub target: StreamTarget,
    /// Optional PipeWire remote name override.
    pub remote: Option<String>,
    /// Requested capture width.
    pub width: u32,
    /// Requested capture height.
    pub height: u32,
    /// Requested capture frame rate.
    pub fps: u32,
    /// Frames buffered between the realtime callback and the async consumer.
    pub queue_capacity: usize,
    /// Format negotiation timeout.
    pub startup_timeout: Duration,
    /// Stream shutdown timeout.
    pub stop_timeout: Duration,
    /// Discovery timeout.
    pub probe_timeout: Duration,
}

impl Default for PipeWireOptions {
    fn default() -> Self {
        Self {
            target: StreamTarget::Default,
            remote: None,
            width: 1920,
            height: 1080,
            fps: 30,
            queue_capacity: 4,
            startup_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(2),
            probe_timeout: Duration::from_secs(2),
        }
    }
}

type ProbeFn = Arc<dyn Fn(&PipeWireOptions) -> Result<Vec<VideoNode>, ApiError> + Send + Sync>;
type OpenFn = Arc<
    dyn Fn(CaptureCapability, PipeWireOptions) -> Result<Box<dyn CaptureSource>, ApiError>
        + Send
        + Sync,
>;

/// A capture provider that enumerates and opens PipeWire video sources.
///
/// The normal constructor uses the live graph.  [`with_hooks`](Self::with_hooks)
/// injects discovery and opening for tests, so registry behaviour can be
/// exercised without a PipeWire daemon.
pub struct PipeWireCaptureProvider {
    options: PipeWireOptions,
    probe: ProbeFn,
    open_source: OpenFn,
}

impl PipeWireCaptureProvider {
    /// Provider with the host defaults and a live PipeWire probe.
    pub fn new() -> Self {
        Self::with_options(PipeWireOptions::default())
    }

    /// Provider with caller-supplied defaults and a live PipeWire probe.
    pub fn with_options(options: PipeWireOptions) -> Self {
        Self {
            options,
            probe: Arc::new(default_probe),
            open_source: Arc::new(default_open),
        }
    }

    /// Provider with an injected discovery function but live opening.
    ///
    /// Useful for capability tests and for callers that cache a graph scan.
    pub fn with_probe<F>(options: PipeWireOptions, probe: F) -> Self
    where
        F: Fn(&PipeWireOptions) -> Result<Vec<VideoNode>, ApiError> + Send + Sync + 'static,
    {
        Self {
            options,
            probe: Arc::new(probe),
            open_source: Arc::new(default_open),
        }
    }

    /// Provider with injected discovery and opening.
    ///
    /// The open hook receives the requested capability and the provider
    /// options; it can return any [`CaptureSource`], which keeps registry
    /// dispatch testable without touching a device.
    pub fn with_hooks<F, G>(options: PipeWireOptions, probe: F, open: G) -> Self
    where
        F: Fn(&PipeWireOptions) -> Result<Vec<VideoNode>, ApiError> + Send + Sync + 'static,
        G: Fn(CaptureCapability, PipeWireOptions) -> Result<Box<dyn CaptureSource>, ApiError>
            + Send
            + Sync
            + 'static,
    {
        Self {
            options,
            probe: Arc::new(probe),
            open_source: Arc::new(open),
        }
    }

    /// The provider's defaults.
    pub fn options(&self) -> &PipeWireOptions {
        &self.options
    }
}

impl Default for PipeWireCaptureProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl CaptureProvider for PipeWireCaptureProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::PipeWire
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        let discovered = (self.probe)(&self.options)?;
        let explicit = !matches!(self.options.target, StreamTarget::Default);
        let matched: Vec<VideoNode> = discovered
            .iter()
            .filter(|node| target_matches(node, &self.options.target))
            .cloned()
            .collect();
        if explicit && !discovered.is_empty() && matched.is_empty() {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!(
                    "Configured PipeWire source `{}` is not in the graph.",
                    self.options.target.describe()
                ),
            )
            .with_details(self.options.target.describe())
            .with_remediation(
                "Start the configured camera or screen-cast session, then refresh discovery.",
            ));
        }
        Ok(matched
            .iter()
            .map(|node| capability_for(node, &self.options))
            .collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::PipeWire {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                "The PipeWire provider cannot open a non-PipeWire capability.",
            )
            .with_details(capability.id.clone()));
        }
        (self.open_source)(capability.clone(), self.options.clone())
    }
}

/// Build the advertised capability for a discovered node.
pub fn capability_for(node: &VideoNode, options: &PipeWireOptions) -> CaptureCapability {
    CaptureCapability {
        id: capability_id(node),
        name: if node.description.is_empty() {
            node.name.clone()
        } else {
            node.description.clone()
        },
        kind: CaptureKind::PipeWire,
        formats: VideoPixelFormat::SUPPORTED
            .iter()
            .map(|format| format.label().to_owned())
            .collect(),
        max_width: options.width,
        max_height: options.height,
        max_fps: options.fps,
        compressed: false,
        hdr: false,
        has_audio: false,
    }
}

/// Stable capability id for a discovered node.
///
/// Node names survive daemon restarts; numeric ids do not.  The id still
/// accepts `pipewire:node:<id>` so a caller can target a transient node.
pub fn capability_id(node: &VideoNode) -> String {
    if node.name.is_empty() {
        format!("pipewire:node:{}", node.id)
    } else {
        format!("pipewire:name:{}", node.name)
    }
}

/// Whether a discovered node satisfies an explicitly configured target.
pub fn target_matches(node: &VideoNode, target: &StreamTarget) -> bool {
    match target {
        StreamTarget::Default => true,
        StreamTarget::Object(name) => node.name == *name,
        StreamTarget::NodeId(id) => node.id == *id,
    }
}

/// Resolve a capability id into a PipeWire stream target.
pub fn target_for_capability(
    id: &str,
    options: &PipeWireOptions,
) -> Result<StreamTarget, ApiError> {
    if id == "pipewire:default" {
        return Ok(options.target.clone());
    }
    if let Some(rest) = id.strip_prefix("pipewire:node:") {
        let node_id = rest.parse::<u32>().map_err(|_| {
            ApiError::new(
                codes::INVALID_REQUEST,
                format!("PipeWire capability `{id}` has a non-numeric node id."),
            )
            .with_details(id.to_owned())
            .with_remediation("Use `pipewire:node:<id>` with the numeric id from discovery.")
        })?;
        return Ok(StreamTarget::NodeId(node_id));
    }
    if let Some(rest) = id.strip_prefix("pipewire:name:") {
        if rest.is_empty() {
            return Err(ApiError::new(
                codes::INVALID_REQUEST,
                "PipeWire capability has an empty node name.",
            )
            .with_details(id.to_owned()));
        }
        return Ok(StreamTarget::Object(rest.to_owned()));
    }
    if let Some(rest) = id.strip_prefix("pipewire:") {
        if rest.is_empty() {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("PipeWire capability `{id}` is incomplete."),
            )
            .with_details(id.to_owned()));
        }
        if let Ok(node_id) = rest.parse::<u32>() {
            return Ok(StreamTarget::NodeId(node_id));
        }
        return Ok(StreamTarget::Object(rest.to_owned()));
    }
    Err(ApiError::new(
        codes::NOT_FOUND,
        format!("`{id}` is not a PipeWire capture source id."),
    )
    .with_details(id.to_owned())
    .with_remediation("Refresh capture discovery and retry with a current source id."))
}

fn default_probe(options: &PipeWireOptions) -> Result<Vec<VideoNode>, ApiError> {
    let config = VideoProbeConfig {
        remote: options.remote.clone(),
        timeout: options.probe_timeout,
        stop_timeout: options.stop_timeout,
    };
    match probe_video_sources_blocking(config) {
        Ok(nodes) => Ok(nodes),
        Err(error) => match error {
            // A host without a PipeWire session simply has no sources of this
            // kind. Diagnostics can ask `probe_video_status` for the reason.
            PipeWireError::Unavailable { .. } | PipeWireError::ProbeTimeout { .. } => {
                debug!(%error, "PipeWire video discovery unavailable");
                Ok(Vec::new())
            }
            other => Err(map_pipewire_error(other)),
        },
    }
}

fn default_open(
    capability: CaptureCapability,
    options: PipeWireOptions,
) -> Result<Box<dyn CaptureSource>, ApiError> {
    let target = target_for_capability(&capability.id, &options)?;
    let config = VideoCaptureConfig {
        target,
        stream_name: stream_name_for(&capability.id),
        remote: options.remote.clone(),
        width: options.width,
        height: options.height,
        fps: options.fps,
        queue_capacity: options.queue_capacity,
        startup_timeout: options.startup_timeout,
        stop_timeout: options.stop_timeout,
    };
    let source = PipeWireVideoSource::open_blocking(config).map_err(map_pipewire_error)?;
    let adapter = PipeWireCaptureSource::new(capability.id, source)?;
    Ok(Box::new(adapter))
}

fn stream_name_for(id: &str) -> String {
    let suffix: String = id
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' || ch == '.' {
                ch
            } else {
                '-'
            }
        })
        .collect();
    let suffix = suffix.trim_matches('-');
    if suffix.is_empty() {
        "ozma-capture".to_owned()
    } else {
        format!(
            "ozma-capture-{}",
            &suffix[suffix.len().saturating_sub(48)..]
        )
    }
}

/// Convert a pipewire-level failure into the node's typed error shape.
pub fn map_pipewire_error(error: PipeWireError) -> ApiError {
    let unsupported = matches!(
        error,
        PipeWireError::FormatMismatch { .. }
            | PipeWireError::VideoFormatUnsupported { .. }
            | PipeWireError::CompressedVideoUnsupported { .. }
            | PipeWireError::NoVideoSource { .. }
    );
    let retryable = matches!(
        error,
        PipeWireError::Unavailable { .. }
            | PipeWireError::StartTimeout { .. }
            | PipeWireError::ProbeTimeout { .. }
            | PipeWireError::Disconnected { .. }
            | PipeWireError::Stream { .. }
            | PipeWireError::Thread { .. }
            | PipeWireError::QueueFull { .. }
            | PipeWireError::Poisoned
    );
    let code = if unsupported {
        codes::UNSUPPORTED
    } else {
        codes::CAPTURE_UNAVAILABLE
    };
    let api = ApiError::new(code, error.to_string())
        .with_details(error.code().to_owned())
        .with_remediation(error.remediation());
    if retryable {
        api.retryable()
    } else {
        api
    }
}

/// A [`CaptureSource`] backed by a native PipeWire video stream.
///
/// The source owns the PipeWire stream thread.  Dropping it (or calling
/// [`stop`](CaptureSource::stop)) quits and joins that thread.
pub struct PipeWireCaptureSource {
    id: String,
    video_format: VideoPixelFormat,
    format: FrameFormat,
    inner: Option<PipeWireVideoSource>,
    stopped: bool,
}

impl PipeWireCaptureSource {
    /// Adapt an open PipeWire video source.
    pub fn new(id: impl Into<String>, inner: PipeWireVideoSource) -> Result<Self, ApiError> {
        let id = id.into();
        let negotiated = inner.negotiated_format();
        let pixel_format =
            media_pixel_format(negotiated.format, negotiated.width, negotiated.height).map_err(
                |error| {
                    ApiError::new(codes::UNSUPPORTED, error.to_string())
                        .with_details(id.clone())
                        .with_remediation(
                            "Choose a source that offers BGRA, BGRx, RGBx, I420, NV12, or YUYV \
                         with even dimensions for planar layouts.",
                        )
                },
            )?;
        let format = FrameFormat::new(pixel_format, negotiated.width, negotiated.height);
        Ok(Self {
            id,
            video_format: negotiated.format,
            format,
            inner: Some(inner),
            stopped: false,
        })
    }

    /// The raw layout negotiated with PipeWire.
    pub fn video_format(&self) -> VideoPixelFormat {
        self.video_format
    }

    /// The underlying source's counters.
    pub fn stats(&self) -> Option<ozma_pipewire::VideoCaptureStats> {
        self.inner.as_ref().map(PipeWireVideoSource::stats)
    }
}

impl std::fmt::Debug for PipeWireCaptureSource {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PipeWireCaptureSource")
            .field("id", &self.id)
            .field("video_format", &self.video_format)
            .field("format", &self.format)
            .field("stopped", &self.stopped)
            .finish()
    }
}

#[async_trait]
impl CaptureSource for PipeWireCaptureSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        if self.stopped {
            return Err(MediaError::Closed {
                id: self.id.clone(),
            });
        }
        let Some(inner) = self.inner.as_mut() else {
            return Err(MediaError::Closed {
                id: self.id.clone(),
            });
        };
        let raw = inner
            .next_frame()
            .await
            .map_err(|error| MediaError::capture(&self.id, error.to_string()))?;

        if raw.width != self.format.width
            || raw.height != self.format.height
            || raw.format != self.video_format
        {
            return Err(MediaError::FormatMismatch {
                expected: self.format.to_string(),
                actual: format!("{} {}x{}", raw.format.label(), raw.width, raw.height),
            });
        }

        into_media_frame(&self.id, raw, self.format)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;
        let Some(mut inner) = self.inner.take() else {
            return Ok(());
        };
        match tokio::task::spawn_blocking(move || inner.stop()).await {
            Ok(()) => Ok(()),
            Err(error) => Err(MediaError::capture(
                &self.id,
                format!("could not join the PipeWire video thread: {error}"),
            )),
        }
    }
}

fn media_pixel_format(
    format: VideoPixelFormat,
    width: u32,
    height: u32,
) -> Result<PixelFormat, MediaError> {
    if width == 0 || height == 0 {
        return Err(MediaError::InvalidFormat(format!(
            "PipeWire negotiated an empty {width}x{height} frame"
        )));
    }
    let pixel_format = match format {
        VideoPixelFormat::Bgra | VideoPixelFormat::Bgrx => PixelFormat::Bgra,
        VideoPixelFormat::Rgbx => PixelFormat::Rgb24,
        VideoPixelFormat::I420 | VideoPixelFormat::Nv12 => PixelFormat::Nv12,
        VideoPixelFormat::Yuyv => PixelFormat::Yuyv,
    };
    if matches!(format, VideoPixelFormat::I420 | VideoPixelFormat::Nv12)
        && (width % 2 != 0 || height % 2 != 0)
    {
        return Err(MediaError::InvalidFormat(format!(
            "planar YUV needs even dimensions, got {width}x{height}"
        )));
    }
    Ok(pixel_format)
}

fn into_media_frame(id: &str, raw: VideoFrame, format: FrameFormat) -> Result<Frame, MediaError> {
    let expected = raw
        .format
        .packed_len(raw.width, raw.height)
        .ok_or_else(|| MediaError::capture(id, "PipeWire reported an invalid frame geometry"))?;
    if raw.data.len() < expected {
        return Err(MediaError::capture(
            id,
            format!(
                "PipeWire delivered {} bytes for a {}-byte {} frame",
                raw.data.len(),
                expected,
                raw.format.label()
            ),
        ));
    }

    let mut raw_data = raw.data;
    raw_data.truncate(expected);
    let data = match raw.format {
        VideoPixelFormat::Bgra => raw_data,
        VideoPixelFormat::Bgrx => {
            for pixel in raw_data.chunks_exact_mut(4) {
                pixel[3] = 0xFF;
            }
            raw_data
        }
        VideoPixelFormat::Rgbx => repack_rgbx(&raw_data, raw.width, raw.height),
        VideoPixelFormat::I420 => i420_to_nv12(&raw_data, raw.width, raw.height),
        VideoPixelFormat::Nv12 => raw_data,
        VideoPixelFormat::Yuyv => raw_data,
    };

    Frame::with_data(format, raw.pts_micros, raw.sequence, false, data)
}

fn repack_rgbx(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let pixels = (width as usize).saturating_mul(height as usize);
    let mut out = Vec::with_capacity(pixels.saturating_mul(3));
    for pixel in data.chunks_exact(4).take(pixels) {
        out.extend_from_slice(&pixel[..3]);
    }
    out
}

fn i420_to_nv12(data: &[u8], width: u32, height: u32) -> Vec<u8> {
    let width = width as usize;
    let height = height as usize;
    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let y_len = width * height;
    let chroma_len = chroma_width * chroma_height;
    let u_start = y_len;
    let v_start = y_len + chroma_len;

    let mut out = Vec::with_capacity(y_len + chroma_len * 2);
    out.extend_from_slice(&data[..y_len]);
    for row in 0..chroma_height {
        let u_row = u_start + row * chroma_width;
        let v_row = v_start + row * chroma_width;
        for column in 0..chroma_width {
            out.push(data[u_row + column]);
            out.push(data[v_row + column]);
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_media::FakeCapture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    fn node() -> VideoNode {
        VideoNode {
            id: 42,
            name: "v4l2_input.test-camera".to_owned(),
            description: "Test camera".to_owned(),
            media_class: "Video/Source".to_owned(),
            path: Some("v4l2:/dev/video42".to_owned()),
        }
    }

    #[test]
    fn capability_ids_prefer_stable_names() {
        assert_eq!(
            capability_id(&node()),
            "pipewire:name:v4l2_input.test-camera"
        );
        let unnamed = VideoNode {
            name: String::new(),
            ..node()
        };
        assert_eq!(capability_id(&unnamed), "pipewire:node:42");
    }

    #[test]
    fn target_parsing_accepts_every_supported_form() {
        let options = PipeWireOptions {
            target: StreamTarget::Default,
            ..PipeWireOptions::default()
        };
        assert_eq!(
            target_for_capability("pipewire:name:cam", &options).unwrap(),
            StreamTarget::Object("cam".to_owned())
        );
        assert_eq!(
            target_for_capability("pipewire:node:7", &options).unwrap(),
            StreamTarget::NodeId(7)
        );
        assert_eq!(
            target_for_capability("pipewire:default", &options).unwrap(),
            StreamTarget::Default
        );
        assert_eq!(
            target_for_capability("pipewire:custom", &options).unwrap(),
            StreamTarget::Object("custom".to_owned())
        );
        assert_eq!(
            target_for_capability("not-pipewire", &options)
                .unwrap_err()
                .code,
            codes::NOT_FOUND
        );
        assert_eq!(
            target_for_capability("pipewire:node:abc", &options)
                .unwrap_err()
                .code,
            codes::INVALID_REQUEST
        );
    }

    #[test]
    fn error_mapping_uses_typed_codes() {
        let unsupported = map_pipewire_error(PipeWireError::CompressedVideoUnsupported {
            format: "video/encoded".to_owned(),
        });
        assert_eq!(unsupported.code, codes::UNSUPPORTED);
        assert!(!unsupported.retryable);

        let unavailable = map_pipewire_error(PipeWireError::Unavailable {
            detail: "no socket".to_owned(),
        });
        assert_eq!(unavailable.code, codes::CAPTURE_UNAVAILABLE);
        assert!(unavailable.retryable);
        assert!(unavailable.remediation.is_some());
        assert_eq!(unavailable.details, "pipewire.unavailable");
    }

    #[test]
    fn layouts_map_to_pipeline_pixel_formats() {
        assert_eq!(
            media_pixel_format(VideoPixelFormat::Bgra, 320, 240).unwrap(),
            PixelFormat::Bgra
        );
        assert_eq!(
            media_pixel_format(VideoPixelFormat::Bgrx, 320, 240).unwrap(),
            PixelFormat::Bgra
        );
        assert_eq!(
            media_pixel_format(VideoPixelFormat::Rgbx, 320, 240).unwrap(),
            PixelFormat::Rgb24
        );
        assert_eq!(
            media_pixel_format(VideoPixelFormat::I420, 320, 240).unwrap(),
            PixelFormat::Nv12
        );
        assert_eq!(
            media_pixel_format(VideoPixelFormat::Nv12, 320, 240).unwrap(),
            PixelFormat::Nv12
        );
        assert_eq!(
            media_pixel_format(VideoPixelFormat::Yuyv, 320, 240).unwrap(),
            PixelFormat::Yuyv
        );
        assert!(media_pixel_format(VideoPixelFormat::Nv12, 321, 240).is_err());
        assert!(media_pixel_format(VideoPixelFormat::I420, 320, 241).is_err());
        assert!(media_pixel_format(VideoPixelFormat::Bgra, 0, 240).is_err());
    }

    #[test]
    fn rgbx_is_repacked_and_bgrx_becomes_opaque() {
        let format = FrameFormat::new(PixelFormat::Rgb24, 2, 1);
        let frame = into_media_frame(
            "test",
            VideoFrame {
                pts_micros: 1,
                sequence: 2,
                width: 2,
                height: 1,
                stride: 8,
                format: VideoPixelFormat::Rgbx,
                data: vec![1, 2, 3, 99, 4, 5, 6, 99],
            },
            format,
        )
        .unwrap();
        assert_eq!(frame.data(), &[1, 2, 3, 4, 5, 6]);

        let format = FrameFormat::new(PixelFormat::Bgra, 1, 1);
        let frame = into_media_frame(
            "test",
            VideoFrame {
                pts_micros: 1,
                sequence: 2,
                width: 1,
                height: 1,
                stride: 4,
                format: VideoPixelFormat::Bgrx,
                data: vec![9, 8, 7, 0],
            },
            format,
        )
        .unwrap();
        assert_eq!(frame.data(), &[9, 8, 7, 0xFF]);
    }

    #[test]
    fn i420_is_interleaved_into_nv12() {
        let format = FrameFormat::new(PixelFormat::Nv12, 2, 2);
        let frame = into_media_frame(
            "test",
            VideoFrame {
                pts_micros: 1,
                sequence: 0,
                width: 2,
                height: 2,
                stride: 2,
                format: VideoPixelFormat::I420,
                data: vec![1, 2, 3, 4, 5, 6],
            },
            format,
        )
        .unwrap();
        assert_eq!(frame.data(), &[1, 2, 3, 4, 5, 6]);
        assert_eq!(
            frame.data().len(),
            PixelFormat::Nv12.nominal_len(2, 2).unwrap()
        );
    }

    #[test]
    fn short_or_oversized_payloads_are_handled_explicitly() {
        let format = FrameFormat::new(PixelFormat::Bgra, 2, 1);
        let error = into_media_frame(
            "test",
            VideoFrame {
                pts_micros: 1,
                sequence: 0,
                width: 2,
                height: 1,
                stride: 8,
                format: VideoPixelFormat::Bgra,
                data: vec![0; 7],
            },
            format,
        )
        .unwrap_err();
        assert!(matches!(error, MediaError::Capture { .. }));

        let frame = into_media_frame(
            "test",
            VideoFrame {
                pts_micros: 1,
                sequence: 0,
                width: 2,
                height: 1,
                stride: 8,
                format: VideoPixelFormat::Bgra,
                data: vec![1; 12],
            },
            format,
        )
        .unwrap();
        assert_eq!(frame.data().len(), 8, "padding is not exposed downstream");
    }

    #[test]
    fn enumeration_maps_nodes_to_capabilities() {
        let provider =
            PipeWireCaptureProvider::with_probe(PipeWireOptions::default(), |_| Ok(vec![node()]));
        assert_eq!(provider.kind(), CaptureKind::PipeWire);
        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].id, "pipewire:name:v4l2_input.test-camera");
        assert_eq!(capabilities[0].kind, CaptureKind::PipeWire);
        assert_eq!(capabilities[0].name, "Test camera");
        assert!(capabilities[0].formats.contains(&"bgra".to_owned()));
        assert!(capabilities[0].formats.contains(&"nv12".to_owned()));
        assert!(!capabilities[0].compressed);
    }

    fn probe_two(_options: &PipeWireOptions) -> Result<Vec<VideoNode>, ApiError> {
        Ok(vec![
            node(),
            VideoNode {
                id: 43,
                name: "other-camera".to_owned(),
                description: "Other".to_owned(),
                media_class: "Video/Source".to_owned(),
                path: None,
            },
        ])
    }

    #[test]
    fn provider_rejects_a_foreign_capability() {
        let provider =
            PipeWireCaptureProvider::with_probe(PipeWireOptions::default(), |_| Ok(Vec::new()));
        let capability = CaptureCapability {
            id: "/dev/video0".to_owned(),
            name: "v4l2".to_owned(),
            kind: CaptureKind::V4l2,
            formats: Vec::new(),
            max_width: 0,
            max_height: 0,
            max_fps: 0,
            compressed: false,
            hdr: false,
            has_audio: false,
        };
        let error = provider.open(&capability).err().unwrap();
        assert_eq!(error.code, codes::NOT_FOUND);
    }

    #[test]
    fn configured_target_filters_discovery() {
        let all = PipeWireCaptureProvider::with_probe(PipeWireOptions::default(), probe_two);
        assert_eq!(all.enumerate().unwrap().len(), 2);

        let by_name = PipeWireCaptureProvider::with_probe(
            PipeWireOptions {
                target: StreamTarget::Object("v4l2_input.test-camera".to_owned()),
                ..PipeWireOptions::default()
            },
            probe_two,
        );
        let capabilities = by_name.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].id, "pipewire:name:v4l2_input.test-camera");

        let by_id = PipeWireCaptureProvider::with_probe(
            PipeWireOptions {
                target: StreamTarget::NodeId(43),
                ..PipeWireOptions::default()
            },
            probe_two,
        );
        let capabilities = by_id.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert_eq!(capabilities[0].id, "pipewire:name:other-camera");

        let missing = PipeWireCaptureProvider::with_probe(
            PipeWireOptions {
                target: StreamTarget::Object("gone".to_owned()),
                ..PipeWireOptions::default()
            },
            probe_two,
        );
        assert_eq!(missing.enumerate().unwrap_err().code, codes::NOT_FOUND);
    }

    #[tokio::test]
    async fn registry_opens_an_injected_pipewire_source() {
        let calls = Arc::new(AtomicUsize::new(0));
        let opens = Arc::clone(&calls);
        let provider = PipeWireCaptureProvider::with_hooks(
            PipeWireOptions::default(),
            |_| Ok(vec![node()]),
            move |capability, _options| {
                opens.fetch_add(1, Ordering::Relaxed);
                let source = FakeCapture::new(FrameFormat::rgb24(64, 48))
                    .with_id(capability.id)
                    .with_fps(1_000);
                Ok(Box::new(source))
            },
        );

        let registry = crate::CaptureRegistry::new().with(provider);
        let capabilities = registry.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        let mut source = registry.open(&capabilities[0]).unwrap();
        assert_eq!(source.id(), "pipewire:name:v4l2_input.test-camera");
        let frame = source.next_frame().await.unwrap();
        assert_eq!(frame.sequence(), 0);
        assert!(!frame.is_empty());
        assert_eq!(calls.load(Ordering::Relaxed), 1);
        source.stop().await.unwrap();
    }

    #[test]
    fn default_probe_reports_no_daemon_as_no_sources() {
        let options = PipeWireOptions {
            remote: Some(format!(
                "ozma-test-no-such-video-remote-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            )),
            probe_timeout: Duration::from_millis(800),
            stop_timeout: Duration::from_millis(800),
            ..PipeWireOptions::default()
        };
        let provider = PipeWireCaptureProvider::with_options(options);
        // Missing daemon is "no sources", not a registry-wide failure.
        assert!(provider.enumerate().unwrap().is_empty());
    }

    fn forced() -> bool {
        matches!(
            std::env::var("OZMA_TEST_PIPEWIRE_VIDEO").as_deref(),
            Ok("1") | Ok("true") | Ok("yes")
        )
    }

    fn report_or_skip(what: &str, error: impl std::fmt::Display) -> bool {
        if forced() {
            panic!("{what} failed while OZMA_TEST_PIPEWIRE_VIDEO is set: {error}");
        }
        eprintln!("skipping {what}: {error}");
        false
    }

    /// Opt-in real-device test.
    ///
    /// Run with:
    ///
    /// ```text
    /// OZMA_TEST_PIPEWIRE_VIDEO=1 cargo test -p ozma-capture -- --nocapture real_pipewire
    /// ```
    ///
    /// Without the variable the test still tries the host graph and skips
    /// cleanly when it has no `Video/Source` node or the source is busy.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn real_pipewire_provider_delivers_frames() {
        let provider = PipeWireCaptureProvider::new();
        let capabilities = match provider.enumerate() {
            Ok(capabilities) => capabilities,
            Err(error) => {
                report_or_skip("PipeWire provider discovery", error);
                return;
            }
        };
        let Some(capability) = capabilities.into_iter().next() else {
            eprintln!("skipping real PipeWire provider test: no Video/Source node");
            return;
        };
        eprintln!("opening {} ({})", capability.id, capability.name);

        let mut source = match provider.open(&capability) {
            Ok(source) => source,
            Err(error) => {
                report_or_skip("PipeWire provider open", error);
                return;
            }
        };
        let format = source.format();
        assert!(format.width > 0 && format.height > 0, "{format}");

        let first = match tokio::time::timeout(Duration::from_secs(8), source.next_frame()).await {
            Ok(Ok(frame)) => frame,
            Ok(Err(error)) => {
                report_or_skip("PipeWire provider next_frame", error);
                return;
            }
            Err(_) => {
                report_or_skip(
                    "PipeWire provider next_frame",
                    "source produced no frame within 8s",
                );
                return;
            }
        };
        assert_eq!(first.sequence(), 0);
        assert!(!first.is_empty());
        assert_eq!(first.format(), format);
        eprintln!(
            "captured {} seq={} pts={}us bytes={}",
            format,
            first.sequence(),
            first.pts_micros(),
            first.len()
        );

        let second = tokio::time::timeout(Duration::from_secs(8), source.next_frame())
            .await
            .expect("second frame is bounded")
            .expect("second frame");
        assert_eq!(second.sequence(), 1);
        assert!(second.pts_micros() >= first.pts_micros());
        assert_eq!(second.format(), format);

        source.stop().await.expect("stop");
        source.stop().await.expect("stop is idempotent");
    }
}
