// SPDX-License-Identifier: AGPL-3.0-only
//! Looking Glass KVMFR shared-memory capture.
//!
//! Looking Glass lets a GPU-passthrough VM publish its framebuffer over an
//! IVSHMEM region instead of QEMU's emulated VGA device. The host side is a
//! plain file (usually `/dev/shm/looking-glass`) with a fixed header and a
//! frame descriptor at [`FRAME_HEADER_OFFSET`]. This module parses that layout
//! without `unsafe` code and exposes it as a [`CaptureSource`].
//!
//! The parser is deliberately strict:
//!
//! * the magic, header length, frame type, dimensions, stride, offset, and
//!   payload length must all agree before a frame is copied;
//! * the payload is read into an owned [`Frame`] before the descriptor can be
//!   reused, so a guest that recycles the buffer cannot mutate a frame already
//!   emitted;
//! * a missing or malformed SHM file produces a typed [`ApiError`], never a
//!   panic or an empty frame.
//!
//! ## Pixel layouts
//!
//! [`PixelFormat`] has no distinct RGBA or planar-I420 variants. Rather than
//! label those bytes as a format they are not, the parser maps them onto the
//! closest pipeline layout that has the same byte count and performs the
//! conversion while copying:
//!
//! | KVMFR frame type | [`PixelFormat`] | Conversion |
//! |---|---|---|
//! | `BGRA` (1) | `Bgra` | none |
//! | `RGBA` (2) | `Bgra` | R/B swapped per pixel |
//! | `RGB_24` (3) | `Rgb24` | none |
//! | `NV12` (4) | `Nv12` | none |
//! | `YUV420` (5) | `Nv12` | U/V planes interleaved |
//!
//! A conversion that cannot be performed exactly (for example planar I420 with
//! a padded stride or odd dimensions) is rejected with an unsupported error
//! instead of returning mislabeled bytes. Frame types outside this table are
//! also rejected.
//!
//! ## Tests
//!
//! Tests build a temporary KVMFR file, so they never need
//! `/dev/shm/looking-glass` or a running VM. A real shared-memory test would be
//! opt-in and is not part of the default suite.

use std::fmt;
use std::fs::File;
use std::os::unix::fs::FileExt;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameBuffer, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};

use crate::provider::CaptureProvider;
use crate::v4l2::{monotonic_pts, next_sequence};

/// Magic at the start of a KVMFR shared-memory region.
pub const KVMFR_MAGIC: &[u8; 8] = b"KVMFR---";

/// KVMFR protocol version this parser understands.
///
/// The version is reported rather than enforced: a newer host with the same
/// header/frame layout remains readable, while a bad magic is rejected.
pub const KVMFR_VERSION: u32 = 21;

/// Frame type: no frame has been published yet.
pub const FRAME_TYPE_INVALID: u32 = 0;
/// Frame type: packed BGRA8.
pub const FRAME_TYPE_BGRA: u32 = 1;
/// Frame type: packed RGBA8.
pub const FRAME_TYPE_RGBA: u32 = 2;
/// Frame type: packed RGB888.
pub const FRAME_TYPE_RGB_24: u32 = 3;
/// Frame type: YUV 4:2:0 semi-planar (NV12).
pub const FRAME_TYPE_NV12: u32 = 4;
/// Frame type: YUV 4:2:0 planar (I420).
pub const FRAME_TYPE_YUV420: u32 = 5;

/// The frame was damaged in rectangular blocks rather than every pixel.
pub const FRAME_FLAG_BLOCK_OPAQUE: u32 = 1 << 0;

/// Size of the KVMFR header prefix reserved before the frame descriptor.
///
/// The first 16 bytes carry the magic, version, and feature flags; the rest is
/// reserved padding. The frame descriptor starts at [`FRAME_HEADER_OFFSET`],
/// not immediately after these fields.
pub const KVMFR_HEADER_SIZE: usize = 32;

/// Offset of the frame descriptor inside the shared-memory region.
pub const FRAME_HEADER_OFFSET: usize = 1024;

/// Serialized size of [`KvmfrFrameHeader`] (eleven little-endian `u32`s).
pub const FRAME_HEADER_SIZE: usize = 44;

/// Default polling rate when a caller does not choose one.
pub const DEFAULT_FPS: u32 = 30;

/// Largest width or height accepted from a frame descriptor.
///
/// This is a sanity bound against corrupt headers, not a product limit.
pub const MAX_DIMENSION: u32 = 16_384;

/// Parsed KVMFR shared-memory header.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct KvmfrHeader {
    /// Eight-byte magic, [`KVMFR_MAGIC`] for a valid region.
    pub magic: [u8; 8],
    /// KVMFR protocol version reported by the host.
    pub version: u32,
    /// Feature bitmask reported by the host.
    pub features: u32,
}

impl fmt::Debug for KvmfrHeader {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("KvmfrHeader")
            .field("magic", &String::from_utf8_lossy(&self.magic))
            .field("version", &self.version)
            .field("features", &self.features)
            .finish()
    }
}

impl KvmfrHeader {
    /// True when the header carries the expected KVMFR magic.
    pub fn has_valid_magic(&self) -> bool {
        &self.magic == KVMFR_MAGIC
    }
}

/// Parse the header at the start of a KVMFR region.
///
/// Rejects input shorter than [`KVMFR_HEADER_SIZE`] and a bad magic with a
/// typed [`ApiError`]. The version is returned for diagnostics but is not
/// rejected, so a newer host with a compatible layout still works.
pub fn parse_header(data: &[u8]) -> Result<KvmfrHeader, ApiError> {
    if data.len() < KVMFR_HEADER_SIZE {
        return Err(invalid_kvmfr(format!(
            "KVMFR header is {} bytes; at least {KVMFR_HEADER_SIZE} are required.",
            data.len()
        )));
    }

    let mut magic = [0u8; 8];
    magic.copy_from_slice(&data[..8]);
    if &magic != KVMFR_MAGIC {
        return Err(invalid_kvmfr(format!(
            "KVMFR magic is `{}`; expected `KVMFR---`.",
            String::from_utf8_lossy(&magic)
        )));
    }

    Ok(KvmfrHeader {
        magic,
        version: read_u32(data, 8),
        features: read_u32(data, 12),
    })
}

/// Frame descriptor published at [`FRAME_HEADER_OFFSET`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct KvmfrFrameHeader {
    /// Monotonically increasing serial; a change means new pixels.
    pub frame_serial: u32,
    /// One of the `FRAME_TYPE_*` constants.
    pub frame_type: u32,
    /// Frame width in pixels.
    pub width: u32,
    /// Frame height in pixels.
    pub height: u32,
    /// Distance in bytes between consecutive rows of the pixel payload.
    pub stride: u32,
    /// Host-reported pitch (informational; not used for bounds checks).
    pub pitch: u32,
    /// Absolute byte offset of the pixel payload from the start of the region.
    pub offset: u32,
    /// Number of payload bytes to copy.
    pub data_size: u32,
    /// One of the `FRAME_FLAG_*` bits.
    pub flags: u32,
    /// Host rotation in degrees.
    pub rotation: u32,
    /// Number of damaged rectangles following the frame header.
    pub damaged_rects_count: u32,
}

impl KvmfrFrameHeader {
    /// Serialized size of the frame descriptor.
    pub const SIZE: usize = FRAME_HEADER_SIZE;

    /// Parse and validate a frame descriptor from exactly its 44-byte region.
    pub fn parse(data: &[u8]) -> Result<Self, ApiError> {
        if data.len() < Self::SIZE {
            return Err(invalid_kvmfr(format!(
                "KVMFR frame header is {} bytes; exactly {} are required.",
                data.len(),
                Self::SIZE
            )));
        }

        let header = Self {
            frame_serial: read_u32(data, 0),
            frame_type: read_u32(data, 4),
            width: read_u32(data, 8),
            height: read_u32(data, 12),
            stride: read_u32(data, 16),
            pitch: read_u32(data, 20),
            offset: read_u32(data, 24),
            data_size: read_u32(data, 28),
            flags: read_u32(data, 32),
            rotation: read_u32(data, 36),
            damaged_rects_count: read_u32(data, 40),
        };
        header.validate()?;
        Ok(header)
    }

    /// True when the descriptor still has the all-zero "no frame yet" shape.
    pub fn is_placeholder(&self) -> bool {
        self.frame_serial == 0
            && self.width == 0
            && self.height == 0
            && self.data_size == 0
            && self.frame_type == FRAME_TYPE_INVALID
    }

    /// Whether the host marked the frame as block-opaque.
    pub fn has_block_opaque_flag(&self) -> bool {
        self.flags & FRAME_FLAG_BLOCK_OPAQUE != 0
    }

    /// Whether the frame type is one this module understands.
    pub fn has_known_frame_type(&self) -> bool {
        matches!(
            self.frame_type,
            FRAME_TYPE_BGRA
                | FRAME_TYPE_RGBA
                | FRAME_TYPE_RGB_24
                | FRAME_TYPE_NV12
                | FRAME_TYPE_YUV420
        )
    }

    /// Validate type, geometry, stride, payload size, and header offset.
    ///
    /// An all-zero descriptor (before the guest publishes a frame) is accepted
    /// so a source can open early and wait for the first serial. Any other
    /// descriptor must describe a copyable frame.
    pub fn validate(&self) -> Result<(), ApiError> {
        if self.is_placeholder() {
            return Ok(());
        }

        if !self.has_known_frame_type() {
            return Err(unsupported_frame_type(self.frame_type));
        }

        if self.width == 0 || self.height == 0 {
            return Err(invalid_kvmfr(format!(
                "KVMFR frame has empty geometry {}x{}.",
                self.width, self.height
            )));
        }
        if self.width > MAX_DIMENSION || self.height > MAX_DIMENSION {
            return Err(invalid_kvmfr(format!(
                "KVMFR frame {}x{} exceeds the {MAX_DIMENSION}-pixel sanity bound.",
                self.width, self.height
            )));
        }

        let row_bytes = packed_row_bytes(self.width, self.frame_type)
            .ok_or_else(|| unsupported_frame_type(self.frame_type))?;

        if self.stride < row_bytes {
            return Err(invalid_kvmfr(format!(
                "KVMFR stride {} is smaller than one {}-pixel row ({row_bytes} bytes).",
                self.stride, self.width
            )));
        }

        if self.data_size == 0 {
            return Err(invalid_kvmfr("KVMFR frame payload is empty."));
        }

        let minimum_offset = FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE;
        if (self.offset as usize) < minimum_offset {
            return Err(invalid_kvmfr(format!(
                "KVMFR payload offset {} overlaps the region/frame headers (minimum {minimum_offset}).",
                self.offset
            )));
        }

        let required = minimum_payload(self)?;
        if (self.data_size as usize) < required {
            return Err(invalid_kvmfr(format!(
                "KVMFR frame payload is {} bytes; {}x{} needs at least {required}.",
                self.data_size, self.width, self.height
            )));
        }

        Ok(())
    }
}

/// Parse the frame descriptor at [`FRAME_HEADER_OFFSET`] from a full region.
pub fn parse_frame_header(shm: &[u8]) -> Result<KvmfrFrameHeader, ApiError> {
    let end = FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE;
    if shm.len() < end {
        return Err(invalid_kvmfr(format!(
            "KVMFR region is {} bytes; the frame header ends at {end}.",
            shm.len()
        )));
    }
    KvmfrFrameHeader::parse(&shm[FRAME_HEADER_OFFSET..end])
}

/// Verify that the frame payload lies entirely inside a region of `shm_len`.
pub fn validate_frame_bounds(header: &KvmfrFrameHeader, shm_len: usize) -> Result<(), ApiError> {
    header.validate()?;
    let offset = header.offset as usize;
    let size = header.data_size as usize;
    let end = offset.checked_add(size).ok_or_else(|| {
        invalid_kvmfr(format!(
            "KVMFR payload offset {offset} plus {} bytes overflows.",
            header.data_size
        ))
    })?;
    if end > shm_len {
        return Err(invalid_kvmfr(format!(
            "KVMFR payload ends at byte {end}, beyond the {shm_len}-byte region."
        )));
    }
    Ok(())
}

/// Map a KVMFR frame type onto the pipeline's byte layout.
///
/// RGBA and planar YUV420 are accepted because [`PixelFormat`] has no distinct
/// variants for them; [`extract_frame`] performs the byte conversion so the
/// returned bytes really match the reported format. Use
/// [`requires_conversion`] when the exact host layout matters.
pub fn frame_format(header: &KvmfrFrameHeader) -> Result<FrameFormat, ApiError> {
    header.validate()?;
    if header.is_placeholder() {
        return Err(ApiError::new(
            codes::UNSUPPORTED,
            "KVMFR has not published a frame yet (serial 0).",
        )
        .with_remediation("Wait for the Looking Glass host to produce its first frame."));
    }

    let pixel_format = match header.frame_type {
        FRAME_TYPE_BGRA | FRAME_TYPE_RGBA => PixelFormat::Bgra,
        FRAME_TYPE_RGB_24 => PixelFormat::Rgb24,
        FRAME_TYPE_NV12 | FRAME_TYPE_YUV420 => PixelFormat::Nv12,
        other => return Err(unsupported_frame_type(other)),
    };

    Ok(FrameFormat::new(pixel_format, header.width, header.height))
}

/// True when [`extract_frame`] rewrites bytes rather than copying them
/// verbatim (RGBA to BGRA, or planar I420 to semi-planar NV12).
pub fn requires_conversion(frame_type: u32) -> bool {
    matches!(frame_type, FRAME_TYPE_RGBA | FRAME_TYPE_YUV420)
}

/// Copy the frame payload into an owned [`FrameBuffer`].
///
/// Exactly `header.data_size` bytes are copied. RGBA and planar YUV420
/// payloads are converted in place to the layout reported by [`frame_format`];
/// a planar layout with padding or odd dimensions is rejected because the
/// conversion would otherwise be a guess.
pub fn extract_frame(shm: &[u8], header: &KvmfrFrameHeader) -> Result<FrameBuffer, ApiError> {
    validate_frame_bounds(header, shm.len())?;

    let offset = header.offset as usize;
    let size = header.data_size as usize;
    let mut data = shm[offset..offset + size].to_vec();
    normalize_payload(&mut data, header)?;

    FrameBuffer::new(data).map_err(ApiError::from)
}

/// Read one frame payload from an open KVMFR file into an owned buffer.
fn read_frame(
    file: &File,
    header: &KvmfrFrameHeader,
    shm_len: usize,
) -> Result<FrameBuffer, ApiError> {
    validate_frame_bounds(header, shm_len)?;

    let size = header.data_size as usize;
    let mut data = vec![0u8; size];
    file.read_exact_at(&mut data, u64::from(header.offset))
        .map_err(|error| {
            invalid_kvmfr(format!(
                "KVMFR payload read at offset {} failed: {error}",
                header.offset
            ))
        })?;
    normalize_payload(&mut data, header)?;

    FrameBuffer::new(data).map_err(ApiError::from)
}

/// A [`CaptureSource`] backed by a KVMFR shared-memory region.
///
/// The region is read through a file handle rather than a memory mapping:
/// `memmap2`'s mapping constructors are `unsafe` because a concurrently
/// resized file can invalidate the mapping, and this crate forbids unsafe
/// code. Each poll reads only the header window, and a new serial triggers a
/// single exact-size read of the payload into an owned [`Frame`].
pub struct LookingGlassSource {
    id: String,
    path: PathBuf,
    file: Option<File>,
    length: usize,
    interval: Duration,
    origin: Instant,
    stopped: bool,
    last_serial: Option<u32>,
    last_sequence: Option<u64>,
    last_pts: Option<u64>,
    last_frame_type: Option<u32>,
    format: FrameFormat,
}

impl fmt::Debug for LookingGlassSource {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("LookingGlassSource")
            .field("id", &self.id)
            .field("path", &self.path)
            .field("open", &self.file.is_some())
            .field("length", &self.length)
            .field("stopped", &self.stopped)
            .field("last_serial", &self.last_serial)
            .field("format", &self.format)
            .finish()
    }
}

impl LookingGlassSource {
    /// Open a KVMFR region with the default 30 fps polling rate.
    pub fn open(path: impl Into<PathBuf>) -> Result<Self, ApiError> {
        Self::open_with(path, DEFAULT_FPS, None)
    }

    /// Open a KVMFR region with an explicit polling rate and optional id.
    ///
    /// The path may be a symlink; the target must be a regular file at least
    /// large enough to hold the headers. The frame descriptor is validated
    /// immediately, but an all-zero descriptor is accepted so the caller can
    /// wait for the guest's first frame.
    pub fn open_with(
        path: impl Into<PathBuf>,
        fps: u32,
        id: Option<String>,
    ) -> Result<Self, ApiError> {
        if fps == 0 {
            return Err(ApiError::new(
                codes::INVALID_REQUEST,
                "Looking Glass polling rate must be at least 1 fps.",
            )
            .with_remediation("Choose a positive fps value (30 is the default)."));
        }

        let path = path.into();
        let file = File::open(&path).map_err(|error| open_error(&path, error))?;
        let metadata = file.metadata().map_err(|error| open_error(&path, error))?;
        if !metadata.is_file() {
            return Err(ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!(
                    "Looking Glass path `{}` is not a regular shared-memory file.",
                    path.display()
                ),
            )
            .with_details(path.display().to_string())
            .with_remediation(
                "Point the source at QEMU's ivshmem-plain mem-path (usually /dev/shm/looking-glass).",
            ));
        }

        let length = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        let minimum = FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE;
        if length < minimum {
            return Err(invalid_kvmfr(format!(
                "Looking Glass shared memory `{}` is {length} bytes; at least {minimum} are required.",
                path.display()
            )));
        }

        let mut prefix = [0u8; FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE];
        file.read_exact_at(&mut prefix, 0)
            .map_err(|error| open_error(&path, error))?;
        let _header = parse_header(&prefix[..KVMFR_HEADER_SIZE])?;
        let frame_header = parse_frame_header(&prefix)?;

        let format = if frame_header.is_placeholder() {
            // Dimensions are unknown until the first serial arrives. Frame
            // creation validates them again before a frame is emitted.
            FrameFormat::new(PixelFormat::Bgra, 0, 0)
        } else {
            frame_format(&frame_header)?
        };

        let interval = Duration::from_micros((1_000_000 / u64::from(fps)).max(1));
        let id = id.unwrap_or_else(|| path.display().to_string());

        Ok(Self {
            id,
            path,
            file: Some(file),
            length,
            interval,
            origin: Instant::now(),
            stopped: false,
            last_serial: None,
            last_sequence: None,
            last_pts: None,
            last_frame_type: None,
            format,
        })
    }

    /// Path of the KVMFR region.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Polling interval derived from the configured fps.
    pub fn poll_interval(&self) -> Duration {
        self.interval
    }

    /// The KVMFR frame type of the most recently emitted frame, if any.
    pub fn last_frame_type(&self) -> Option<u32> {
        self.last_frame_type
    }
}

#[async_trait]
impl CaptureSource for LookingGlassSource {
    fn id(&self) -> &str {
        &self.id
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        loop {
            if self.stopped {
                return Err(MediaError::EndOfStream);
            }

            let file = self.file.as_ref().ok_or_else(|| {
                MediaError::capture(&self.id, "Looking Glass shared memory is closed")
            })?;

            let mut prefix = [0u8; FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE];
            file.read_exact_at(&mut prefix, 0).map_err(|error| {
                MediaError::capture(&self.id, format!("header read failed: {error}"))
            })?;
            let header = parse_frame_header(&prefix).map_err(MediaError::from)?;

            let fresh = header.frame_serial != 0 && self.last_serial != Some(header.frame_serial);

            if fresh {
                let format = frame_format(&header).map_err(MediaError::from)?;
                let buffer = read_frame(file, &header, self.length).map_err(MediaError::from)?;

                // The guest may publish a new serial while the payload is being
                // read. Re-check before emitting so a torn frame is retried
                // rather than delivered as if it were coherent.
                if current_serial(file).map_err(MediaError::from)? != header.frame_serial {
                    continue;
                }

                let pts = monotonic_pts(elapsed_micros(self.origin), self.last_pts);
                let sequence = next_sequence(self.last_sequence);

                let frame = Frame::new(format, pts, sequence, false, buffer)?;
                self.last_serial = Some(header.frame_serial);
                self.last_sequence = Some(sequence);
                self.last_pts = Some(pts);
                self.last_frame_type = Some(header.frame_type);
                self.format = format;
                return Ok(frame);
            }

            tokio::time::sleep(self.interval).await;
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        // Idempotent: the file handle is dropped and the flag stays set.
        self.stopped = true;
        self.file = None;
        Ok(())
    }
}

/// Default KVMFR shared-memory path used by QEMU's `ivshmem-plain` device.
pub const LOOKING_GLASS_DEFAULT_SHM_PATH: &str = "/dev/shm/looking-glass";

/// Default capture-source id for the Looking Glass provider.
pub const LOOKING_GLASS_DEFAULT_ID: &str = "looking-glass0";

/// Default human-readable name for the Looking Glass provider.
pub const LOOKING_GLASS_DEFAULT_NAME: &str = "Looking Glass IVSHMEM";

/// Default polling rate for the Looking Glass provider.
pub const LOOKING_GLASS_DEFAULT_FPS: u32 = DEFAULT_FPS;

/// Configuration for [`LookingGlassProvider`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LookingGlassProviderConfig {
    /// KVMFR region published by QEMU's `ivshmem-plain` device.
    pub shm_path: PathBuf,
    /// Stable capture source id.
    pub id: String,
    /// Human-readable source name.
    pub name: String,
    /// Polling rate used by the opened [`LookingGlassSource`].
    pub fps: u32,
}

impl Default for LookingGlassProviderConfig {
    fn default() -> Self {
        Self {
            shm_path: PathBuf::from(LOOKING_GLASS_DEFAULT_SHM_PATH),
            id: LOOKING_GLASS_DEFAULT_ID.to_owned(),
            name: LOOKING_GLASS_DEFAULT_NAME.to_owned(),
            fps: LOOKING_GLASS_DEFAULT_FPS,
        }
    }
}

impl LookingGlassProviderConfig {
    /// Configuration for a specific KVMFR region, with the default identity and
    /// polling rate.
    pub fn new(shm_path: impl Into<PathBuf>) -> Self {
        Self {
            shm_path: shm_path.into(),
            ..Self::default()
        }
    }

    /// Override the advertised id and name.
    pub fn with_identity(mut self, id: impl Into<String>, name: impl Into<String>) -> Self {
        self.id = id.into();
        self.name = name.into();
        self
    }

    /// Override the polling rate.
    pub fn with_fps(mut self, fps: u32) -> Self {
        self.fps = fps;
        self
    }

    fn validate(&self) -> Result<(), ApiError> {
        if self.shm_path.as_os_str().is_empty() {
            return Err(ApiError::invalid(
                "The Looking Glass provider requires a non-empty shared-memory path.",
            ));
        }
        if self.id.trim().is_empty() {
            return Err(ApiError::invalid(
                "The Looking Glass provider requires a non-empty id.",
            ));
        }
        if self.fps == 0 {
            return Err(ApiError::invalid("Looking Glass fps must be at least 1."));
        }
        Ok(())
    }
}

/// A capture provider that advertises a Looking Glass KVMFR region.
///
/// Discovery reads only the 32-byte KVMFR header and the frame descriptor at
/// [`FRAME_HEADER_OFFSET`]. It never opens a capture, waits for a serial, or
/// creates a source. A missing path or a region without the KVMFR magic is
/// reported as "no source" (an empty list); a magic-valid region whose frame
/// descriptor has invalid geometry or an unsupported frame type is a discovery
/// error, because claiming it would advertise a source that cannot work.
#[derive(Clone, Debug)]
pub struct LookingGlassProvider {
    config: LookingGlassProviderConfig,
}

impl LookingGlassProvider {
    /// Create a provider for `config`.
    pub fn new(config: LookingGlassProviderConfig) -> Self {
        Self { config }
    }

    /// The configuration this provider was built with.
    pub fn config(&self) -> &LookingGlassProviderConfig {
        &self.config
    }

    /// The capability advertised for the configured KVMFR region.
    ///
    /// Returns `None` when the path is absent, is not a regular file, is too
    /// short to hold the headers, or does not carry the KVMFR magic. Geometry
    /// and frame-type failures are returned as errors.
    pub fn capability(&self) -> Result<Option<CaptureCapability>, ApiError> {
        self.config.validate()?;

        let path = &self.config.shm_path;
        let file = match File::open(path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(open_error(path, error)),
        };
        let metadata = file.metadata().map_err(|error| open_error(path, error))?;
        if !metadata.is_file() {
            return Ok(None);
        }

        let minimum = FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE;
        let length = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
        if length < minimum {
            return Ok(None);
        }

        let mut prefix = [0u8; FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE];
        file.read_exact_at(&mut prefix, 0)
            .map_err(|error| open_error(path, error))?;

        // A foreign or truncated header means "no Looking Glass source here",
        // not a broken provider. Everything after a valid magic is structural
        // and is surfaced to the caller.
        if parse_header(&prefix[..KVMFR_HEADER_SIZE]).is_err() {
            return Ok(None);
        }
        let frame_header = parse_frame_header(&prefix)?;

        let (formats, max_width, max_height) = if frame_header.is_placeholder() {
            // The guest has not published a frame yet. The source still exists,
            // but geometry and the frame type are unknown until the first
            // serial, so advertise no formats rather than a guess.
            (Vec::new(), 0, 0)
        } else {
            let format = frame_format(&frame_header)?;
            (
                vec![format.pixel_format.as_str().to_owned()],
                format.width,
                format.height,
            )
        };

        Ok(Some(CaptureCapability {
            id: self.config.id.clone(),
            name: self.config.name.clone(),
            kind: CaptureKind::LookingGlass,
            formats,
            max_width,
            max_height,
            max_fps: self.config.fps,
            compressed: false,
            hdr: false,
            has_audio: false,
        }))
    }
}

impl CaptureProvider for LookingGlassProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::LookingGlass
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(self.capability()?.into_iter().collect())
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.kind != CaptureKind::LookingGlass || capability.id != self.config.id {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!(
                    "Looking Glass source `{}` is not available from this provider.",
                    capability.id
                ),
            )
            .with_details(capability.id.clone()));
        }

        Ok(Box::new(LookingGlassSource::open_with(
            &self.config.shm_path,
            self.config.fps,
            Some(self.config.id.clone()),
        )?))
    }
}

/// Read just the serial field, used to detect a frame change during a copy.
fn current_serial(file: &File) -> Result<u32, ApiError> {
    let mut bytes = [0u8; 4];
    file.read_exact_at(&mut bytes, FRAME_HEADER_OFFSET as u64)
        .map_err(|error| invalid_kvmfr(format!("KVMFR serial read failed: {error}")).retryable())?;
    Ok(u32::from_le_bytes(bytes))
}

fn read_u32(data: &[u8], offset: usize) -> u32 {
    let bytes = [
        data[offset],
        data[offset + 1],
        data[offset + 2],
        data[offset + 3],
    ];
    u32::from_le_bytes(bytes)
}

fn packed_row_bytes(width: u32, frame_type: u32) -> Option<u32> {
    let bytes_per_pixel = match frame_type {
        FRAME_TYPE_BGRA | FRAME_TYPE_RGBA => 4,
        FRAME_TYPE_RGB_24 => 3,
        FRAME_TYPE_NV12 | FRAME_TYPE_YUV420 => 1,
        _ => return None,
    };
    width.checked_mul(bytes_per_pixel)
}

fn minimum_payload(header: &KvmfrFrameHeader) -> Result<usize, ApiError> {
    let stride = header.stride as usize;
    let height = header.height as usize;
    let luma = stride
        .checked_mul(height)
        .ok_or_else(|| invalid_kvmfr("KVMFR frame dimensions overflow the payload length."))?;

    match header.frame_type {
        FRAME_TYPE_NV12 | FRAME_TYPE_YUV420 => {
            let chroma = stride
                .checked_mul(height.div_ceil(2))
                .ok_or_else(|| invalid_kvmfr("KVMFR chroma plane length overflows."))?;
            luma.checked_add(chroma)
                .ok_or_else(|| invalid_kvmfr("KVMFR frame payload length overflows."))
        }
        _ => Ok(luma),
    }
}

fn normalize_payload(data: &mut [u8], header: &KvmfrFrameHeader) -> Result<(), ApiError> {
    match header.frame_type {
        FRAME_TYPE_RGBA => {
            let stride = header.stride as usize;
            let width = header.width as usize;
            // Bounds were validated, so every row/span is present.
            for row in 0..header.height as usize {
                let base = row * stride;
                for column in 0..width {
                    let pixel = base + column * 4;
                    data.swap(pixel, pixel + 2);
                }
            }
            Ok(())
        }
        FRAME_TYPE_YUV420 => {
            if header.stride != header.width {
                return Err(ApiError::new(
                    codes::UNSUPPORTED,
                    format!(
                        "KVMFR planar YUV420 frame has stride {} for width {}.",
                        header.stride, header.width
                    ),
                )
                .with_details(format!(
                    "{}x{} stride {}",
                    header.width, header.height, header.stride
                ))
                .with_remediation(
                    "Configure the Looking Glass host to publish NV12, or use an unpadded I420 layout.",
                ));
            }
            if header.width % 2 != 0 || header.height % 2 != 0 {
                return Err(ApiError::new(
                    codes::UNSUPPORTED,
                    format!(
                        "KVMFR planar YUV420 frame has odd dimensions {}x{}.",
                        header.width, header.height
                    ),
                )
                .with_remediation(
                    "NV12 requires even dimensions; use a BGRA or NV12 Looking Glass frame type.",
                ));
            }

            let width = header.width as usize;
            let height = header.height as usize;
            let y_size = width * height;
            let chroma = (width / 2) * (height / 2);
            if data.len() < y_size + chroma * 2 {
                return Err(invalid_kvmfr(
                    "KVMFR planar YUV420 payload is shorter than its planes.",
                ));
            }

            let u = data[y_size..y_size + chroma].to_vec();
            let v = data[y_size + chroma..y_size + chroma * 2].to_vec();
            for index in 0..chroma {
                data[y_size + index * 2] = u[index];
                data[y_size + index * 2 + 1] = v[index];
            }
            Ok(())
        }
        _ => Ok(()),
    }
}

fn invalid_kvmfr(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::CAPTURE_UNAVAILABLE, message)
        .with_remediation("Check the QEMU ivshmem-plain device and the Looking Glass host version.")
}

fn unsupported_frame_type(frame_type: u32) -> ApiError {
    ApiError::new(
        codes::UNSUPPORTED,
        format!("KVMFR frame type {frame_type} is not supported by this capture source."),
    )
    .with_details(frame_type.to_string())
    .with_remediation("Use BGRA, RGBA, RGB24, NV12, or YUV420 output from the Looking Glass host.")
}

fn open_error(path: &Path, error: std::io::Error) -> ApiError {
    if error.kind() == std::io::ErrorKind::NotFound {
        ApiError::new(
            codes::NOT_FOUND,
            format!(
                "Looking Glass shared memory `{}` does not exist.",
                path.display()
            ),
        )
        .with_details(path.display().to_string())
        .with_remediation(
            "Start the VM with an ivshmem-plain device backed by this mem-path, then retry.",
        )
    } else {
        ApiError::new(
            codes::CAPTURE_UNAVAILABLE,
            format!(
                "Failed to open Looking Glass shared memory `{}`: {error}",
                path.display()
            ),
        )
        .with_details(path.display().to_string())
        .with_remediation("Check the SHM file permissions and that the VM is running.")
        .retryable()
    }
}

fn elapsed_micros(origin: Instant) -> u64 {
    u64::try_from(origin.elapsed().as_micros()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::io::Write;
    use std::os::unix::fs::FileExt;

    const OFFSET: usize = FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE;

    fn frame_header_bytes(
        frame_serial: u32,
        frame_type: u32,
        width: u32,
        height: u32,
        stride: u32,
        offset: u32,
        data_size: u32,
    ) -> [u8; FRAME_HEADER_SIZE] {
        let mut bytes = [0u8; FRAME_HEADER_SIZE];
        let fields = [
            frame_serial,
            frame_type,
            width,
            height,
            stride,
            stride,
            offset,
            data_size,
            0,
            0,
            0,
        ];
        for (index, value) in fields.iter().enumerate() {
            let start = index * 4;
            bytes[start..start + 4].copy_from_slice(&value.to_le_bytes());
        }
        bytes
    }

    fn valid_header(frame_type: u32) -> KvmfrFrameHeader {
        let width = 2u32;
        let height = 2u32;
        let stride = packed_row_bytes(width, frame_type).unwrap();
        let data_size = match frame_type {
            FRAME_TYPE_NV12 | FRAME_TYPE_YUV420 => stride * (height + height.div_ceil(2)),
            _ => stride * height,
        };
        KvmfrFrameHeader::parse(&frame_header_bytes(
            1,
            frame_type,
            width,
            height,
            stride,
            OFFSET as u32,
            data_size,
        ))
        .unwrap()
    }

    fn write_fixture(
        path: &Path,
        serial: u32,
        frame_type: u32,
        width: u32,
        height: u32,
        stride: u32,
        pixels: &[u8],
    ) {
        let total = OFFSET + pixels.len();
        let mut bytes = vec![0u8; total];
        bytes[..8].copy_from_slice(KVMFR_MAGIC);
        bytes[8..12].copy_from_slice(&KVMFR_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&0u32.to_le_bytes());
        bytes[FRAME_HEADER_OFFSET..OFFSET].copy_from_slice(&frame_header_bytes(
            serial,
            frame_type,
            width,
            height,
            stride,
            OFFSET as u32,
            pixels.len() as u32,
        ));
        bytes[OFFSET..].copy_from_slice(pixels);

        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(path)
            .unwrap();
        file.write_all(&bytes).unwrap();
        file.sync_all().unwrap();
    }

    fn update_fixture(path: &Path, serial: u32, pixels: &[u8]) {
        let file = OpenOptions::new().write(true).open(path).unwrap();
        file.write_all_at(&serial.to_le_bytes(), FRAME_HEADER_OFFSET as u64)
            .unwrap();
        file.write_all_at(pixels, OFFSET as u64).unwrap();
        file.sync_all().unwrap();
    }

    #[test]
    fn magic_and_version_are_parsed() {
        let mut bytes = vec![0u8; KVMFR_HEADER_SIZE];
        bytes[..8].copy_from_slice(KVMFR_MAGIC);
        bytes[8..12].copy_from_slice(&KVMFR_VERSION.to_le_bytes());
        bytes[12..16].copy_from_slice(&7u32.to_le_bytes());

        let header = parse_header(&bytes).unwrap();
        assert_eq!(header.magic, *KVMFR_MAGIC);
        assert_eq!(header.version, KVMFR_VERSION);
        assert_eq!(header.features, 7);
        assert!(header.has_valid_magic());
    }

    #[test]
    fn short_or_foreign_headers_are_rejected() {
        let short = parse_header(&[0u8; 16]).unwrap_err();
        assert_eq!(short.code, codes::CAPTURE_UNAVAILABLE);

        let mut bad = vec![0u8; KVMFR_HEADER_SIZE];
        bad[..8].copy_from_slice(b"NOTKVMFR");
        let error = parse_header(&bad).unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("magic"));
    }

    #[test]
    fn frame_header_fields_are_little_endian() {
        let bytes = frame_header_bytes(9, FRAME_TYPE_BGRA, 640, 480, 2560, 1_100, 1_228_800);
        let header = KvmfrFrameHeader::parse(&bytes).unwrap();
        assert_eq!(header.frame_serial, 9);
        assert_eq!(header.frame_type, FRAME_TYPE_BGRA);
        assert_eq!(header.width, 640);
        assert_eq!(header.height, 480);
        assert_eq!(header.stride, 2560);
        assert_eq!(header.pitch, 2560);
        assert_eq!(header.offset, 1_100);
        assert_eq!(header.data_size, 1_228_800);
    }

    #[test]
    fn placeholder_header_is_not_a_frame() {
        let header = KvmfrFrameHeader::parse(&[0u8; FRAME_HEADER_SIZE]).unwrap();
        assert!(header.is_placeholder());
        assert!(frame_format(&header).is_err());
    }

    #[test]
    fn invalid_geometry_is_rejected() {
        let zero_width = KvmfrFrameHeader::parse(&frame_header_bytes(
            1,
            FRAME_TYPE_BGRA,
            0,
            2,
            8,
            OFFSET as u32,
            16,
        ));
        assert!(zero_width.is_err());

        let short_stride = KvmfrFrameHeader::parse(&frame_header_bytes(
            1,
            FRAME_TYPE_BGRA,
            4,
            4,
            15,
            OFFSET as u32,
            64,
        ));
        assert!(short_stride.is_err());

        let oversized = KvmfrFrameHeader::parse(&frame_header_bytes(
            1,
            FRAME_TYPE_BGRA,
            MAX_DIMENSION + 1,
            2,
            (MAX_DIMENSION + 1) * 4,
            OFFSET as u32,
            64,
        ));
        assert!(oversized.is_err());

        let header = valid_header(FRAME_TYPE_BGRA);
        let bounds = validate_frame_bounds(&header, OFFSET + 8).unwrap_err();
        assert_eq!(bounds.code, codes::CAPTURE_UNAVAILABLE);
    }

    #[test]
    fn frame_format_mapping_is_explicit() {
        assert_eq!(
            frame_format(&valid_header(FRAME_TYPE_BGRA))
                .unwrap()
                .pixel_format,
            PixelFormat::Bgra
        );
        assert_eq!(
            frame_format(&valid_header(FRAME_TYPE_RGBA))
                .unwrap()
                .pixel_format,
            PixelFormat::Bgra
        );
        assert_eq!(
            frame_format(&valid_header(FRAME_TYPE_RGB_24))
                .unwrap()
                .pixel_format,
            PixelFormat::Rgb24
        );
        assert_eq!(
            frame_format(&valid_header(FRAME_TYPE_NV12))
                .unwrap()
                .pixel_format,
            PixelFormat::Nv12
        );
        assert_eq!(
            frame_format(&valid_header(FRAME_TYPE_YUV420))
                .unwrap()
                .pixel_format,
            PixelFormat::Nv12
        );
        assert_eq!(
            frame_format(&valid_header(FRAME_TYPE_BGRA)).unwrap().width,
            2
        );

        let unknown =
            KvmfrFrameHeader::parse(&frame_header_bytes(1, 99, 2, 2, 8, OFFSET as u32, 16));
        assert_eq!(unknown.unwrap_err().code, codes::UNSUPPORTED);
    }

    #[test]
    fn extract_copies_bgra_verbatim() {
        let header = valid_header(FRAME_TYPE_BGRA);
        let mut shm = vec![0u8; OFFSET + 16];
        let pixels = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        shm[OFFSET..].copy_from_slice(&pixels);

        let buffer = extract_frame(&shm, &header).unwrap();
        assert_eq!(buffer.as_slice(), pixels);
        assert!(!requires_conversion(FRAME_TYPE_BGRA));
    }

    #[test]
    fn rgba_is_swizzled_to_bgra() {
        let header = valid_header(FRAME_TYPE_RGBA);
        let mut shm = vec![0u8; OFFSET + 16];
        let pixels = [
            10u8, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255,
        ];
        shm[OFFSET..].copy_from_slice(&pixels);

        let buffer = extract_frame(&shm, &header).unwrap();
        assert_eq!(
            buffer.as_slice(),
            &[30, 20, 10, 255, 60, 50, 40, 255, 90, 80, 70, 255, 120, 110, 100, 255]
        );
        assert!(requires_conversion(FRAME_TYPE_RGBA));
    }

    #[test]
    fn planar_yuv420_is_interleaved_to_nv12() {
        let header = KvmfrFrameHeader::parse(&frame_header_bytes(
            1,
            FRAME_TYPE_YUV420,
            2,
            2,
            2,
            OFFSET as u32,
            6,
        ))
        .unwrap();
        let mut shm = vec![0u8; OFFSET + 6];
        shm[OFFSET..].copy_from_slice(&[10, 20, 30, 40, 100, 200]);

        let buffer = extract_frame(&shm, &header).unwrap();
        assert_eq!(buffer.as_slice(), &[10, 20, 30, 40, 100, 200]);
        assert!(requires_conversion(FRAME_TYPE_YUV420));
    }

    #[test]
    fn padded_planar_yuv420_is_refused_instead_of_guessed() {
        let header = KvmfrFrameHeader::parse(&frame_header_bytes(
            1,
            FRAME_TYPE_YUV420,
            2,
            2,
            4,
            OFFSET as u32,
            12,
        ))
        .unwrap();
        let shm = vec![0u8; OFFSET + 12];
        let error = extract_frame(&shm, &header).unwrap_err();
        assert_eq!(error.code, codes::UNSUPPORTED);
    }

    #[tokio::test]
    async fn source_emits_two_serials_with_monotonic_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("looking-glass");
        let first_pixels = [1u8, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, 16];
        let second_pixels = [
            21u8, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31, 32, 33, 34, 35, 36,
        ];
        write_fixture(&path, 1, FRAME_TYPE_BGRA, 2, 2, 8, &first_pixels);

        let mut source = LookingGlassSource::open_with(&path, 1_000, Some("lg-test".into()))
            .expect("fixture is valid KVMFR");
        assert_eq!(source.id(), "lg-test");
        assert_eq!(source.format().width, 2);
        assert_eq!(source.format().height, 2);

        let first = source.next_frame().await.unwrap();
        assert_eq!(first.sequence(), 0);
        assert_eq!(first.format(), FrameFormat::new(PixelFormat::Bgra, 2, 2));
        assert_eq!(first.data(), first_pixels);

        update_fixture(&path, 2, &second_pixels);
        let second = source.next_frame().await.unwrap();
        assert_eq!(second.sequence(), 1);
        assert_eq!(second.data(), second_pixels);
        assert!(
            second.pts_micros() > first.pts_micros(),
            "PTS must increase: {} then {}",
            first.pts_micros(),
            second.pts_micros()
        );

        source.stop().await.unwrap();
        assert_eq!(
            source.next_frame().await.unwrap_err(),
            MediaError::EndOfStream
        );
        // Stop is idempotent.
        source.stop().await.unwrap();
        assert!(source.path().ends_with("looking-glass"));
    }

    #[tokio::test]
    async fn missing_shared_memory_is_a_typed_error() {
        let error = LookingGlassSource::open("/nonexistent/ozma-kvmfr").unwrap_err();
        assert_eq!(error.code, codes::NOT_FOUND);
        assert!(error.remediation.is_some());

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("empty");
        File::create(&path).unwrap();
        let error = LookingGlassSource::open(&path).unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn foreign_magic_is_a_typed_error() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("not-kvmfr");
        write_fixture(&path, 1, FRAME_TYPE_BGRA, 2, 2, 8, &[0u8; 16]);
        let file = OpenOptions::new().write(true).open(&path).unwrap();
        file.write_all_at(b"NOTKVMFR", 0).unwrap();
        file.sync_all().unwrap();

        let error = LookingGlassSource::open(&path).unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("magic"));
    }

    #[tokio::test]
    async fn provider_advertises_and_opens_a_real_kvmfr_region() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("looking-glass");
        let width = 640u32;
        let height = 480u32;
        let pixels = vec![7u8; (width * height * 3) as usize];
        write_fixture(
            &path,
            1,
            FRAME_TYPE_RGB_24,
            width,
            height,
            width * 3,
            &pixels,
        );

        let provider = LookingGlassProvider::new(
            LookingGlassProviderConfig::new(&path)
                .with_identity("lg-test", "Test Looking Glass")
                .with_fps(60),
        );

        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        let capability = &capabilities[0];
        assert_eq!(capability.id, "lg-test");
        assert_eq!(capability.name, "Test Looking Glass");
        assert_eq!(capability.kind, CaptureKind::LookingGlass);
        assert_eq!(capability.formats, ["rgb24"]);
        assert_eq!(
            (capability.max_width, capability.max_height),
            (width, height)
        );
        assert_eq!(capability.max_fps, 60);
        assert!(!capability.compressed);
        assert!(!capability.hdr);
        assert!(!capability.has_audio);

        let mut source = provider.open(capability).unwrap();
        let frame = source.next_frame().await.unwrap();
        assert_eq!(frame.format(), FrameFormat::rgb24(width, height));
        assert_eq!(frame.data(), pixels);
        source.stop().await.unwrap();
    }

    #[test]
    fn provider_maps_kvmfr_frame_types_to_pipeline_formats() {
        let dir = tempfile::tempdir().unwrap();
        let cases = [
            (FRAME_TYPE_BGRA, "bgra", 4usize),
            (FRAME_TYPE_RGBA, "bgra", 4),
            (FRAME_TYPE_RGB_24, "rgb24", 3),
            (FRAME_TYPE_NV12, "nv12", 1),
            (FRAME_TYPE_YUV420, "nv12", 1),
        ];

        for (index, (frame_type, expected, bytes_per_pixel)) in cases.into_iter().enumerate() {
            let path = dir.path().join(format!("looking-glass-{index}"));
            let stride = 2 * bytes_per_pixel;
            let data_size = if matches!(frame_type, FRAME_TYPE_NV12 | FRAME_TYPE_YUV420) {
                stride * 3
            } else {
                stride * 2
            };
            write_fixture(
                &path,
                1,
                frame_type,
                2,
                2,
                stride as u32,
                &vec![0; data_size],
            );

            let provider = LookingGlassProvider::new(LookingGlassProviderConfig::new(&path));
            let capabilities = provider.enumerate().unwrap();
            assert_eq!(
                capabilities[0].formats,
                [expected],
                "frame type {frame_type}"
            );
            assert_eq!(
                (capabilities[0].max_width, capabilities[0].max_height),
                (2, 2)
            );
        }
    }

    #[test]
    fn provider_treats_absent_and_foreign_shared_memory_as_no_source() {
        let dir = tempfile::tempdir().unwrap();

        let missing =
            LookingGlassProvider::new(LookingGlassProviderConfig::new(dir.path().join("absent")));
        assert!(missing.enumerate().unwrap().is_empty());

        let foreign_path = dir.path().join("foreign");
        let mut foreign = vec![0u8; FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE];
        foreign[..8].copy_from_slice(b"NOTKVMFR");
        std::fs::write(&foreign_path, foreign).unwrap();
        let foreign = LookingGlassProvider::new(LookingGlassProviderConfig::new(&foreign_path));
        assert!(foreign.enumerate().unwrap().is_empty());

        let short_path = dir.path().join("short");
        std::fs::write(&short_path, [0u8; 16]).unwrap();
        let short = LookingGlassProvider::new(LookingGlassProviderConfig::new(&short_path));
        assert!(short.enumerate().unwrap().is_empty());
    }

    #[test]
    fn provider_rejects_invalid_geometry_instead_of_advertising_it() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bad-geometry");
        let mut bytes = vec![0u8; OFFSET + 16];
        bytes[..8].copy_from_slice(KVMFR_MAGIC);
        bytes[8..12].copy_from_slice(&KVMFR_VERSION.to_le_bytes());
        bytes[FRAME_HEADER_OFFSET..OFFSET].copy_from_slice(&frame_header_bytes(
            1,
            FRAME_TYPE_BGRA,
            0,
            2,
            8,
            OFFSET as u32,
            16,
        ));
        std::fs::write(&path, bytes).unwrap();

        let provider = LookingGlassProvider::new(LookingGlassProviderConfig::new(&path));
        let error = provider.enumerate().unwrap_err();
        assert_eq!(error.code, codes::CAPTURE_UNAVAILABLE);
        assert!(error.message.contains("geometry"));
    }

    #[test]
    fn provider_advertises_a_placeholder_before_the_first_frame() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("placeholder");
        let mut bytes = vec![0u8; FRAME_HEADER_OFFSET + FRAME_HEADER_SIZE];
        bytes[..8].copy_from_slice(KVMFR_MAGIC);
        bytes[8..12].copy_from_slice(&KVMFR_VERSION.to_le_bytes());
        std::fs::write(&path, bytes).unwrap();

        let provider = LookingGlassProvider::new(LookingGlassProviderConfig::new(&path));
        let capabilities = provider.enumerate().unwrap();
        assert_eq!(capabilities.len(), 1);
        assert!(capabilities[0].formats.is_empty());
        assert_eq!(
            (
                capabilities[0].max_width,
                capabilities[0].max_height,
                capabilities[0].max_fps
            ),
            (0, 0, LOOKING_GLASS_DEFAULT_FPS)
        );
    }

    #[test]
    fn provider_rejects_a_mismatched_kind_or_id() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("looking-glass");
        write_fixture(&path, 1, FRAME_TYPE_BGRA, 2, 2, 8, &[0u8; 16]);
        let provider = LookingGlassProvider::new(LookingGlassProviderConfig::new(&path));
        let capability = provider.enumerate().unwrap().remove(0);

        let mut wrong_kind = capability.clone();
        wrong_kind.kind = CaptureKind::Fake;
        assert_eq!(
            provider
                .open(&wrong_kind)
                .err()
                .expect("wrong kind must fail")
                .code,
            codes::NOT_FOUND
        );

        let mut wrong_id = capability;
        wrong_id.id = "someone-else".to_owned();
        assert_eq!(
            provider
                .open(&wrong_id)
                .err()
                .expect("wrong id must fail")
                .code,
            codes::NOT_FOUND
        );
    }
}
