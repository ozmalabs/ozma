// SPDX-License-Identifier: AGPL-3.0-only
//! V4L2 discovery and single-plane MMAP capture.
//!
//! Discovery reads `/dev` and sysfs only, so a node can be advertised without
//! opening it. Opening a source is the real thing: the provider asks the kernel
//! for capabilities, negotiates a supported pixel format, requests MMAP
//! buffers, and then hands the device to a dedicated capture worker. The worker
//! copies one dequeued buffer per [`CaptureSource::next_frame`] call into an
//! owned [`Frame`].
//!
//! The `v4l` crate keeps its mapped buffers behind a borrow of the device, so a
//! safe self-referential stream cannot be stored in the async source itself.
//! Instead the worker thread owns both `Device` and `Stream` for their whole
//! lifetime and communicates with the async side through channels. That keeps
//! `ozma-capture` free of `unsafe` code while still using real MMAP streaming.

use std::fmt;
use std::fs;
use std::os::unix::fs::FileTypeExt;
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};
use v4l::buffer::{Flags as BufferFlags, Metadata, Type};
use v4l::capability::Flags as CapabilityFlags;
use v4l::format::{Format, FourCC};
use v4l::framesize::{FrameSizeEnum, Stepwise};
use v4l::io::mmap::Stream;
use v4l::io::traits::CaptureStream;
use v4l::video::Capture;
use v4l::Device;

use crate::provider::CaptureProvider;

/// Largest resolution this source asks for by default.
///
/// The M1 target is 1080p30. A larger sensor mode is not useful to a client
/// that will transcode down, and requesting it costs memory bandwidth.
pub const DEFAULT_MAX_WIDTH: u32 = 1920;
/// Largest height this source asks for by default.
pub const DEFAULT_MAX_HEIGHT: u32 = 1080;

/// Resolution used when a device offers no usable frame-size enumeration.
pub const FALLBACK_WIDTH: u32 = 640;
/// Resolution used when a device offers no usable frame-size enumeration.
pub const FALLBACK_HEIGHT: u32 = 480;

/// How long a frame read may block before the source reports an error.
pub const DEFAULT_FRAME_TIMEOUT: Duration = Duration::from_secs(1);

/// Pixel formats in negotiation order: MJPEG, NV12, YUYV, then compressed
/// passthrough (H.264, HEVC, AV1). MJPEG is preferred because it keeps USB
/// bandwidth low; NV12 is the best raw layout for hardware encoders; YUYV is the
/// most compatible raw fallback. Compressed formats are last because they are
/// only useful when the HLS encoder matches; the snapshot path still needs a
/// decodable layout.
pub const PREFERRED_FOURCCS: [[u8; 4]; 6] =
    [*b"MJPG", *b"NV12", *b"YUYV", *b"H264", *b"HEVC", *b"AV01"];

/// Common resolutions considered for stepwise frame-size ranges.
const COMMON_RESOLUTIONS: [(u32, u32); 6] = [
    (1920, 1080),
    (1280, 720),
    (1024, 768),
    (800, 600),
    (640, 480),
    (320, 240),
];

/// Map a V4L2 four-character code onto the pipeline's byte layout.
///
/// `YUY2` is accepted as an alias for `YUYV` because a few drivers report it.
pub fn fourcc_to_pixel_format(fourcc: [u8; 4]) -> Option<PixelFormat> {
    match &fourcc {
        b"MJPG" => Some(PixelFormat::Mjpeg),
        b"NV12" => Some(PixelFormat::Nv12),
        b"YUYV" | b"YUY2" => Some(PixelFormat::Yuyv),
        b"H264" => Some(PixelFormat::H264),
        b"HEVC" => Some(PixelFormat::Hevc),
        b"AV01" => Some(PixelFormat::Av1),
        _ => None,
    }
}

/// Pick the first preferred format present on the device.
///
/// Only formats this crate can describe are ever returned, even when
/// `available` contains other fourccs.
pub fn preferred_fourcc(available: &[[u8; 4]]) -> Option<[u8; 4]> {
    PREFERRED_FOURCCS
        .iter()
        .find(|preferred| available.contains(*preferred))
        .copied()
}

/// Choose a capture resolution from the sizes a device advertises.
///
/// Prefers the largest mode that fits within [`DEFAULT_MAX_WIDTH`] by
/// [`DEFAULT_MAX_HEIGHT`]. If every mode is larger, the smallest mode is the
/// least costly alternative. Zero-sized entries are ignored.
pub fn choose_resolution(sizes: &[(u32, u32)]) -> Option<(u32, u32)> {
    let valid = sizes
        .iter()
        .copied()
        .filter(|(width, height)| *width > 0 && *height > 0);

    let fitting = valid
        .clone()
        .filter(|(width, height)| *width <= DEFAULT_MAX_WIDTH && *height <= DEFAULT_MAX_HEIGHT)
        .max_by_key(|&(width, height)| (u64::from(width) * u64::from(height), width, height));

    fitting.or_else(|| {
        valid.min_by_key(|&(width, height)| (u64::from(width) * u64::from(height), width, height))
    })
}

/// Compose a microsecond timestamp from a V4L2 `timeval`-shaped pair.
///
/// Returns `None` for negative or out-of-range components so the caller can
/// fall back to the local monotonic clock.
pub fn timestamp_micros(seconds: i64, micros: i64) -> Option<u64> {
    if seconds < 0 || micros < 0 || micros >= 1_000_000 {
        return None;
    }
    let seconds = u64::try_from(seconds).ok()?;
    let micros = u64::try_from(micros).ok()?;
    seconds.checked_mul(1_000_000)?.checked_add(micros)
}

/// Assign the next capture sequence number.
///
/// The first frame is sequence zero. The source keeps its own counter rather
/// than trusting the driver's `sequence` field, which can restart after a
/// stream toggle.
pub use crate::provider::{monotonic_pts, next_sequence};

/// Whether a captured buffer is independently decodable.
///
/// Every MJPEG buffer is a complete JPEG image, so it is always a keyframe.
/// H.264/HEVC/AV1 pass-through relies on the driver's `KEYFRAME` buffer flag
/// (and, for H.264, an Annex-B IDR scan when the flag is absent). Raw layouts
/// are not access units and never report a keyframe.
pub fn keyframe_for(format: PixelFormat, driver_keyframe: bool) -> bool {
    match format {
        PixelFormat::Mjpeg => true,
        PixelFormat::H264 | PixelFormat::Hevc | PixelFormat::Av1 => driver_keyframe,
        _ => false,
    }
}

/// Scan an H.264 Annex-B access unit for an IDR NAL unit (type 5).
///
/// Capture drivers normally set the V4L2 `KEYFRAME` flag, but passthrough
/// implementations vary. This is a conservative byte scan, not a full parser:
/// it only recognizes Annex-B start codes.
pub fn h264_access_unit_contains_idr(data: &[u8]) -> bool {
    let mut index = 0;
    while index + 3 < data.len() {
        let start_len = if data[index..].starts_with(&[0, 0, 1]) {
            3
        } else if data[index..].starts_with(&[0, 0, 0, 1]) {
            4
        } else {
            index += 1;
            continue;
        };
        let nal_index = index + start_len;
        if nal_index < data.len() && (data[nal_index] & 0x1f) == 5 {
            return true;
        }
        index = nal_index;
    }
    false
}

/// A discovered V4L2 video node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct V4l2Node {
    /// Numeric suffix of `/dev/videoN`.
    pub index: u32,
    /// Stable id, normally the device path.
    pub id: String,
    /// Path to the device node.
    pub device_path: PathBuf,
    /// Human-readable name from sysfs, or a generated fallback.
    pub name: String,
    /// Whether the name looks like a capture node rather than a metadata or
    /// output-only node.
    pub capture_like: bool,
}

impl V4l2Node {
    /// Protocol capability for this node.
    ///
    /// Width, height, FPS, and format list are left at zero/empty because
    /// sysfs does not expose them. Consumers must treat zero as "unknown
    /// until opened", not as a limit.
    pub fn capability(&self) -> CaptureCapability {
        CaptureCapability {
            id: self.id.clone(),
            name: self.name.clone(),
            kind: CaptureKind::V4l2,
            formats: Vec::new(),
            max_width: 0,
            max_height: 0,
            max_fps: 0,
            compressed: false,
            hdr: false,
            has_audio: false,
        }
    }
}

impl fmt::Display for V4l2Node {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{} ({})", self.id, self.name)
    }
}

/// Parse the numeric index from a `videoN` filename.
pub fn parse_video_index(name: &str) -> Option<u32> {
    let digits = name.strip_prefix("video")?;
    if digits.is_empty() || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok()
}

/// Heuristic for whether a sysfs name describes a capture node.
///
/// Without `VIDIOC_QUERYCAP` this cannot be exact. It intentionally excludes
/// names that clearly say "metadata" or "output" and includes the names used by
/// UVC cameras, HDMI capture dongles, and virtual video devices. Discovery
/// keeps the raw node list available through [`discover_v4l2`], so callers that
/// need every node are not limited to this filter.
pub fn is_capture_like_name(name: &str) -> bool {
    let name = name.to_ascii_lowercase();
    if name.contains("metadata") || name.contains("output") {
        return false;
    }
    [
        "capture",
        "camera",
        "webcam",
        "cam",
        "hdmi",
        "uvc",
        "grabber",
        "elgato",
        "avermedia",
        "video",
        "usb",
    ]
    .iter()
    .any(|keyword| name.contains(keyword))
}

fn api_io_error(path: &Path, error: std::io::Error) -> ApiError {
    ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!(
            "Failed to inspect capture metadata at {}: {error}",
            path.display()
        ),
    )
    .with_details(path.display().to_string())
    .with_remediation("Check device permissions and that udev has created the video node.")
}

/// Enumerate `/dev/video*` nodes and read their sysfs names.
///
/// `dev_root` is normally `/dev` and `sys_root` is normally
/// `/sys/class/video4linux`; tests inject temporary directories. A missing
/// directory yields an empty list, which is the correct result on a host with
/// no V4L2 subsystem. Other filesystem errors are returned.
pub fn discover_v4l2(dev_root: &Path, sys_root: &Path) -> Result<Vec<V4l2Node>, ApiError> {
    let entries = match fs::read_dir(dev_root) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(error) => return Err(api_io_error(dev_root, error)),
    };

    let mut nodes = Vec::new();
    for entry in entries {
        let entry = entry.map_err(|error| api_io_error(dev_root, error))?;
        let file_name = entry.file_name();
        let file_name = file_name.to_string_lossy();
        let Some(index) = parse_video_index(&file_name) else {
            continue;
        };

        let device_path = entry.path();
        match fs::metadata(&device_path) {
            Ok(metadata) if metadata.is_dir() => continue,
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(api_io_error(&device_path, error)),
        }

        let sys_name_path = sys_root.join(format!("video{index}")).join("name");
        let name = fs::read_to_string(&sys_name_path)
            .ok()
            .map(|name| name.trim().to_owned())
            .filter(|name| !name.is_empty())
            .unwrap_or_else(|| format!("V4L2 video{index}"));
        let capture_like = is_capture_like_name(&name);

        nodes.push(V4l2Node {
            index,
            id: device_path.to_string_lossy().into_owned(),
            device_path,
            name,
            capture_like,
        });
    }

    nodes.sort_by_key(|node| node.index);
    Ok(nodes)
}

/// Safe V4L2 discovery and capture provider.
#[derive(Clone, Debug)]
pub struct V4l2Provider {
    dev_root: PathBuf,
    sys_root: PathBuf,
}

impl Default for V4l2Provider {
    fn default() -> Self {
        Self::new()
    }
}

impl V4l2Provider {
    /// Provider rooted at the host's `/dev` and `/sys/class/video4linux`.
    pub fn new() -> Self {
        Self {
            dev_root: PathBuf::from("/dev"),
            sys_root: PathBuf::from("/sys/class/video4linux"),
        }
    }

    /// Provider with injected roots, for tests and unusual containers.
    pub fn with_roots(dev_root: impl Into<PathBuf>, sys_root: impl Into<PathBuf>) -> Self {
        Self {
            dev_root: dev_root.into(),
            sys_root: sys_root.into(),
        }
    }

    /// Discover every video node, including metadata/output nodes.
    pub fn discover(&self) -> Result<Vec<V4l2Node>, ApiError> {
        discover_v4l2(&self.dev_root, &self.sys_root)
    }
}

impl CaptureProvider for V4l2Provider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::V4l2
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(self
            .discover()?
            .into_iter()
            .filter(|node| node.capture_like)
            .map(|node| node.capability())
            .collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::V4l2 {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                "The V4L2 provider cannot open a non-V4L2 capability.",
            ));
        }
        let node = self
            .discover()?
            .into_iter()
            .find(|node| node.id == capability.id)
            .ok_or_else(|| {
                ApiError::new(
                    codes::NOT_FOUND,
                    format!("V4L2 device `{}` is no longer present.", capability.id),
                )
                .with_details(capability.id.clone())
                .with_remediation("Replug the capture device and refresh capture discovery.")
            })?;
        Ok(Box::new(V4l2Capture::open(node)?))
    }
}

/// Commands sent from the async source to its blocking capture worker.
enum Command {
    NextFrame,
    Stop,
}

/// A live single-plane V4L2 MMAP capture source.
///
/// Construction is synchronous and performs capability checks, format
/// negotiation, and MMAP buffer allocation. The `v4l` API queues buffers and
/// issues `VIDIOC_STREAMON` on the first stream read, so streaming begins with
/// the first [`CaptureSource::next_frame`]; that call also returns the first
/// frame. Frames are then captured by a dedicated worker thread, one buffered
/// request at a time. Dropping the source closes the command channel, which
/// makes the worker exit and release its stream and device.
pub struct V4l2Capture {
    node: V4l2Node,
    format: FrameFormat,
    fourcc: FourCC,
    requests: Option<tokio::sync::mpsc::Sender<Command>>,
    frames: tokio::sync::mpsc::Receiver<Result<Frame, MediaError>>,
    worker: Option<std::thread::JoinHandle<()>>,
    stopped: bool,
}

impl fmt::Debug for V4l2Capture {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("V4l2Capture")
            .field("node", &self.node)
            .field("format", &self.format)
            .field("stopped", &self.stopped)
            .finish()
    }
}

impl V4l2Capture {
    /// Open and negotiate a real capture source.
    ///
    /// Fails with a structured [`ApiError`] when the path is missing, is not a
    /// character device, does not expose single-plane streaming capture, has no
    /// supported pixel format, or cannot allocate MMAP buffers. Streaming is
    /// started lazily by the first [`CaptureSource::next_frame`] call, after
    /// this method has confirmed that buffers and a stream are usable.
    pub fn open(node: V4l2Node) -> Result<Self, ApiError> {
        let id = node.id.clone();

        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);
        let (request_tx, request_rx) = tokio::sync::mpsc::channel(1);
        let (frame_tx, frame_rx) = tokio::sync::mpsc::channel(1);

        let worker_node = node.clone();
        let worker = std::thread::Builder::new()
            .name(format!("ozma-v4l2-{}", node.index))
            .spawn(move || capture_worker(worker_node, ready_tx, request_rx, frame_tx))
            .map_err(|error| {
                ApiError::new(
                    codes::CAPTURE_UNAVAILABLE,
                    format!("Failed to start the V4L2 capture worker for `{id}`: {error}"),
                )
                .with_details(id.clone())
                .with_remediation("Check process/thread limits and retry.")
            })?;

        match ready_rx.recv_timeout(Duration::from_secs(10)) {
            Ok(Ok((format, fourcc))) => Ok(Self {
                node,
                format,
                fourcc,
                requests: Some(request_tx),
                frames: frame_rx,
                worker: Some(worker),
                stopped: false,
            }),
            Ok(Err(error)) => {
                drop(request_tx);
                let _ = worker.join();
                Err(error)
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                // Dropping the request sender makes the worker exit as soon as
                // its setup call returns. Do not join: the point of the
                // timeout is not to block the caller on a wedged driver.
                drop(request_tx);
                Err(ApiError::new(
                    codes::CAPTURE_UNAVAILABLE,
                    format!("Timed out while opening V4L2 device `{id}`."),
                )
                .with_details(id)
                .with_remediation(
                    "Check that another process is not streaming from the device and that the driver is responsive.",
                )
                .retryable())
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = worker.join();
                Err(ApiError::new(
                    codes::CAPTURE_UNAVAILABLE,
                    format!("The V4L2 capture worker for `{id}` exited during setup."),
                )
                .with_details(id)
                .with_remediation("Check dmesg for a driver or USB reset, then retry."))
            }
        }
    }

    /// The discovered node behind this source.
    pub fn node(&self) -> &V4l2Node {
        &self.node
    }

    /// Device path, for diagnostics.
    pub fn device_path(&self) -> &Path {
        &self.node.device_path
    }

    /// Exact negotiated V4L2 fourcc.
    pub fn fourcc(&self) -> FourCC {
        self.fourcc
    }
}

impl Drop for V4l2Capture {
    fn drop(&mut self) {
        // `stop` performs the deterministic, joined shutdown. On a bare drop,
        // still close the channels so a worker blocked on a command exits and
        // one blocked on a frame exits as soon as its timeout expires. Drop
        // must not block, so the worker handle is detached rather than joined.
        self.frames.close();
        if let Some(requests) = self.requests.take() {
            let _ = requests.try_send(Command::Stop);
        }
    }
}

#[async_trait]
impl CaptureSource for V4l2Capture {
    fn id(&self) -> &str {
        &self.node.id
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        if self.stopped {
            return Err(MediaError::Closed {
                id: self.node.id.clone(),
            });
        }

        // A previous `next_frame` future may have been cancelled after the
        // worker produced a frame. Return that buffered frame instead of
        // requesting another one.
        if let Ok(frame) = self.frames.try_recv() {
            return frame;
        }

        let Some(requests) = &self.requests else {
            return Err(MediaError::Closed {
                id: self.node.id.clone(),
            });
        };
        if requests.send(Command::NextFrame).await.is_err() {
            self.requests = None;
            return Err(MediaError::capture(
                &self.node.id,
                "capture worker stopped unexpectedly",
            ));
        }

        match self.frames.recv().await {
            Some(frame) => frame,
            None => {
                self.requests = None;
                Err(MediaError::capture(
                    &self.node.id,
                    "capture worker stopped unexpectedly",
                ))
            }
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        if self.stopped {
            return Ok(());
        }
        self.stopped = true;

        // Closing first also unblocks a worker that is waiting to deliver a
        // frame. Dropping the command sender after a best-effort stop makes the
        // worker exit even if a queued `NextFrame` still occupies the channel.
        self.frames.close();
        if let Some(requests) = self.requests.take() {
            let _ = requests.try_send(Command::Stop);
            drop(requests);
        }

        // Joining the worker guarantees its `Stream` has issued STREAMOFF,
        // unmapped the MMAP buffers, released them with REQBUFS(0), and closed
        // the device file descriptor before `stop` returns.
        if let Some(worker) = self.worker.take() {
            match tokio::task::spawn_blocking(move || worker.join()).await {
                Ok(Ok(())) => {}
                Ok(Err(_)) => {
                    return Err(MediaError::capture(
                        &self.node.id,
                        "capture worker panicked while stopping",
                    ))
                }
                Err(error) => {
                    return Err(MediaError::capture(
                        &self.node.id,
                        format!("could not join capture worker: {error}"),
                    ))
                }
            }
        }

        Ok(())
    }
}

/// Owns the blocking V4L2 device and stream for the lifetime of a source.
fn capture_worker(
    node: V4l2Node,
    ready: std_mpsc::SyncSender<Result<(FrameFormat, FourCC), ApiError>>,
    mut commands: tokio::sync::mpsc::Receiver<Command>,
    responses: tokio::sync::mpsc::Sender<Result<Frame, MediaError>>,
) {
    let (device, negotiated, pixel_format) = match open_device(&node) {
        Ok(opened) => opened,
        Err(error) => {
            let _ = ready.send(Err(error));
            return;
        }
    };

    let format = FrameFormat::new(pixel_format, negotiated.width, negotiated.height);
    let fourcc = negotiated.fourcc;
    let mut stream = match Stream::with_buffers(&device, Type::VideoCapture, 4) {
        Ok(stream) => stream,
        Err(error) => {
            let _ = ready.send(Err(worker_io_error(
                &node,
                "request MMAP capture buffers for",
                error,
            )));
            return;
        }
    };
    stream.set_timeout(DEFAULT_FRAME_TIMEOUT);

    if ready.send(Ok((format, fourcc))).is_err() {
        // The caller went away during setup.
        return;
    }

    let origin = Instant::now();
    let mut sequence = 0u64;
    let mut last_pts = None;

    while let Some(command) = commands.blocking_recv() {
        match command {
            Command::Stop => break,
            Command::NextFrame => {
                let outcome = capture_one(
                    &mut stream,
                    &node.id,
                    format,
                    pixel_format,
                    sequence,
                    last_pts,
                    origin,
                );
                match outcome {
                    Ok((frame, pts)) => {
                        sequence = next_sequence(Some(sequence));
                        last_pts = Some(pts);
                        if responses.blocking_send(Ok(frame)).is_err() {
                            break;
                        }
                    }
                    Err(error) => {
                        // A broken stream cannot safely be restarted: the v4l
                        // crate may still have buffers queued. Report the
                        // failure and let the owner re-open the source.
                        let _ = responses.blocking_send(Err(error));
                        break;
                    }
                }
            }
        }
    }

    // Drop order matters: the stream unmaps buffers and stops streaming, then
    // the device's handle closes the file descriptor.
    drop(stream);
    drop(device);
}

/// Open a node, verify its capabilities, and negotiate an exact format.
fn open_device(node: &V4l2Node) -> Result<(Device, Format, PixelFormat), ApiError> {
    let metadata = fs::metadata(&node.device_path).map_err(|error| {
        if error.kind() == std::io::ErrorKind::NotFound {
            missing_node_error(node)
        } else {
            worker_io_error(node, "inspect", error)
        }
    })?;
    if !metadata.file_type().is_char_device() {
        return Err(ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            format!(
                "V4L2 path `{}` is not a character device.",
                node.device_path.display()
            ),
        )
        .with_details(node.id.clone())
        .with_remediation("Use a /dev/videoN node created by the kernel, not a regular file.")
        .retryable());
    }

    let device = Device::with_path(&node.device_path)
        .map_err(|error| worker_io_error(node, "open", error))?;

    let capabilities = device
        .query_caps()
        .map_err(|error| worker_io_error(node, "query capabilities for", error))?;

    if !capabilities
        .capabilities
        .contains(CapabilityFlags::VIDEO_CAPTURE)
    {
        let message = if capabilities
            .capabilities
            .contains(CapabilityFlags::VIDEO_CAPTURE_MPLANE)
        {
            format!(
                "V4L2 device `{}` is multi-planar capture only; this source implements single-plane MMAP.",
                node.id
            )
        } else {
            format!(
                "V4L2 device `{}` does not expose video capture (driver `{}`, card `{}`).",
                node.id, capabilities.driver, capabilities.card
            )
        };
        return Err(ApiError::new(codes::CAPTURE_UNAVAILABLE, message)
            .with_details(node.id.clone())
            .with_remediation(
                "Pick a capture node such as a camera or HDMI grabber, not a metadata or output node.",
            ));
    }

    if !capabilities
        .capabilities
        .contains(CapabilityFlags::STREAMING)
    {
        return Err(ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            format!("V4L2 device `{}` does not support streaming I/O.", node.id),
        )
        .with_details(node.id.clone())
        .with_remediation("This source requires a driver with MMAP streaming support."));
    }

    let fourcc = select_fourcc(&device, node)?;
    let sizes = frame_size_candidates(&device, fourcc);
    let (width, height) = choose_resolution(&sizes).unwrap_or((FALLBACK_WIDTH, FALLBACK_HEIGHT));

    // Ask for the chosen mode, but keep exactly what the driver reports. A
    // driver is allowed to adjust geometry; it must not silently change to a
    // layout we cannot describe.
    let requested = Format::new(width, height, FourCC::new(&fourcc));
    let negotiated = device
        .set_format(&requested)
        .map_err(|error| worker_io_error(node, "set capture format for", error))?;

    let Some(pixel_format) = fourcc_to_pixel_format(negotiated.fourcc.repr) else {
        return Err(ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            format!(
                "V4L2 device `{}` negotiated unsupported pixel format `{}`.",
                node.id, negotiated.fourcc
            ),
        )
        .with_details(node.id.clone())
        .with_remediation(
            "Try a device that advertises MJPEG, NV12, YUYV, H.264, HEVC, or AV1 capture.",
        ));
    };

    if negotiated.width == 0 || negotiated.height == 0 {
        return Err(ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            format!(
                "V4L2 device `{}` negotiated an empty {}x{} frame.",
                node.id, negotiated.width, negotiated.height
            ),
        )
        .with_details(node.id.clone())
        .with_remediation("Check the driver's frame-size enumeration and retry."));
    }

    Ok((device, negotiated, pixel_format))
}

fn select_fourcc(device: &Device, node: &V4l2Node) -> Result<[u8; 4], ApiError> {
    let available: Vec<[u8; 4]> = device
        .enum_formats()
        .unwrap_or_default()
        .into_iter()
        .map(|format| format.fourcc.repr)
        .collect();

    if let Some(fourcc) = preferred_fourcc(&available) {
        return Ok(fourcc);
    }

    // Enumeration can fail on some virtual drivers even though the current
    // format is perfectly usable. Accept it when it is a layout we understand.
    if let Ok(current) = device.format() {
        if fourcc_to_pixel_format(current.fourcc.repr).is_some() {
            return Ok(current.fourcc.repr);
        }
    }

    let listed = if available.is_empty() {
        "none advertised".to_owned()
    } else {
        available
            .iter()
            .map(|fourcc| String::from_utf8_lossy(fourcc).into_owned())
            .collect::<Vec<_>>()
            .join(", ")
    };
    Err(ApiError::new(
        codes::CAPTURE_UNAVAILABLE,
        format!(
            "V4L2 device `{}` has no supported pixel format ({listed}).",
            node.id
        ),
    )
    .with_details(node.id.clone())
    .with_remediation("A supported device advertises MJPEG, NV12, YUYV, H.264, HEVC, or AV1."))
}

fn frame_size_candidates(device: &Device, fourcc: [u8; 4]) -> Vec<(u32, u32)> {
    match device.enum_framesizes(FourCC::new(&fourcc)) {
        Ok(sizes) => expand_frames(sizes),
        Err(_) => match device.format() {
            Ok(current)
                if current.fourcc.repr == fourcc && current.width > 0 && current.height > 0 =>
            {
                vec![(current.width, current.height)]
            }
            _ => Vec::new(),
        },
    }
}

/// Expand enumerated frame sizes without materialising every stepwise mode.
fn expand_frames(sizes: Vec<v4l::FrameSize>) -> Vec<(u32, u32)> {
    let mut expanded = Vec::new();
    for size in sizes {
        match size.size {
            FrameSizeEnum::Discrete(discrete) => {
                expanded.push((discrete.width, discrete.height));
            }
            FrameSizeEnum::Stepwise(stepwise) => {
                for (width, height) in COMMON_RESOLUTIONS {
                    if stepwise_contains(&stepwise, width, height) {
                        expanded.push((width, height));
                    }
                }
                expanded.push((stepwise.min_width, stepwise.min_height));
                expanded.push((stepwise.max_width, stepwise.max_height));
            }
        }
    }
    expanded
}

fn stepwise_contains(stepwise: &Stepwise, width: u32, height: u32) -> bool {
    fn aligned(value: u32, min: u32, max: u32, step: u32) -> bool {
        value >= min && value <= max && (step == 0 || (value - min) % step == 0)
    }
    aligned(
        width,
        stepwise.min_width,
        stepwise.max_width,
        stepwise.step_width,
    ) && aligned(
        height,
        stepwise.min_height,
        stepwise.max_height,
        stepwise.step_height,
    )
}

fn capture_one(
    stream: &mut Stream<'_>,
    id: &str,
    format: FrameFormat,
    pixel_format: PixelFormat,
    sequence: u64,
    last_pts: Option<u64>,
    origin: Instant,
) -> Result<(Frame, u64), MediaError> {
    let (buffer, metadata) =
        CaptureStream::next(stream).map_err(|error| capture_io_error(id, error))?;

    let used = usize::try_from(metadata.bytesused)
        .map_err(|_| MediaError::capture(id, "driver reported an invalid buffer length"))?;
    if used == 0 {
        return Err(MediaError::EmptyFrame);
    }
    if used > buffer.len() {
        return Err(MediaError::capture(
            id,
            format!(
                "driver reported {used} used bytes in a {}-byte buffer",
                buffer.len()
            ),
        ));
    }
    let payload = &buffer[..used];

    let pts = monotonic_pts(pts_from_metadata(&metadata, origin), last_pts);
    let driver_keyframe = metadata.flags.contains(BufferFlags::KEYFRAME);
    let keyframe = keyframe_for(pixel_format, driver_keyframe)
        || (pixel_format == PixelFormat::H264 && h264_access_unit_contains_idr(payload));

    Frame::with_data(format, pts, sequence, keyframe, payload.to_vec()).map(|frame| (frame, pts))
}

fn pts_from_metadata(metadata: &Metadata, origin: Instant) -> u64 {
    if metadata.flags.contains(BufferFlags::TIMESTAMP_MONOTONIC) {
        if let Some(pts) = timestamp_micros(
            metadata.timestamp.sec as i64,
            metadata.timestamp.usec as i64,
        ) {
            return pts;
        }
    }
    // Older or unusual drivers leave the timestamp type unset. A local
    // monotonic clock still gives the pipeline a usable, non-decreasing PTS.
    elapsed_micros(origin)
}

fn elapsed_micros(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn capture_io_error(id: &str, error: std::io::Error) -> MediaError {
    let message = if error.kind() == std::io::ErrorKind::TimedOut {
        format!(
            "timed out after {} ms waiting for a frame",
            DEFAULT_FRAME_TIMEOUT.as_millis()
        )
    } else {
        format!("frame capture failed: {error}")
    };
    MediaError::capture(id, message)
}

fn worker_io_error(node: &V4l2Node, action: &str, error: std::io::Error) -> ApiError {
    let not_found = error.kind() == std::io::ErrorKind::NotFound;
    let code = if not_found {
        codes::NOT_FOUND
    } else {
        codes::CAPTURE_UNAVAILABLE
    };
    let api = ApiError::new(
        code,
        format!("Failed to {action} V4L2 device `{}`: {error}", node.id),
    )
    .with_details(node.id.clone())
    .with_remediation(
        "Check the device is connected, the node is a character device, and the service user can open it.",
    );
    if not_found {
        api
    } else {
        api.retryable()
    }
}

fn missing_node_error(node: &V4l2Node) -> ApiError {
    ApiError::new(
        codes::NOT_FOUND,
        format!("V4L2 device `{}` no longer exists.", node.id),
    )
    .with_details(node.id.clone())
    .with_remediation("Replug the capture device and refresh capture discovery.")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};

    struct TempTree(PathBuf);

    impl TempTree {
        fn new(tag: &str) -> Self {
            static COUNTER: AtomicU64 = AtomicU64::new(0);
            let unique = COUNTER.fetch_add(1, Ordering::Relaxed);
            let root = std::env::temp_dir().join(format!(
                "ozma-capture-{}-{tag}-{unique}",
                std::process::id()
            ));
            let _ = fs::remove_dir_all(&root);
            fs::create_dir_all(&root).unwrap();
            Self(root)
        }

        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempTree {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    fn fixture() -> (TempTree, PathBuf, PathBuf) {
        let tree = TempTree::new("discovery");
        let dev = tree.path().join("dev");
        let sys = tree.path().join("sys/class/video4linux");
        fs::create_dir_all(&dev).unwrap();
        fs::create_dir_all(&sys).unwrap();

        for (index, name) in [
            (10, "USB Capture HDMI"),
            (2, "Integrated Camera"),
            (0, "UVC Camera"),
            (3, "UVC metadata"),
        ] {
            fs::write(dev.join(format!("video{index}")), b"").unwrap();
            let node = sys.join(format!("video{index}"));
            fs::create_dir_all(&node).unwrap();
            fs::write(node.join("name"), format!("{name}\n")).unwrap();
        }

        // Files that must not be mistaken for video nodes.
        fs::write(dev.join("videoX"), b"").unwrap();
        fs::write(dev.join("notvideo1"), b"").unwrap();
        (tree, dev, sys)
    }

    #[test]
    fn parses_only_numeric_video_names() {
        assert_eq!(parse_video_index("video0"), Some(0));
        assert_eq!(parse_video_index("video12"), Some(12));
        assert_eq!(parse_video_index("video"), None);
        assert_eq!(parse_video_index("videoX"), None);
        assert_eq!(parse_video_index("video12x"), None);
        assert_eq!(parse_video_index("camera0"), None);
    }

    #[test]
    fn capture_heuristic_excludes_metadata_and_output() {
        assert!(is_capture_like_name("USB Capture HDMI"));
        assert!(is_capture_like_name("Integrated Camera"));
        assert!(is_capture_like_name("UVC Camera"));
        // Many UVC cards name themselves only "... Cam ..." without the word
        // "camera" or "webcam".
        assert!(is_capture_like_name("Creative Live! Cam Sync 4K"));
        assert!(!is_capture_like_name("UVC metadata"));
        assert!(!is_capture_like_name("Video Output"));
        assert!(!is_capture_like_name(""));
    }

    #[test]
    fn discovery_reads_names_and_sorts_numerically() {
        let (_tree, dev, sys) = fixture();
        let nodes = discover_v4l2(&dev, &sys).unwrap();
        let indexes: Vec<u32> = nodes.iter().map(|node| node.index).collect();
        assert_eq!(indexes, vec![0, 2, 3, 10]);
        assert_eq!(nodes[0].name, "UVC Camera");
        assert_eq!(nodes[3].name, "USB Capture HDMI");
        assert!(nodes[0].capture_like);
        assert!(!nodes[2].capture_like);
        assert_eq!(
            nodes[0].id,
            dev.join("video0").to_string_lossy().to_string()
        );
    }

    #[test]
    fn provider_enumerates_capture_nodes_only() {
        let (_tree, dev, sys) = fixture();
        let provider = V4l2Provider::with_roots(dev, sys);
        let capabilities = provider.enumerate().unwrap();
        let ids: Vec<String> = capabilities
            .iter()
            .map(|capability| capability.id.clone())
            .collect();
        assert_eq!(ids.len(), 3, "metadata node must be excluded");
        assert!(ids[0].ends_with("video0"));
        assert!(ids[1].ends_with("video2"));
        assert!(ids[2].ends_with("video10"));
        assert_eq!(capabilities[0].kind, CaptureKind::V4l2);
        assert_eq!(capabilities[0].max_width, 0, "unknown until opened");
    }

    #[test]
    fn missing_directories_yield_no_sources() {
        let tree = TempTree::new("empty");
        let nodes = discover_v4l2(&tree.path().join("dev"), &tree.path().join("sys")).unwrap();
        assert!(nodes.is_empty());
    }

    #[test]
    fn fallback_name_is_used_when_sysfs_is_missing() {
        let tree = TempTree::new("noname");
        let dev = tree.path().join("dev");
        fs::create_dir_all(&dev).unwrap();
        fs::write(dev.join("video7"), b"").unwrap();
        let nodes = discover_v4l2(&dev, &tree.path().join("sys")).unwrap();
        assert_eq!(nodes.len(), 1);
        assert_eq!(nodes[0].name, "V4L2 video7");
    }

    #[test]
    fn fourcc_mapping_covers_negotiable_layouts() {
        assert_eq!(fourcc_to_pixel_format(*b"MJPG"), Some(PixelFormat::Mjpeg));
        assert_eq!(fourcc_to_pixel_format(*b"NV12"), Some(PixelFormat::Nv12));
        assert_eq!(fourcc_to_pixel_format(*b"YUYV"), Some(PixelFormat::Yuyv));
        assert_eq!(fourcc_to_pixel_format(*b"YUY2"), Some(PixelFormat::Yuyv));
        assert_eq!(fourcc_to_pixel_format(*b"H264"), Some(PixelFormat::H264));
        assert_eq!(fourcc_to_pixel_format(*b"HEVC"), Some(PixelFormat::Hevc));
        assert_eq!(fourcc_to_pixel_format(*b"AV01"), Some(PixelFormat::Av1));
        assert_eq!(fourcc_to_pixel_format(*b"RGB3"), None);
        assert_eq!(fourcc_to_pixel_format([0, 0, 0, 0]), None);
    }

    #[test]
    fn format_preference_is_mjpeg_then_nv12_then_yuyv() {
        assert_eq!(
            preferred_fourcc(&[*b"YUYV", *b"NV12", *b"MJPG"]),
            Some(*b"MJPG")
        );
        assert_eq!(preferred_fourcc(&[*b"YUYV", *b"NV12"]), Some(*b"NV12"));
        assert_eq!(preferred_fourcc(&[*b"YUYV"]), Some(*b"YUYV"));
        assert_eq!(preferred_fourcc(&[*b"H264", *b"RGB3"]), Some(*b"H264"));
        assert_eq!(
            preferred_fourcc(&[*b"HEVC", *b"AV01"]),
            Some(*b"HEVC"),
            "HEVC is preferred over AV1"
        );
        assert_eq!(preferred_fourcc(&[*b"AV01", *b"RGB3"]), Some(*b"AV01"));
        assert_eq!(preferred_fourcc(&[*b"RGB3"]), None);
        assert_eq!(preferred_fourcc(&[]), None);
    }

    #[test]
    fn resolution_stays_within_the_default_budget() {
        let sizes = [(320, 240), (3840, 2160), (1280, 720), (1920, 1080)];
        assert_eq!(choose_resolution(&sizes), Some((1920, 1080)));
        assert_eq!(choose_resolution(&[(0, 0), (640, 480)]), Some((640, 480)));
        assert_eq!(choose_resolution(&[]), None);
    }

    #[test]
    fn oversized_modes_fall_back_to_the_smallest() {
        assert_eq!(
            choose_resolution(&[(3840, 2160), (2560, 1440)]),
            Some((2560, 1440))
        );
    }

    #[test]
    fn sequences_start_at_zero_and_advance() {
        assert_eq!(next_sequence(None), 0);
        assert_eq!(next_sequence(Some(0)), 1);
        assert_eq!(next_sequence(Some(41)), 42);
        assert_eq!(next_sequence(Some(u64::MAX)), u64::MAX);
    }

    #[test]
    fn pts_never_goes_backwards() {
        assert_eq!(monotonic_pts(10, None), 10);
        assert_eq!(monotonic_pts(10, Some(10)), 11);
        assert_eq!(monotonic_pts(9, Some(10)), 11);
        assert_eq!(monotonic_pts(15, Some(10)), 15);
        assert_eq!(monotonic_pts(15, Some(u64::MAX)), u64::MAX);
    }

    #[test]
    fn timestamps_reject_invalid_components() {
        assert_eq!(timestamp_micros(2, 500_000), Some(2_500_000));
        assert_eq!(timestamp_micros(0, 0), Some(0));
        assert_eq!(timestamp_micros(-1, 0), None);
        assert_eq!(timestamp_micros(0, -1), None);
        assert_eq!(timestamp_micros(0, 1_000_000), None);
    }

    #[test]
    fn keyframe_semantics_are_explicit() {
        assert!(keyframe_for(PixelFormat::Mjpeg, false));
        assert!(keyframe_for(PixelFormat::Mjpeg, true));
        assert!(keyframe_for(PixelFormat::H264, true));
        assert!(!keyframe_for(PixelFormat::H264, false));
        assert!(!keyframe_for(PixelFormat::Nv12, true));
        assert!(!keyframe_for(PixelFormat::Yuyv, true));
    }

    #[test]
    fn h264_idr_scan_finds_annex_b_idr_only() {
        assert!(!h264_access_unit_contains_idr(&[]));
        assert!(!h264_access_unit_contains_idr(&[0, 0, 1, 0x41, 0x88]));
        // 0x65 & 0x1f == 5 (IDR)
        assert!(h264_access_unit_contains_idr(&[0, 0, 1, 0x65, 0x88]));
        assert!(h264_access_unit_contains_idr(&[0, 0, 0, 1, 0x65]));
        // SPS then IDR
        assert!(h264_access_unit_contains_idr(&[
            0, 0, 1, 0x67, 1, 2, 0, 0, 1, 0x65,
        ]));
    }

    #[test]
    fn open_rejects_unknown_devices() {
        let (_tree, dev, sys) = fixture();
        let provider = V4l2Provider::with_roots(dev, sys);
        let capability = CaptureCapability {
            id: "/dev/video99".to_owned(),
            name: "Missing".to_owned(),
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
        assert!(error.remediation.is_some());
    }

    #[test]
    fn open_reports_a_missing_device_path_as_structured_not_found() {
        let tree = TempTree::new("missing");
        let node = V4l2Node {
            index: 99,
            id: tree
                .path()
                .join("dev/video99")
                .to_string_lossy()
                .into_owned(),
            device_path: tree.path().join("dev/video99"),
            name: "Missing".to_owned(),
            capture_like: true,
        };
        let error = V4l2Capture::open(node).err().unwrap();
        assert_eq!(error.code, codes::NOT_FOUND);
        assert!(error.details.is_string());
        assert!(error.remediation.is_some());
    }

    #[test]
    fn open_rejects_a_character_device_that_is_not_v4l2() {
        let path = PathBuf::from("/dev/null");
        if !path.exists() {
            return;
        }
        let node = V4l2Node {
            index: 0,
            id: path.to_string_lossy().into_owned(),
            device_path: path,
            name: "not a camera".to_owned(),
            capture_like: false,
        };
        let error = V4l2Capture::open(node).err().unwrap();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.remediation.is_some());
    }

    #[tokio::test]
    async fn open_rejects_a_regular_file_instead_of_pretending_to_stream() {
        let (_tree, dev, sys) = fixture();
        let provider = V4l2Provider::with_roots(dev, sys);
        let capability = provider.enumerate().unwrap().remove(0);
        let error = provider.open(&capability).err().unwrap();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("character device"));
        assert!(error.remediation.is_some());
    }

    #[test]
    fn stepwise_ranges_include_common_modes() {
        let stepwise = Stepwise {
            min_width: 320,
            max_width: 1920,
            step_width: 2,
            min_height: 240,
            max_height: 1080,
            step_height: 2,
        };
        assert!(stepwise_contains(&stepwise, 1280, 720));
        assert!(stepwise_contains(&stepwise, 1920, 1080));
        assert!(!stepwise_contains(&stepwise, 3840, 2160));
        assert!(!stepwise_contains(&stepwise, 641, 480));
    }

    /// Opt-in hardware test.
    ///
    /// Run with a real capture node:
    ///
    /// ```text
    /// OZMA_CAPTURE_TEST_DEVICE=/dev/video2 \
    ///     cargo test -p ozma-capture -- --ignored real_v4l2_capture
    /// ```
    ///
    /// Without the environment variable the test uses the first capture-like
    /// node found by discovery, and skips cleanly when the host has none.
    #[tokio::test]
    #[ignore = "requires a real V4L2 capture device; set OZMA_CAPTURE_TEST_DEVICE or run on hardware"]
    async fn real_v4l2_capture_yields_monotonic_frames() {
        let provider = V4l2Provider::new();
        let env_path = std::env::var_os("OZMA_CAPTURE_TEST_DEVICE").map(PathBuf::from);

        let mut source: Box<dyn CaptureSource> = if let Some(path) = env_path {
            let index = path
                .file_name()
                .and_then(|name| parse_video_index(&name.to_string_lossy()))
                .unwrap_or(0);
            let node = V4l2Node {
                index,
                id: path.to_string_lossy().into_owned(),
                device_path: path,
                name: "OZMA_CAPTURE_TEST_DEVICE".to_owned(),
                capture_like: true,
            };
            Box::new(V4l2Capture::open(node).expect("open configured V4L2 device"))
        } else {
            // UVC cards often expose a metadata node under the same sysfs name
            // as the capture node. Try each advertised node until one opens.
            let mut opened = None;
            for capability in provider.enumerate().expect("V4L2 discovery") {
                match provider.open(&capability) {
                    Ok(source) => {
                        opened = Some(source);
                        break;
                    }
                    Err(error) => {
                        eprintln!("skipping {}: {error}", capability.id);
                    }
                }
            }
            let Some(source) = opened else {
                eprintln!("skipping real capture test: no openable V4L2 capture node");
                return;
            };
            source
        };

        let format = source.format();
        assert!(format.width > 0 && format.height > 0, "{format}");

        let first = source.next_frame().await.expect("first frame");
        assert_eq!(first.sequence(), 0);
        assert!(!first.is_empty());
        assert_eq!(first.format(), format);
        if format.pixel_format == PixelFormat::Mjpeg {
            assert!(first.is_keyframe(), "every MJPEG frame is a keyframe");
        }
        eprintln!(
            "captured {} seq={} pts={} keyframe={} bytes={}",
            format,
            first.sequence(),
            first.pts_micros(),
            first.is_keyframe(),
            first.len()
        );

        let second = source.next_frame().await.expect("second frame");
        assert_eq!(second.sequence(), 1);
        assert!(second.pts_micros() > first.pts_micros());
        assert!(!second.is_empty());
        assert_eq!(second.format(), format);

        source.stop().await.expect("stop");
        source.stop().await.expect("stop is idempotent");
    }
}
