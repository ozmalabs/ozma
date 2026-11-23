//! Native PipeWire video capture.
//!
//! A [`PipeWireVideoSource`] opens a `Stream` in `Direction::Input` with
//! `MEDIA_TYPE=Video`, `MEDIA_CATEGORY=Capture`, and the same
//! `AUTOCONNECT | MAP_BUFFERS | RT_PROCESS` flags as the audio path.  The
//! PipeWire main loop lives on its own OS thread; the realtime `process`
//! callback packs one buffer into an owned [`VideoFrame`] and offers it to a
//! bounded async channel with `try_send`, dropping (and counting) frames when
//! the consumer is behind.
//!
//! Negotiation prefers raw layouts the rest of the pipeline can carry:
//! `BGRA`, `BGRx`, `RGBx`, `I420`, then `NV12`, with packed `YUYV` as a final
//! compatibility fallback for UVC cameras.  Compressed camera formats
//! (MJPEG/H.264 inside PipeWire) are rejected with a typed error rather than
//! being mislabelled as raw pixels.
//!
//! This module is Linux-only and contains no `unsafe` code.

use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    rc::Rc,
    sync::{
        atomic::{AtomicU32, AtomicU64, Ordering},
        mpsc as std_mpsc, Arc, Mutex,
    },
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use pipewire as pw;
use pw::{
    main_loop::MainLoopRc,
    properties::properties,
    spa::{
        self,
        param::{
            format::{FormatProperties, MediaSubtype, MediaType},
            video::{VideoFormat, VideoInfoRaw},
            ParamType,
        },
        pod::{self, serialize::PodSerializer, ChoiceValue, Object, Pod, Property, Value},
        utils::{Choice, ChoiceEnum, ChoiceFlags, Direction, Fraction, Rectangle, SpaTypes},
    },
    stream::{StreamBox, StreamFlags, StreamState},
    types::ObjectType,
};
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::error::PipeWireError;
use crate::pw_common::{SharedError, StreamTarget};
use crate::status::StatusError;

/// Raw video layouts this crate can actually carry.
///
/// The order of [`VideoPixelFormat::SUPPORTED`] is the negotiation preference:
/// BGRA and BGRx first (native compositor layouts), then RGBx, then planar
/// YUV420, with packed YUYV last for UVC compatibility.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VideoPixelFormat {
    /// Packed 32-bit BGRA, four bytes per pixel.
    Bgra,
    /// Packed 32-bit BGRx, four bytes per pixel; the fourth byte is padding.
    Bgrx,
    /// Packed 32-bit RGBx, four bytes per pixel; the fourth byte is padding.
    Rgbx,
    /// Planar YUV 4:2:0 with separate U and V planes.
    I420,
    /// Semi-planar YUV 4:2:0 (Y plane then interleaved UV).
    Nv12,
    /// Packed YUV 4:2:2 (`YUYV`/`YUY2`), the common UVC fallback.
    Yuyv,
}

/// Rows and row size for one plane of a raw video layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct PlaneLayout {
    /// Number of rows in this plane.
    pub rows: usize,
    /// Bytes per row, including any padding.
    pub row_bytes: usize,
}

impl VideoPixelFormat {
    /// Negotiation order, most preferred first.
    ///
    /// The first five layouts are the pipeline contract.  `Yuyv` trails them
    /// as a raw compatibility fallback for UVC cameras that offer nothing else.
    pub const SUPPORTED: [Self; 6] = [
        Self::Bgra,
        Self::Bgrx,
        Self::Rgbx,
        Self::I420,
        Self::Nv12,
        Self::Yuyv,
    ];

    /// Stable lowercase label used in capability strings and diagnostics.
    pub fn label(self) -> &'static str {
        match self {
            Self::Bgra => "bgra",
            Self::Bgrx => "bgrx",
            Self::Rgbx => "rgbx",
            Self::I420 => "i420",
            Self::Nv12 => "nv12",
            Self::Yuyv => "yuyv",
        }
    }

    /// True for the packed four-byte layouts.
    pub fn is_packed_rgb(self) -> bool {
        matches!(self, Self::Bgra | Self::Bgrx | Self::Rgbx)
    }

    /// True for 4:2:0 planar/semi-planar layouts.
    pub fn is_planar(self) -> bool {
        matches!(self, Self::I420 | Self::Nv12)
    }

    /// Bytes in one row of the first (luma/RGB) plane.
    pub fn row_bytes(self, width: u32) -> Option<usize> {
        let width = usize::try_from(width).ok()?;
        if width == 0 {
            return None;
        }
        match self {
            Self::Bgra | Self::Bgrx | Self::Rgbx => width.checked_mul(4),
            Self::I420 | Self::Nv12 => Some(width),
            Self::Yuyv => width.checked_mul(2),
        }
    }

    /// Plane geometry for a tightly packed frame of this layout.
    ///
    /// 4:2:0 chroma dimensions round up for odd widths/heights, matching SPA's
    /// frame-size convention.  Returns `None` for zero or overflowing sizes.
    pub fn plane_layout(self, width: u32, height: u32) -> Option<Vec<PlaneLayout>> {
        let width = usize::try_from(width).ok()?;
        let height = usize::try_from(height).ok()?;
        if width == 0 || height == 0 {
            return None;
        }
        match self {
            Self::Bgra | Self::Bgrx | Self::Rgbx => Some(vec![PlaneLayout {
                rows: height,
                row_bytes: width.checked_mul(4)?,
            }]),
            Self::Yuyv => Some(vec![PlaneLayout {
                rows: height,
                row_bytes: width.checked_mul(2)?,
            }]),
            Self::I420 => {
                let chroma_width = width.div_ceil(2);
                let chroma_height = height.div_ceil(2);
                Some(vec![
                    PlaneLayout {
                        rows: height,
                        row_bytes: width,
                    },
                    PlaneLayout {
                        rows: chroma_height,
                        row_bytes: chroma_width,
                    },
                    PlaneLayout {
                        rows: chroma_height,
                        row_bytes: chroma_width,
                    },
                ])
            }
            Self::Nv12 => {
                let chroma_height = height.div_ceil(2);
                Some(vec![
                    PlaneLayout {
                        rows: height,
                        row_bytes: width,
                    },
                    PlaneLayout {
                        rows: chroma_height,
                        row_bytes: width.div_ceil(2).checked_mul(2)?,
                    },
                ])
            }
        }
    }

    /// Total tightly packed byte length for this layout.
    pub fn packed_len(self, width: u32, height: u32) -> Option<usize> {
        self.plane_layout(width, height)?
            .iter()
            .try_fold(0usize, |total, plane| {
                total.checked_add(plane.rows.checked_mul(plane.row_bytes)?)
            })
    }

    /// Map a SPA video format onto a layout this crate can carry.
    pub fn from_spa(format: VideoFormat) -> Option<Self> {
        match format {
            VideoFormat::BGRA => Some(Self::Bgra),
            VideoFormat::BGRx => Some(Self::Bgrx),
            VideoFormat::RGBx => Some(Self::Rgbx),
            VideoFormat::I420 => Some(Self::I420),
            VideoFormat::NV12 => Some(Self::Nv12),
            VideoFormat::YUY2 => Some(Self::Yuyv),
            _ => None,
        }
    }

    /// SPA video format for this layout.
    pub fn to_spa(self) -> VideoFormat {
        match self {
            Self::Bgra => VideoFormat::BGRA,
            Self::Bgrx => VideoFormat::BGRx,
            Self::Rgbx => VideoFormat::RGBx,
            Self::I420 => VideoFormat::I420,
            Self::Nv12 => VideoFormat::NV12,
            Self::Yuyv => VideoFormat::YUY2,
        }
    }
}

/// Format negotiated with a video `Stream`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct NegotiatedVideoFormat {
    /// Raw layout the graph selected.
    pub format: VideoPixelFormat,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Frame-rate numerator reported on the format parameter (may be zero).
    pub fps_num: u32,
    /// Frame-rate denominator reported on the format parameter (may be zero).
    pub fps_den: u32,
    /// Tightly packed row stride in bytes for the first plane.
    pub stride: usize,
}

impl NegotiatedVideoFormat {
    /// Human-readable description for logs and diagnostics.
    pub fn summary(&self) -> String {
        format!(
            "{} {}x{} stride={} fps={}/{}",
            self.format.label(),
            self.width,
            self.height,
            self.stride,
            self.fps_num,
            self.fps_den
        )
    }
}

/// Configuration for [`PipeWireVideoSource`].
#[derive(Debug, Clone)]
pub struct VideoCaptureConfig {
    /// Default video source, a named object, or a node id.
    pub target: StreamTarget,
    /// Stream/node name advertised in the graph.
    pub stream_name: String,
    /// Optional PipeWire remote name override (e.g. `pipewire-0`).
    pub remote: Option<String>,
    /// Requested width.  The source may negotiate a different size.
    pub width: u32,
    /// Requested height.  The source may negotiate a different size.
    pub height: u32,
    /// Requested frame rate.  The source may negotiate a different rate.
    pub fps: u32,
    /// Number of complete frames buffered between the realtime callback and
    /// `next_frame()`.  Larger smooths scheduling jitter; smaller bounds latency.
    pub queue_capacity: usize,
    /// Maximum time to wait for the first negotiated format.
    pub startup_timeout: Duration,
    /// Maximum time to wait for the PipeWire thread to exit during `stop`.
    pub stop_timeout: Duration,
}

impl Default for VideoCaptureConfig {
    fn default() -> Self {
        Self {
            target: StreamTarget::Default,
            stream_name: "ozma-video-capture".to_string(),
            remote: None,
            width: 1920,
            height: 1080,
            fps: 30,
            queue_capacity: 4,
            startup_timeout: Duration::from_secs(5),
            stop_timeout: Duration::from_secs(2),
        }
    }
}

impl VideoCaptureConfig {
    fn normalized(&self) -> Self {
        let mut config = self.clone();
        config.stream_name = if config.stream_name.trim().is_empty() {
            "ozma-video-capture".to_string()
        } else {
            config.stream_name.clone()
        };
        config.width = config.width.max(1);
        config.height = config.height.max(1);
        config.fps = config.fps.max(1);
        config.queue_capacity = config.queue_capacity.max(1);
        config
    }
}

/// An owned raw video frame.
///
/// `data` is tightly packed: rows contain no padding, and planar layouts are
/// concatenated plane-by-plane (Y, then U/V).  `stride` is the packed row size
/// of the first plane, so it is exactly `format.row_bytes(width)`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    /// Monotonic presentation timestamp, microseconds since stream start.
    pub pts_micros: u64,
    /// Monotonically increasing frame counter for this stream.
    pub sequence: u64,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Packed row stride in bytes for the first plane.
    pub stride: usize,
    /// Raw layout of `data`.
    pub format: VideoPixelFormat,
    /// Packed frame bytes.
    pub data: Vec<u8>,
}

impl VideoFrame {
    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// True when the payload is empty.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }

    /// True when `data` contains at least one complete packed frame.
    pub fn is_complete(&self) -> bool {
        self.format
            .packed_len(self.width, self.height)
            .map(|expected| self.data.len() >= expected)
            .unwrap_or(false)
    }
}

/// Snapshot of video capture counters.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoCaptureStats {
    /// Frames delivered to `next_frame()` / `try_next_frame()`.
    pub frames: u64,
    /// Frames dropped because the async queue was full or closed.
    pub dropped_frames: u64,
    /// Dequeue, layout, or copy errors.
    pub errors: u64,
    /// Currently negotiated layout, when known.
    pub format: Option<VideoPixelFormat>,
    /// Negotiated width, or zero before negotiation.
    pub width: u32,
    /// Negotiated height, or zero before negotiation.
    pub height: u32,
    /// Negotiated packed stride for the first plane, or zero before negotiation.
    pub stride: usize,
}

#[derive(Default)]
struct VideoCounters {
    frames: AtomicU64,
    dropped_frames: AtomicU64,
    errors: AtomicU64,
    format: Mutex<Option<NegotiatedVideoFormat>>,
}

impl VideoCounters {
    fn set_format(&self, format: NegotiatedVideoFormat) {
        if let Ok(mut slot) = self.format.lock() {
            *slot = Some(format);
        }
    }

    fn clear_format(&self) {
        if let Ok(mut slot) = self.format.lock() {
            *slot = None;
        }
    }

    fn snapshot(&self) -> VideoCaptureStats {
        let format = self.format.lock().map(|slot| *slot).unwrap_or(None);
        VideoCaptureStats {
            frames: self.frames.load(Ordering::Relaxed),
            dropped_frames: self.dropped_frames.load(Ordering::Relaxed),
            errors: self.errors.load(Ordering::Relaxed),
            format: format.map(|negotiated| negotiated.format),
            width: format.map_or(0, |negotiated| negotiated.width),
            height: format.map_or(0, |negotiated| negotiated.height),
            stride: format.map_or(0, |negotiated| negotiated.stride),
        }
    }
}

/// A `Video/Source` node discovered on the PipeWire graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoNode {
    /// PipeWire global id (not stable across daemon restarts).
    pub id: u32,
    /// `node.name`, stable for a configured source.
    pub name: String,
    /// `node.description` (or `node.nick`) for humans.
    pub description: String,
    /// PipeWire media class; normally `Video/Source`.
    pub media_class: String,
    /// `object.path` when present, for example `v4l2:/dev/video0`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

impl VideoNode {
    /// True when this node advertises the video source media class.
    pub fn is_video_source(&self) -> bool {
        self.media_class == "Video/Source" || self.media_class.starts_with("Video/Source/")
    }
}

/// Result of a video-source discovery probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoStatus {
    /// True when the graph answered with a sync round-trip.
    pub reachable: bool,
    /// Remote name/socket used for the connection.
    pub remote: Option<String>,
    /// Discovered `Video/Source` nodes.
    pub sources: Vec<VideoNode>,
    /// Time spent on the probe.
    pub probe_micros: u64,
    /// Why the probe failed, when `reachable` is false.
    pub error: Option<StatusError>,
}

impl VideoStatus {
    /// True when the graph exposes at least one video source.
    pub fn has_video(&self) -> bool {
        !self.sources.is_empty()
    }

    /// Short human-readable summary for diagnostics.
    pub fn summary(&self) -> String {
        if let Some(error) = &self.error {
            return format!(
                "PipeWire video unreachable via {}: {} ({})",
                self.remote.as_deref().unwrap_or("default remote"),
                error.message,
                error.code
            );
        }
        let names: Vec<&str> = self
            .sources
            .iter()
            .map(|source| source.name.as_str())
            .collect();
        format!(
            "PipeWire video reachable via {} ({} source(s){})",
            self.remote.as_deref().unwrap_or("default remote"),
            self.sources.len(),
            if names.is_empty() {
                String::new()
            } else {
                format!(": {}", names.join(", "))
            }
        )
    }

    fn unreachable(remote: Option<String>, error: &PipeWireError) -> Self {
        Self {
            reachable: false,
            remote,
            sources: Vec::new(),
            probe_micros: 0,
            error: Some(StatusError {
                code: error.code().to_string(),
                message: error.to_string(),
                remediation: error.remediation().to_string(),
            }),
        }
    }
}

/// Optional overrides for video discovery.
#[derive(Debug, Clone)]
pub struct VideoProbeConfig {
    /// PipeWire remote name (normally from `PIPEWIRE_REMOTE`).
    pub remote: Option<String>,
    /// Maximum time to wait for the sync round-trip.
    pub timeout: Duration,
    /// Maximum time to wait for the probe thread to exit after a timeout.
    pub stop_timeout: Duration,
}

impl Default for VideoProbeConfig {
    fn default() -> Self {
        Self {
            remote: None,
            timeout: Duration::from_secs(2),
            stop_timeout: Duration::from_secs(2),
        }
    }
}

/// Build one `EnumFormat` pod for a raw video layout.
///
/// Width, height, and frame rate are expressed as ranges whose default is the
/// requested value.  A source that cannot provide the exact request can still
/// negotiate its nearest supported mode, and [`parse_video_format`] reports
/// exactly what was chosen.
pub fn build_video_format_pod(
    format: VideoPixelFormat,
    width: u32,
    height: u32,
    fps: u32,
) -> Result<Vec<u8>, PipeWireError> {
    if width == 0 || height == 0 || width > 16_384 || height > 16_384 {
        return Err(PipeWireError::video_format_unsupported(
            format!("{width}x{height}"),
            "1x1..16384x16384",
        ));
    }
    let fps = fps.max(1).min(1_000);

    let object = pod::object!(
        SpaTypes::ObjectParamFormat,
        ParamType::EnumFormat,
        pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
        pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
        pod::property!(FormatProperties::VideoFormat, Id, format.to_spa()),
        pod::property!(
            FormatProperties::VideoSize,
            Choice,
            Range,
            Rectangle,
            Rectangle { width, height },
            Rectangle {
                width: 1,
                height: 1
            },
            Rectangle {
                width: 16_384,
                height: 16_384
            }
        ),
        pod::property!(
            FormatProperties::VideoFramerate,
            Choice,
            Range,
            Fraction,
            Fraction { num: fps, denom: 1 },
            Fraction { num: 1, denom: 1 },
            Fraction {
                num: 1_000,
                denom: 1
            }
        ),
    );

    let serialized =
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
            .map_err(|error| PipeWireError::stream(format!("serialize video format: {error:?}")))?;
    Ok(serialized.0.into_inner())
}

/// Build a `ParamBuffers` pod for the negotiated raw layout.
///
/// Capture consumers live in a different process from the graph's producer, so
/// the client input port asks for MAP FD (`MemFd`) buffers of the exact packed
/// geometry rather than inheriting the producer's MMAP pointers.  Without this,
/// a V4L2 source that hands out `MemPtr` buffers cannot satisfy the link and
/// PipeWire fails with `use input buffers: -22`.
pub fn build_buffer_pod(format: NegotiatedVideoFormat) -> Result<Vec<u8>, PipeWireError> {
    let size = format
        .format
        .packed_len(format.width, format.height)
        .ok_or_else(|| {
            PipeWireError::video_format_unsupported(
                format!("{}x{}", format.width, format.height),
                "a frame size that fits in memory",
            )
        })?;
    let size = i32::try_from(size).map_err(|_| {
        PipeWireError::video_format_unsupported(
            format!("{size} bytes"),
            "a frame smaller than 2 GiB",
        )
    })?;
    let stride = i32::try_from(format.stride).map_err(|_| {
        PipeWireError::video_format_unsupported(
            format!("{}-byte stride", format.stride),
            "a stride smaller than 2 GiB",
        )
    })?;

    let buffers = Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Range {
            default: 4,
            min: 2,
            max: 32,
        },
    )));
    let data_type = Value::Choice(ChoiceValue::Int(Choice(
        ChoiceFlags::empty(),
        ChoiceEnum::Flags {
            default: 1 << spa::sys::SPA_DATA_MemFd,
            flags: Vec::new(),
        },
    )));

    let object = Object {
        type_: SpaTypes::ObjectParamBuffers.as_raw(),
        id: ParamType::Buffers.as_raw(),
        properties: vec![
            Property::new(spa::sys::SPA_PARAM_BUFFERS_buffers, buffers),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_blocks, Value::Int(1)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_size, Value::Int(size)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_stride, Value::Int(stride)),
            Property::new(spa::sys::SPA_PARAM_BUFFERS_dataType, data_type),
        ],
    };

    let serialized =
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
            .map_err(|error| {
                PipeWireError::stream(format!("serialize video buffers: {error:?}"))
            })?;
    Ok(serialized.0.into_inner())
}

/// Parse a negotiated video `Format` parameter.
///
/// Non-video parameters, compressed video, unsupported raw layouts, and empty
/// frames are all rejected with typed errors.
pub fn parse_video_format(param: &Pod) -> Result<NegotiatedVideoFormat, PipeWireError> {
    let (media_type, media_subtype) = spa::param::format_utils::parse_format(param)
        .map_err(|error| PipeWireError::stream(format!("parse video format: {error:?}")))?;

    if media_type != MediaType::Video {
        return Err(PipeWireError::video_format_unsupported(
            format!("{media_type:?}/{media_subtype:?}"),
            "Video/Raw",
        ));
    }
    if media_subtype != MediaSubtype::Raw {
        return Err(PipeWireError::compressed_video(format!(
            "{media_type:?}/{media_subtype:?}"
        )));
    }

    let mut info = VideoInfoRaw::new();
    info.parse(param)
        .map_err(|error| PipeWireError::stream(format!("parse raw video info: {error:?}")))?;

    let format = VideoPixelFormat::from_spa(info.format()).ok_or_else(|| {
        PipeWireError::video_format_unsupported(
            format!("{:?}", info.format()),
            "BGRA, BGRx, RGBx, I420, NV12, or YUYV",
        )
    })?;

    let size = info.size();
    if size.width == 0 || size.height == 0 {
        return Err(PipeWireError::video_format_unsupported(
            format!("{}x{}", size.width, size.height),
            "a non-empty frame size",
        ));
    }
    let stride = format.row_bytes(size.width).ok_or_else(|| {
        PipeWireError::video_format_unsupported(
            format!("{}x{}", size.width, size.height),
            "a frame size that fits in memory",
        )
    })?;
    let rate = info.framerate();

    Ok(NegotiatedVideoFormat {
        format,
        width: size.width,
        height: size.height,
        fps_num: rate.num,
        fps_den: rate.denom,
        stride,
    })
}

/// Pack one stream buffer into tightly packed owned bytes.
///
/// Handles the two common PipeWire shapes: one buffer per plane, and one
/// contiguous buffer containing every plane.  `stride` is the chunk stride of
/// the first plane; negative/zero strides fall back to tightly packed rows.
fn pack_buffer(
    format: NegotiatedVideoFormat,
    datas: &mut [spa::buffer::Data],
) -> Result<Vec<u8>, String> {
    let planes = format
        .format
        .plane_layout(format.width, format.height)
        .ok_or_else(|| "invalid negotiated frame size".to_string())?;
    let total = format
        .format
        .packed_len(format.width, format.height)
        .ok_or_else(|| "frame size overflows memory accounting".to_string())?;
    let mut out = Vec::with_capacity(total);

    if datas.len() == planes.len() {
        for (data, plane) in datas.iter_mut().zip(&planes) {
            copy_plane(data, *plane, &mut out)?;
        }
    } else if datas.len() == 1 {
        copy_contiguous_planes(&mut datas[0], format, &planes, &mut out)?;
    } else {
        return Err(format!(
            "expected {} video plane(s), got {}",
            planes.len(),
            datas.len()
        ));
    }

    if out.len() < total {
        return Err(format!(
            "packed {} bytes for a {}-byte {} frame",
            out.len(),
            total,
            format.format.label()
        ));
    }
    Ok(out)
}

fn copy_plane(
    data: &mut spa::buffer::Data,
    plane: PlaneLayout,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let chunk = data.chunk();
    let offset = chunk.offset() as usize;
    let size = chunk.size() as usize;
    let stride = chunk.stride();
    let Some(slice) = data.data() else {
        return Err("plane has no mapped memory".to_string());
    };
    let stride = if stride > 0 {
        stride as usize
    } else {
        plane.row_bytes
    };
    copy_rows(slice, offset, size, stride, plane, out)
}

fn copy_contiguous_planes(
    data: &mut spa::buffer::Data,
    format: NegotiatedVideoFormat,
    planes: &[PlaneLayout],
    out: &mut Vec<u8>,
) -> Result<(), String> {
    let chunk = data.chunk();
    let offset = chunk.offset() as usize;
    let size = chunk.size() as usize;
    let first_stride = chunk.stride();
    let Some(slice) = data.data() else {
        return Err("frame has no mapped memory".to_string());
    };

    let first_row_bytes = planes[0].row_bytes;
    let stride0 = if first_stride > 0 {
        first_stride as usize
    } else {
        first_row_bytes
    };

    let mut plane_offset = offset;
    for (index, plane) in planes.iter().enumerate() {
        // I420 chroma rows are half the luma width; NV12 chroma rows have the
        // same byte width as a luma row for even widths.  When the graph gives
        // an explicit stride, honour the conventional relationship.
        let stride = match (format.format, index) {
            (VideoPixelFormat::I420, 1 | 2) => (stride0 / 2).max(plane.row_bytes),
            _ => stride0.max(plane.row_bytes),
        };
        copy_rows(slice, plane_offset, size, stride, *plane, out)?;
        plane_offset = plane_offset
            .checked_add(
                stride
                    .checked_mul(plane.rows)
                    .ok_or_else(|| "plane size overflows".to_string())?,
            )
            .ok_or_else(|| "plane offset overflows".to_string())?;
    }
    Ok(())
}

fn copy_rows(
    slice: &[u8],
    offset: usize,
    size: usize,
    stride: usize,
    plane: PlaneLayout,
    out: &mut Vec<u8>,
) -> Result<(), String> {
    if stride < plane.row_bytes {
        return Err(format!(
            "row stride {stride} is smaller than {} bytes",
            plane.row_bytes
        ));
    }
    if plane.rows == 0 || plane.row_bytes == 0 {
        return Ok(());
    }
    let end = offset
        .checked_add(
            stride
                .checked_mul(plane.rows.saturating_sub(1))
                .and_then(|n| n.checked_add(plane.row_bytes))
                .ok_or_else(|| "plane extent overflows".to_string())?,
        )
        .ok_or_else(|| "plane extent overflows".to_string())?;

    // Some producers report a chunk size for the whole plane while others set
    // it per row.  Use whichever bound is smaller but never read past memory.
    let chunk_end = offset.saturating_add(size).min(slice.len());
    if end > slice.len() || (size != 0 && end > chunk_end) {
        return Err(format!(
            "plane needs {end} bytes but only {} are mapped",
            chunk_end.min(slice.len())
        ));
    }

    out.reserve(plane.rows * plane.row_bytes);
    for row in 0..plane.rows {
        let start = offset + row * stride;
        out.extend_from_slice(&slice[start..start + plane.row_bytes]);
    }
    Ok(())
}

enum VideoCommand {
    Quit,
}

struct VideoReadySlot {
    inner: Mutex<Option<std_mpsc::SyncSender<Result<NegotiatedVideoFormat, PipeWireError>>>>,
}

impl VideoReadySlot {
    fn new(tx: std_mpsc::SyncSender<Result<NegotiatedVideoFormat, PipeWireError>>) -> Arc<Self> {
        Arc::new(Self {
            inner: Mutex::new(Some(tx)),
        })
    }

    fn send(&self, result: Result<NegotiatedVideoFormat, PipeWireError>) {
        let sender = self.inner.lock().ok().and_then(|mut slot| slot.take());
        if let Some(tx) = sender {
            let _ = tx.try_send(result);
        }
    }
}

struct VideoUserData {
    format: Option<NegotiatedVideoFormat>,
    sequence: u64,
    started: Instant,
    tx: mpsc::Sender<VideoFrame>,
    ready: Arc<VideoReadySlot>,
    shared: Arc<SharedError>,
    counters: Arc<VideoCounters>,
    bound_node: Arc<AtomicU32>,
}

/// Native PipeWire video capture stream.
///
/// The PipeWire main loop runs on a dedicated OS thread.  Dropping the source
/// (or calling [`stop`](Self::stop)) quits that loop and joins the thread.
pub struct PipeWireVideoSource {
    format: NegotiatedVideoFormat,
    frames: mpsc::Receiver<VideoFrame>,
    cmd_tx: Option<pw::channel::Sender<VideoCommand>>,
    done_rx: Option<std_mpsc::Receiver<()>>,
    thread: Option<JoinHandle<()>>,
    counters: Arc<VideoCounters>,
    shared: Arc<SharedError>,
    bound_node: Arc<AtomicU32>,
    stop_timeout: Duration,
}

impl PipeWireVideoSource {
    /// Open the configured target and wait for format negotiation.
    ///
    /// This is the synchronous form used by capture providers.  It is bounded
    /// by [`VideoCaptureConfig::startup_timeout`] and never blocks longer than
    /// that plus `stop_timeout` while cleaning up a failed start.
    pub fn open_blocking(config: VideoCaptureConfig) -> Result<Self, PipeWireError> {
        let config = config.normalized();
        let startup_timeout = config.startup_timeout;
        let stop_timeout = config.stop_timeout;
        let target = config.target.clone();

        let (frame_tx, frames) = mpsc::channel::<VideoFrame>(config.queue_capacity);
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let ready = VideoReadySlot::new(ready_tx);
        let (cmd_tx, cmd_rx) = pw::channel::channel::<VideoCommand>();
        let (done_tx, done_rx) = std_mpsc::channel::<()>();
        let counters = Arc::new(VideoCounters::default());
        let shared = Arc::new(SharedError::default());
        let bound_node = Arc::new(AtomicU32::new(u32::MAX));

        let thread_counters = Arc::clone(&counters);
        let thread_shared = Arc::clone(&shared);
        let thread_bound = Arc::clone(&bound_node);
        let thread_ready = Arc::clone(&ready);

        let handle = thread::Builder::new()
            .name("ozma-pw-video-capture".into())
            .spawn(move || {
                video_thread_main(
                    config,
                    frame_tx,
                    thread_ready,
                    cmd_rx,
                    thread_counters,
                    thread_shared,
                    thread_bound,
                );
                drop(done_tx);
            })
            .map_err(|error| PipeWireError::Thread {
                detail: error.to_string(),
            })?;

        let negotiated = match ready_rx.recv_timeout(startup_timeout) {
            Ok(result) => result,
            Err(std_mpsc::RecvTimeoutError::Timeout) => Err(PipeWireError::StartTimeout {
                detail: format!(
                    "no video format negotiated for {} within {:?}",
                    target.describe(),
                    startup_timeout
                ),
            }),
            Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(PipeWireError::disconnected(
                "video capture thread exited before the stream negotiated a format",
            )),
        };

        match negotiated {
            Ok(format) => {
                debug!(
                    format = format.format.label(),
                    width = format.width,
                    height = format.height,
                    stride = format.stride,
                    target = %target.describe(),
                    "PipeWire video capture ready"
                );
                Ok(Self {
                    format,
                    frames,
                    cmd_tx: Some(cmd_tx),
                    done_rx: Some(done_rx),
                    thread: Some(handle),
                    counters,
                    shared,
                    bound_node,
                    stop_timeout,
                })
            }
            Err(error) => {
                let _ = cmd_tx.send(VideoCommand::Quit);
                let finished = matches!(
                    done_rx.recv_timeout(stop_timeout),
                    Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected)
                );
                if finished {
                    let _ = handle.join();
                } else {
                    warn!("video capture thread did not exit after startup failure; detaching");
                }
                Err(error)
            }
        }
    }

    /// Async form of [`open_blocking`](Self::open_blocking).
    ///
    /// The PipeWire setup and format wait happen on a blocking worker so the
    /// async runtime is never stalled.
    pub async fn new(config: VideoCaptureConfig) -> Result<Self, PipeWireError> {
        let startup_timeout = config.startup_timeout;
        let stop_timeout = config.stop_timeout;
        let handle = tokio::task::spawn_blocking(move || Self::open_blocking(config));
        let limit = startup_timeout
            .saturating_add(stop_timeout)
            .saturating_add(Duration::from_secs(1));
        match tokio::time::timeout(limit, handle).await {
            Ok(Ok(result)) => result,
            Ok(Err(error)) => Err(PipeWireError::Thread {
                detail: format!("video capture startup task failed: {error}"),
            }),
            Err(_) => Err(PipeWireError::StartTimeout {
                detail: format!("video capture startup exceeded {limit:?}"),
            }),
        }
    }

    /// The actual layout/size negotiated with the graph.
    pub fn negotiated_format(&self) -> NegotiatedVideoFormat {
        self.format
    }

    /// Graph node id this stream is connected to, once known.
    pub fn bound_node_id(&self) -> Option<u32> {
        match self.bound_node.load(Ordering::Relaxed) {
            u32::MAX => None,
            id => Some(id),
        }
    }

    pub fn stats(&self) -> VideoCaptureStats {
        self.counters.snapshot()
    }

    /// Take the last fatal stream error, if any.
    pub fn take_error(&self) -> Option<PipeWireError> {
        self.shared.take()
    }

    /// Wait for the next captured frame.
    ///
    /// Returns [`PipeWireError::Disconnected`] if the stream stopped.  Use
    /// `tokio::time::timeout` at the call site when a bounded wait is needed.
    pub async fn next_frame(&mut self) -> Result<VideoFrame, PipeWireError> {
        match self.frames.recv().await {
            Some(frame) => Ok(frame),
            None => Err(self
                .shared
                .take()
                .unwrap_or_else(|| PipeWireError::disconnected("video capture stream closed"))),
        }
    }

    /// Non-blocking variant of [`next_frame`](Self::next_frame).
    pub fn try_next_frame(&mut self) -> Result<Option<VideoFrame>, PipeWireError> {
        match self.frames.try_recv() {
            Ok(frame) => Ok(Some(frame)),
            Err(mpsc::error::TryRecvError::Empty) => Ok(None),
            Err(mpsc::error::TryRecvError::Disconnected) => Err(self
                .shared
                .take()
                .unwrap_or_else(|| PipeWireError::disconnected("video capture stream closed"))),
        }
    }

    /// Quit the PipeWire main loop and join its thread.
    ///
    /// Safe to call more than once; also called by `Drop`.
    pub fn stop(&mut self) {
        if let Some(tx) = self.cmd_tx.take() {
            let _ = tx.send(VideoCommand::Quit);
        }
        let finished = match self.done_rx.take() {
            Some(done) => matches!(
                done.recv_timeout(self.stop_timeout),
                Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected)
            ),
            None => true,
        };
        if let Some(handle) = self.thread.take() {
            if finished {
                let _ = handle.join();
            } else {
                warn!(
                    "video capture thread did not exit within {:?}; detaching",
                    self.stop_timeout
                );
            }
        }
    }
}

impl Drop for PipeWireVideoSource {
    fn drop(&mut self) {
        self.stop();
    }
}

const FLAGS: StreamFlags = StreamFlags::AUTOCONNECT
    .union(StreamFlags::MAP_BUFFERS)
    .union(StreamFlags::RT_PROCESS);

fn video_thread_main(
    config: VideoCaptureConfig,
    frame_tx: mpsc::Sender<VideoFrame>,
    ready: Arc<VideoReadySlot>,
    cmd_rx: pw::channel::Receiver<VideoCommand>,
    counters: Arc<VideoCounters>,
    shared: Arc<SharedError>,
    bound_node: Arc<AtomicU32>,
) {
    pw::init();

    let main_loop = match MainLoopRc::new(None) {
        Ok(main_loop) => main_loop,
        Err(error) => {
            let err = PipeWireError::unavailable(format!("create PipeWire main loop: {error}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let quit_loop = main_loop.clone();
    let _cmd_listener = cmd_rx.attach(main_loop.loop_(), move |_| quit_loop.quit());

    let mut context_props = properties! {
        *pw::keys::APP_NAME => "ozma-pipewire",
    };
    if let Some(remote) = config.remote.as_deref() {
        context_props.insert(*pw::keys::REMOTE_NAME, remote);
    }
    let context = match pw::context::ContextRc::new(&main_loop, Some(context_props)) {
        Ok(context) => context,
        Err(error) => {
            let err = PipeWireError::unavailable(format!("create PipeWire context: {error}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(error) => {
            let err = PipeWireError::unavailable(format!("connect to PipeWire: {error}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let mut stream_props = properties! {
        *pw::keys::MEDIA_TYPE => "Video",
        *pw::keys::MEDIA_CATEGORY => "Capture",
        *pw::keys::MEDIA_ROLE => "Camera",
        *pw::keys::NODE_NAME => config.stream_name.as_str(),
        *pw::keys::NODE_DESCRIPTION => "Ozma video capture",
        *pw::keys::APP_NAME => "ozma-pipewire",
    };
    if let Some(name) = config.target.object_name() {
        stream_props.insert("target.object", name);
    }

    let stream = match StreamBox::new(&core, &config.stream_name, stream_props) {
        Ok(stream) => stream,
        Err(error) => {
            let err = PipeWireError::stream(format!("create video capture stream: {error}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let user_data = VideoUserData {
        format: None,
        sequence: 0,
        started: Instant::now(),
        tx: frame_tx,
        ready: Arc::clone(&ready),
        shared: Arc::clone(&shared),
        counters: Arc::clone(&counters),
        bound_node: Arc::clone(&bound_node),
    };

    let quit_on_closed = main_loop.clone();
    let quit_on_error = main_loop.clone();
    let quit_on_bad_format = main_loop.clone();
    let proc_counters = Arc::clone(&counters);
    let proc_bound = Arc::clone(&bound_node);
    let _listener = match stream
        .add_local_listener_with_user_data(user_data)
        .state_changed(move |stream, user_data, _old, new| match new {
            StreamState::Streaming => {
                user_data
                    .bound_node
                    .store(stream.node_id(), Ordering::Relaxed);
            }
            StreamState::Error(message) => {
                user_data.counters.errors.fetch_add(1, Ordering::Relaxed);
                let err = PipeWireError::stream_or_unavailable(message.clone());
                user_data.shared.set(err.clone());
                user_data.ready.send(Err(err));
                // Stop the thread so a blocked consumer sees the channel close
                // and can surface the recorded error instead of hanging.
                quit_on_error.quit();
            }
            _ => {}
        })
        .param_changed(move |stream, user_data, id, param| {
            if id != ParamType::Format.as_raw() {
                return;
            }
            // `None` clears the format; the next `Some` renegotiates.
            let Some(param) = param else {
                user_data.format = None;
                user_data.counters.clear_format();
                return;
            };
            match parse_video_format(param) {
                Ok(format) => {
                    user_data.format = Some(format);
                    user_data.counters.set_format(format);
                    user_data
                        .bound_node
                        .store(stream.node_id(), Ordering::Relaxed);
                    debug!(
                        format = format.format.label(),
                        width = format.width,
                        height = format.height,
                        stride = format.stride,
                        "video format negotiated"
                    );
                    // Ask the graph for shared-memory buffers of the exact
                    // packed geometry.  See `build_buffer_pod`.
                    if let Ok(bytes) = build_buffer_pod(format) {
                        if let Some(pod) = Pod::from_bytes(&bytes) {
                            if let Err(error) = stream.update_params(&mut [pod]) {
                                debug!(%error, "could not set video buffer parameters");
                            }
                        }
                    }
                    user_data.ready.send(Ok(format));
                }
                Err(error) => {
                    // Do not keep producing with a stale/unsupported format.
                    user_data.format = None;
                    user_data.counters.clear_format();
                    user_data.counters.errors.fetch_add(1, Ordering::Relaxed);
                    user_data.shared.set(error.clone());
                    user_data.ready.send(Err(error));
                    // An unsupported format cannot produce usable frames.
                    quit_on_bad_format.quit();
                }
            }
        })
        .process(move |stream, user_data| {
            process_video(
                stream,
                user_data,
                &proc_counters,
                &proc_bound,
                &quit_on_closed,
            );
        })
        .register()
    {
        Ok(listener) => listener,
        Err(error) => {
            let err = PipeWireError::stream(format!("register video callbacks: {error}"));
            shared.set(err.clone());
            ready.send(Err(err));
            return;
        }
    };

    let mut format_bytes = Vec::new();
    for format in VideoPixelFormat::SUPPORTED {
        match build_video_format_pod(format, config.width, config.height, config.fps) {
            Ok(bytes) => format_bytes.push(bytes),
            Err(error) => {
                shared.set(error.clone());
                ready.send(Err(error));
                return;
            }
        }
    }

    let mut params: Vec<&Pod> = Vec::new();
    for bytes in &format_bytes {
        match Pod::from_bytes(bytes) {
            Some(pod) => params.push(pod),
            None => {
                let err = PipeWireError::stream("invalid serialized video format pod");
                shared.set(err.clone());
                ready.send(Err(err));
                return;
            }
        }
    }

    if let Err(error) = stream.connect(
        Direction::Input,
        config.target.node_id(),
        FLAGS,
        &mut params,
    ) {
        let err = PipeWireError::stream(format!("connect video capture stream: {error}"));
        shared.set(err.clone());
        ready.send(Err(err));
        return;
    }

    main_loop.run();
}

fn process_video(
    stream: &pw::stream::Stream,
    user_data: &mut VideoUserData,
    counters: &VideoCounters,
    bound_node: &AtomicU32,
    main_loop: &MainLoopRc,
) {
    let Some(format) = user_data.format else {
        return;
    };
    let node_id = stream.node_id();
    if node_id != u32::MAX {
        bound_node.store(node_id, Ordering::Relaxed);
    }

    let Some(mut buffer) = stream.dequeue_buffer() else {
        counters.errors.fetch_add(1, Ordering::Relaxed);
        return;
    };
    let datas = buffer.datas_mut();

    let data = match pack_buffer(format, datas) {
        Ok(data) if !data.is_empty() => data,
        Ok(_) => {
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
        Err(error) => {
            debug!(%error, "could not pack a PipeWire video frame");
            counters.errors.fetch_add(1, Ordering::Relaxed);
            return;
        }
    };

    let frame = VideoFrame {
        pts_micros: user_data.started.elapsed().as_micros() as u64,
        sequence: user_data.sequence,
        width: format.width,
        height: format.height,
        stride: format.stride,
        format: format.format,
        data,
    };
    user_data.sequence = user_data.sequence.wrapping_add(1);

    match user_data.tx.try_send(frame) {
        Ok(()) => {
            counters.frames.fetch_add(1, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Full(_)) => {
            counters.dropped_frames.fetch_add(1, Ordering::Relaxed);
        }
        Err(mpsc::error::TrySendError::Closed(_)) => {
            counters.dropped_frames.fetch_add(1, Ordering::Relaxed);
            // The async owner is gone; stop capturing.
            main_loop.quit();
        }
    }
}

/// Probe the graph for `Video/Source` nodes with a bounded timeout.
///
/// Synchronous form, suitable for [`crate::PipeWireCaptureProvider`]-style
/// discovery.  A missing daemon is reported as [`PipeWireError::Unavailable`]
/// or [`PipeWireError::ProbeTimeout`], never as an empty success.
pub fn probe_video_sources_blocking(
    config: VideoProbeConfig,
) -> Result<Vec<VideoNode>, PipeWireError> {
    let timeout = config.timeout;
    let stop_timeout = config.stop_timeout;
    let remote = configured_remote(config.remote.clone());

    let (cmd_tx, cmd_rx) = pw::channel::channel::<VideoProbeCommand>();
    let (result_tx, result_rx) = std_mpsc::sync_channel(1);
    let (done_tx, done_rx) = std_mpsc::channel::<()>();

    let thread = thread::Builder::new()
        .name("ozma-pw-video-probe".into())
        .spawn(move || {
            video_probe_thread_main(config, cmd_rx, result_tx);
            drop(done_tx);
        })
        .map_err(|error| PipeWireError::Thread {
            detail: error.to_string(),
        })?;

    let outcome = match result_rx.recv_timeout(timeout) {
        Ok(result) => result,
        Err(std_mpsc::RecvTimeoutError::Timeout) => Err(PipeWireError::ProbeTimeout {
            detail: format!(
                "no video-source reply from {} within {:?}",
                remote.as_deref().unwrap_or("default remote"),
                timeout
            ),
        }),
        Err(std_mpsc::RecvTimeoutError::Disconnected) => Err(PipeWireError::unavailable(
            "PipeWire video probe thread exited without a result",
        )),
    };

    if outcome.is_err() {
        let _ = cmd_tx.send(VideoProbeCommand::Quit);
    }
    let finished = matches!(
        done_rx.recv_timeout(stop_timeout),
        Ok(()) | Err(std_mpsc::RecvTimeoutError::Disconnected)
    );
    if finished {
        let _ = thread.join();
    } else {
        debug!("PipeWire video probe thread did not exit; detaching");
    }

    outcome
}

/// Async form of [`probe_video_sources_blocking`].
pub async fn probe_video_sources_with(
    config: VideoProbeConfig,
) -> Result<Vec<VideoNode>, PipeWireError> {
    let timeout = config
        .timeout
        .saturating_add(config.stop_timeout)
        .saturating_add(Duration::from_secs(1));
    let handle = tokio::task::spawn_blocking(move || probe_video_sources_blocking(config));
    match tokio::time::timeout(timeout, handle).await {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => Err(PipeWireError::Thread {
            detail: format!("video probe task failed: {error}"),
        }),
        Err(_) => Err(PipeWireError::ProbeTimeout {
            detail: format!("video probe exceeded {timeout:?}"),
        }),
    }
}

/// Probe for `Video/Source` nodes with a caller-supplied timeout.
pub async fn probe_video_sources(timeout: Duration) -> Result<Vec<VideoNode>, PipeWireError> {
    probe_video_sources_with(VideoProbeConfig {
        timeout,
        ..VideoProbeConfig::default()
    })
    .await
}

/// Probe and always return a [`VideoStatus`], recording failures in `error`.
pub async fn probe_video_status(config: VideoProbeConfig) -> VideoStatus {
    let remote = configured_remote(config.remote.clone());
    let started = Instant::now();
    match probe_video_sources_with(config).await {
        Ok(sources) => VideoStatus {
            reachable: true,
            remote,
            sources,
            probe_micros: started.elapsed().as_micros() as u64,
            error: None,
        },
        Err(error) => VideoStatus::unreachable(remote, &error),
    }
}

fn configured_remote(override_remote: Option<String>) -> Option<String> {
    override_remote
        .or_else(|| env::var("PIPEWIRE_REMOTE").ok())
        .or_else(|| {
            env::var("XDG_RUNTIME_DIR")
                .ok()
                .map(|dir| format!("{dir}/pipewire-0"))
        })
}

enum VideoProbeCommand {
    Quit,
}

fn video_probe_thread_main(
    config: VideoProbeConfig,
    cmd_rx: pw::channel::Receiver<VideoProbeCommand>,
    result_tx: std_mpsc::SyncSender<Result<Vec<VideoNode>, PipeWireError>>,
) {
    pw::init();

    let main_loop = match MainLoopRc::new(None) {
        Ok(main_loop) => main_loop,
        Err(error) => {
            let _ = result_tx.try_send(Err(PipeWireError::unavailable(format!(
                "create PipeWire main loop: {error}"
            ))));
            return;
        }
    };

    let quit_loop = main_loop.clone();
    let _cmd_listener = cmd_rx.attach(main_loop.loop_(), move |_| quit_loop.quit());

    let mut context_props = properties! {
        *pw::keys::APP_NAME => "ozma-pipewire-video-probe",
    };
    if let Some(remote) = config.remote.as_deref() {
        context_props.insert(*pw::keys::REMOTE_NAME, remote);
    }
    let context = match pw::context::ContextRc::new(&main_loop, Some(context_props)) {
        Ok(context) => context,
        Err(error) => {
            let _ = result_tx.try_send(Err(PipeWireError::unavailable(format!(
                "create PipeWire context: {error}"
            ))));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(error) => {
            let _ = result_tx.try_send(Err(PipeWireError::unavailable(format!(
                "connect to PipeWire: {error}"
            ))));
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(registry) => registry,
        Err(error) => {
            let _ = result_tx.try_send(Err(PipeWireError::unavailable(format!(
                "get PipeWire registry: {error}"
            ))));
            return;
        }
    };

    let nodes: Rc<RefCell<HashMap<u32, VideoNode>>> = Rc::new(RefCell::new(HashMap::new()));
    let error_slot: Rc<RefCell<Option<String>>> = Rc::new(RefCell::new(None));
    let done = Rc::new(RefCell::new(None::<spa::utils::result::AsyncSeq>));

    let nodes_for_global = Rc::clone(&nodes);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            if global.type_ != ObjectType::Node {
                return;
            }
            let Some(props) = global.props else {
                return;
            };
            let media_class = props.get("media.class").unwrap_or_default();
            if media_class != "Video/Source" && !media_class.starts_with("Video/Source/") {
                return;
            }
            let name = props.get("node.name").unwrap_or_default().to_string();
            if name.is_empty() {
                return;
            }
            let description = props
                .get("node.description")
                .or_else(|| props.get("node.nick"))
                .unwrap_or(&name)
                .to_string();
            let path = props.get("object.path").map(str::to_string);
            if let Ok(mut nodes) = nodes_for_global.try_borrow_mut() {
                nodes.insert(
                    global.id,
                    VideoNode {
                        id: global.id,
                        name,
                        description,
                        media_class: media_class.to_string(),
                        path,
                    },
                );
            }
        })
        .global_remove({
            let nodes = Rc::clone(&nodes);
            move |id| {
                if let Ok(mut nodes) = nodes.try_borrow_mut() {
                    nodes.remove(&id);
                }
            }
        })
        .register();

    let sync = match core.sync(0) {
        Ok(sync) => sync,
        Err(error) => {
            let _ = result_tx.try_send(Err(PipeWireError::unavailable(format!(
                "PipeWire core sync failed: {error}"
            ))));
            return;
        }
    };
    if let Ok(mut slot) = done.try_borrow_mut() {
        *slot = Some(sync);
    }

    let done_for_listener = Rc::clone(&done);
    let quit_on_done = main_loop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            let is_done = done_for_listener
                .try_borrow()
                .map(|slot| *slot == Some(seq))
                .unwrap_or(false);
            if is_done {
                quit_on_done.quit();
            }
        })
        .error({
            let error_slot = Rc::clone(&error_slot);
            let quit_on_error = main_loop.clone();
            move |_id, _seq, _res, message| {
                if let Ok(mut slot) = error_slot.try_borrow_mut() {
                    *slot = Some(message.to_string());
                }
                quit_on_error.quit();
            }
        })
        .register();

    main_loop.run();

    if let Ok(error) = error_slot.try_borrow() {
        if let Some(error) = error.as_ref() {
            let _ = result_tx.try_send(Err(PipeWireError::unavailable(error.clone())));
            return;
        }
    }

    let mut sources: Vec<VideoNode> = nodes
        .try_borrow()
        .map(|nodes| nodes.values().cloned().collect())
        .unwrap_or_default();
    sources.sort_by(|a, b| a.name.cmp(&b.name));
    let _ = result_tx.try_send(Ok(sources));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixed_format_pod(
        format: VideoPixelFormat,
        width: u32,
        height: u32,
        fps_num: u32,
        fps_den: u32,
    ) -> Vec<u8> {
        let object = pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::Format,
            pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pod::property!(FormatProperties::VideoFormat, Id, format.to_spa()),
            pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                Rectangle { width, height }
            ),
            pod::property!(
                FormatProperties::VideoFramerate,
                Fraction,
                Fraction {
                    num: fps_num,
                    denom: fps_den
                }
            ),
        );
        PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
            .expect("serialize fixed video format")
            .0
            .into_inner()
    }

    #[test]
    fn format_preference_matches_the_contract() {
        assert_eq!(
            VideoPixelFormat::SUPPORTED,
            [
                VideoPixelFormat::Bgra,
                VideoPixelFormat::Bgrx,
                VideoPixelFormat::Rgbx,
                VideoPixelFormat::I420,
                VideoPixelFormat::Nv12,
                VideoPixelFormat::Yuyv,
            ]
        );
        for format in VideoPixelFormat::SUPPORTED {
            assert_eq!(VideoPixelFormat::from_spa(format.to_spa()), Some(format));
            assert!(!format.label().is_empty());
        }
        assert_eq!(
            VideoPixelFormat::from_spa(VideoFormat::NV21),
            None,
            "unsupported layouts must not be silently accepted"
        );
    }

    #[test]
    fn packed_layouts_have_expected_strides_and_lengths() {
        assert_eq!(VideoPixelFormat::Bgra.row_bytes(1920), Some(7680));
        assert_eq!(
            VideoPixelFormat::Bgra.packed_len(1920, 1080),
            Some(8_294_400)
        );
        assert_eq!(VideoPixelFormat::Rgbx.packed_len(4, 2), Some(32));
        assert_eq!(VideoPixelFormat::Yuyv.row_bytes(1920), Some(3840));
        assert_eq!(
            VideoPixelFormat::Yuyv.packed_len(1920, 1080),
            Some(1920 * 1080 * 2)
        );

        assert_eq!(VideoPixelFormat::Nv12.row_bytes(1920), Some(1920));
        assert_eq!(
            VideoPixelFormat::Nv12.packed_len(1920, 1080),
            Some(1920 * 1080 + 1920 * 540)
        );
        assert_eq!(
            VideoPixelFormat::I420.packed_len(1920, 1080),
            Some(1920 * 1080 + 2 * 960 * 540)
        );
        assert_eq!(VideoPixelFormat::I420.row_bytes(0), None);
        assert_eq!(VideoPixelFormat::Nv12.packed_len(0, 10), None);
    }

    #[test]
    fn odd_planar_sizes_round_chroma_up() {
        let nv12 = VideoPixelFormat::Nv12.plane_layout(3, 3).unwrap();
        assert_eq!(
            nv12[0],
            PlaneLayout {
                rows: 3,
                row_bytes: 3
            }
        );
        assert_eq!(
            nv12[1],
            PlaneLayout {
                rows: 2,
                row_bytes: 4
            }
        );
        let i420 = VideoPixelFormat::I420.plane_layout(5, 3).unwrap();
        assert_eq!(
            i420[1],
            PlaneLayout {
                rows: 2,
                row_bytes: 3
            }
        );
        assert_eq!(
            i420[2],
            PlaneLayout {
                rows: 2,
                row_bytes: 3
            }
        );
    }

    #[test]
    fn enum_format_pods_are_valid_video_pods() {
        for format in VideoPixelFormat::SUPPORTED {
            let bytes = build_video_format_pod(format, 1920, 1080, 30).expect("build pod");
            let pod = Pod::from_bytes(&bytes).expect("valid pod");
            let (media_type, media_subtype) =
                spa::param::format_utils::parse_format(pod).expect("parse media type");
            assert_eq!(media_type, MediaType::Video);
            assert_eq!(media_subtype, MediaSubtype::Raw);
        }
    }

    #[test]
    fn invalid_video_requests_are_typed_errors() {
        for (width, height) in [(0, 1080), (1920, 0), (20_000, 1080)] {
            let error =
                build_video_format_pod(VideoPixelFormat::Bgra, width, height, 30).unwrap_err();
            assert_eq!(error.code(), "pipewire.video_format_unsupported");
        }
    }

    #[test]
    fn buffer_pods_serialize_for_every_layout() {
        for format in VideoPixelFormat::SUPPORTED {
            let negotiated = NegotiatedVideoFormat {
                format,
                width: 640,
                height: 480,
                fps_num: 30,
                fps_den: 1,
                stride: format.row_bytes(640).unwrap(),
            };
            let bytes = build_buffer_pod(negotiated).expect("build buffer pod");
            assert!(!bytes.is_empty());
            assert!(Pod::from_bytes(&bytes).is_some(), "{format:?}");
        }

        let invalid = NegotiatedVideoFormat {
            format: VideoPixelFormat::Nv12,
            width: 0,
            height: 480,
            fps_num: 30,
            fps_den: 1,
            stride: 0,
        };
        let error = build_buffer_pod(invalid).unwrap_err();
        assert_eq!(error.code(), "pipewire.video_format_unsupported");
    }

    #[test]
    fn negotiated_format_round_trips_through_spa() {
        for format in VideoPixelFormat::SUPPORTED {
            let bytes = fixed_format_pod(format, 1280, 720, 60, 1);
            let pod = Pod::from_bytes(&bytes).expect("valid pod");
            let negotiated = parse_video_format(pod).expect("parse video format");
            assert_eq!(negotiated.format, format);
            assert_eq!(negotiated.width, 1280);
            assert_eq!(negotiated.height, 720);
            assert_eq!(negotiated.stride, format.row_bytes(1280).unwrap());
            assert_eq!(negotiated.fps_num, 60);
            assert_eq!(negotiated.fps_den, 1);
            assert!(negotiated.summary().contains(format.label()));
        }
    }

    #[test]
    fn compressed_video_is_rejected_clearly() {
        let object = pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::Format,
            pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Mjpg),
            pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                Rectangle {
                    width: 1920,
                    height: 1080
                }
            ),
        );
        let bytes =
            PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
                .unwrap()
                .0
                .into_inner();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let error = parse_video_format(pod).unwrap_err();
        assert_eq!(error.code(), "pipewire.compressed_video");
        assert!(error.remediation().contains("compressed"));
    }

    #[test]
    fn unsupported_raw_layouts_are_rejected() {
        let object = pod::object!(
            SpaTypes::ObjectParamFormat,
            ParamType::Format,
            pod::property!(FormatProperties::MediaType, Id, MediaType::Video),
            pod::property!(FormatProperties::MediaSubtype, Id, MediaSubtype::Raw),
            pod::property!(FormatProperties::VideoFormat, Id, VideoFormat::NV21),
            pod::property!(
                FormatProperties::VideoSize,
                Rectangle,
                Rectangle {
                    width: 320,
                    height: 240
                }
            ),
        );
        let bytes =
            PodSerializer::serialize(std::io::Cursor::new(Vec::new()), &Value::Object(object))
                .unwrap()
                .0
                .into_inner();
        let pod = Pod::from_bytes(&bytes).unwrap();
        let error = parse_video_format(pod).unwrap_err();
        assert_eq!(error.code(), "pipewire.video_format_unsupported");
        assert!(error.remediation().contains("BGRA"));
    }

    #[test]
    fn stats_snapshot_includes_the_negotiated_format() {
        let counters = VideoCounters::default();
        counters.frames.store(3, Ordering::Relaxed);
        counters.dropped_frames.store(1, Ordering::Relaxed);
        counters.errors.store(2, Ordering::Relaxed);
        counters.set_format(NegotiatedVideoFormat {
            format: VideoPixelFormat::Nv12,
            width: 640,
            height: 480,
            fps_num: 30,
            fps_den: 1,
            stride: 640,
        });
        let stats = counters.snapshot();
        assert_eq!(stats.frames, 3);
        assert_eq!(stats.dropped_frames, 1);
        assert_eq!(stats.errors, 2);
        assert_eq!(stats.format, Some(VideoPixelFormat::Nv12));
        assert_eq!(stats.width, 640);
        assert_eq!(stats.height, 480);
        assert_eq!(stats.stride, 640);
    }

    #[test]
    fn video_frames_report_completeness() {
        let frame = VideoFrame {
            pts_micros: 10,
            sequence: 0,
            width: 4,
            height: 2,
            stride: 16,
            format: VideoPixelFormat::Bgra,
            data: vec![0; 32],
        };
        assert_eq!(frame.len(), 32);
        assert!(!frame.is_empty());
        assert!(frame.is_complete());
        let short = VideoFrame {
            data: vec![0; 31],
            ..frame
        };
        assert!(!short.is_complete());
    }
}
