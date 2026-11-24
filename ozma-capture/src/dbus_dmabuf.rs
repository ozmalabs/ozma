// SPDX-License-Identifier: AGPL-3.0-only
//! QEMU D-Bus `ScanoutDMABUF` / `ScanoutDMABUF2` import.
//!
//! QEMU's `-display dbus,gl=on` listener does not send the pixman
//! `Scanout`/`Update` byte arrays. It sends:
//!
//! ```text
//! ScanoutDMABUF(h dmabuf, u width, u height, u stride, u fourcc,
//!               t modifier, b y0_top)          signature "huuuutb"
//! ScanoutDMABUF2(ah dmabuf, u x, u y, u width, u height,
//!                au offset, au stride, u num_planes, u fourcc,
//!                u backing_width, u backing_height, t modifier,
//!                b y0_top)                    signature "ahuuuuauauuuuutb"
//! UpdateDMABUF(i x, i y, i w, i h)             signature "iiii"
//! ```
//!
//! `ScanoutDMABUF` carries one DMA-BUF descriptor in the message's SCM_RIGHTS
//! list. `ScanoutDMABUF2` (advertised through the listener's `Interfaces`
//! property) carries one descriptor per plane plus the backing geometry, pixel
//! origin, and per-plane offsets/strides. `UpdateDMABUF` carries only a damage
//! rectangle: it refers to the most recent scanout buffer, whose contents the
//! GPU has already updated. This module validates the header, maps each plane
//! read-only, and converts it into the tightly packed BGRA layout the rest of
//! the capture pipeline uses.
//!
//! ## Supported layouts
//!
//! Only `DRM_FORMAT_MOD_LINEAR` (`0`) is mappable without a format-specific
//! decoder. The packed 32-bit formats below are converted. Linear YUV 4:2:0 in
//! the usual arrangements is supported: `NV12`/`NV21` two-plane (or, for the
//! v1 signature only, single-fd contiguous `NV12`) and `YUV420`/`YVU420`
//! three-plane. A tiled or compressed modifier is rejected with a typed error
//! naming the modifier and fourcc rather than being interpreted as linear
//! pixels.
//!
//! ## CPU access
//!
//! A DMA-BUF mapped for CPU access must be bracketed by
//! `DMA_BUF_IOCTL_SYNC` start/end read. [`MappedDmaBuf`] issues that ioctl
//! around every snapshot or damage copy. Exporters without cache operations
//! (`ENOTTY`/`EINVAL`) are treated as coherent, which is the case on this x86
//! host; any other sync failure is a typed import error rather than silently
//! reading stale pixels.
//!
//! ## The isolated `unsafe` operations
//!
//! `memmap2`'s mapping constructor is `unsafe` because a regular file can be
//! truncated while mapped. A DMA-BUF's size is fixed for the lifetime of its
//! export, this mapping is read-only, and the [`File`] (the only owner of the
//! descriptor) is held for the mapping's whole lifetime. `OwnedFd::from_raw_fd`
//! is also isolated here: the descriptors come directly from `SCM_RIGHTS`,
//! have `MSG_CMSG_CLOEXEC` set by the receiver, and are adopted exactly once.
//! `libc::ioctl` is the only way to issue `DMA_BUF_IOCTL_SYNC`; it operates on
//! a plain `#[repr(C)]` value and the borrowed descriptor.

use std::fs::File;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};

use memmap2::Mmap;

/// `DRM_FORMAT_MOD_LINEAR`: the only modifier this importer can interpret.
pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Upper bound on a mapped DMABUF, matching the D-Bus body bound.
const MAX_DMABUF_BYTES: usize = 512 * 1024 * 1024;

/// Largest plane count accepted from `ScanoutDMABUF2`.
const MAX_PLANES: usize = 4;

/// `DMA_BUF_IOCTL_SYNC`: `_IOW('b', 0, struct dma_buf_sync)` on Linux.
///
/// `struct dma_buf_sync` is a single `__u64`, so the encoded size is 8 and the
/// type letter is `b` (`0x62`). This matches `include/uapi/linux/dma-buf.h`.
const DMA_BUF_IOCTL_SYNC: nix::libc::c_ulong = 0x4008_6200;
const DMA_BUF_SYNC_READ: u64 = 1 << 0;
const DMA_BUF_SYNC_END: u64 = 1 << 2;

#[repr(C)]
struct DmaBufSync {
    flags: u64,
}

const fn fourcc(a: u8, b: u8, c: u8, d: u8) -> u32 {
    a as u32 | (b as u32) << 8 | (c as u32) << 16 | (d as u32) << 24
}

pub(crate) const DRM_FORMAT_XBGR8888: u32 = fourcc(b'X', b'B', b'2', b'4');
pub(crate) const DRM_FORMAT_ABGR8888: u32 = fourcc(b'A', b'B', b'2', b'4');
pub(crate) const DRM_FORMAT_XRGB8888: u32 = fourcc(b'X', b'R', b'2', b'4');
pub(crate) const DRM_FORMAT_ARGB8888: u32 = fourcc(b'A', b'R', b'2', b'4');
pub(crate) const DRM_FORMAT_BGRA8888: u32 = fourcc(b'B', b'A', b'2', b'4');
pub(crate) const DRM_FORMAT_RGBA8888: u32 = fourcc(b'R', b'A', b'2', b'4');
pub(crate) const DRM_FORMAT_NV12: u32 = fourcc(b'N', b'V', b'1', b'2');
pub(crate) const DRM_FORMAT_NV21: u32 = fourcc(b'N', b'V', b'2', b'1');
pub(crate) const DRM_FORMAT_YUV420: u32 = fourcc(b'Y', b'U', b'1', b'2');
pub(crate) const DRM_FORMAT_YVU420: u32 = fourcc(b'Y', b'V', b'1', b'2');

/// D-Bus signature of the `ScanoutDMABUF2` body.
pub(crate) const SCANOUT_DMABUF2_SIGNATURE: &str = "ahuuuuauauuuuutb";

/// Typed failures from DMA-BUF header parsing and import.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DmaBufError {
    /// The message body did not match the expected layout.
    #[error("malformed DMA-BUF scanout: {0}")]
    Protocol(String),
    /// The format, modifier, or mapping is not supported.
    #[error("unsupported DMA-BUF scanout: {0}")]
    Unsupported(String),
    /// The declared dimensions, plane table, or buffer size are invalid.
    #[error("invalid DMA-BUF geometry: {0}")]
    Geometry(String),
}

/// Header fields of one `ScanoutDMABUF` method call.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct ScanoutDmabuf {
    /// Index into the message's unix-fd list (`h`).
    pub fd_index: u32,
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// Row stride in bytes.
    pub stride: u32,
    /// DRM format fourcc.
    pub fourcc: u32,
    /// DRM format modifier.
    pub modifier: u64,
    /// Whether buffer row 0 is the top row of the image.
    pub y0_top: bool,
}

/// Header fields of one multi-plane `ScanoutDMABUF2` method call.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct ScanoutDmabuf2 {
    /// Indices into the message's unix-fd list (`ah`), one per plane.
    pub fd_indexes: Vec<u32>,
    /// X origin of the scanout rectangle inside the backing buffer.
    pub x: u32,
    /// Y origin of the scanout rectangle inside the backing buffer.
    pub y: u32,
    /// Display width in pixels.
    pub width: u32,
    /// Display height in pixels.
    pub height: u32,
    /// Per-plane byte offsets.
    pub offsets: Vec<u32>,
    /// Per-plane row strides in bytes.
    pub strides: Vec<u32>,
    /// Number of planes (must match the fd/offset/stride arrays).
    pub num_planes: u32,
    /// DRM format fourcc.
    pub fourcc: u32,
    /// Backing buffer width in pixels.
    pub backing_width: u32,
    /// Backing buffer height in pixels.
    pub backing_height: u32,
    /// DRM format modifier.
    pub modifier: u64,
    /// Whether buffer row 0 is the top row of the image.
    pub y0_top: bool,
}

fn read_u32(body: &[u8], offset: usize) -> Option<u32> {
    let bytes = body.get(offset..offset.checked_add(4)?)?;
    Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
}

fn read_u64(body: &[u8], offset: usize) -> Option<u64> {
    let bytes = body.get(offset..offset.checked_add(8)?)?;
    let mut value = [0u8; 8];
    value.copy_from_slice(bytes);
    Some(u64::from_le_bytes(value))
}

/// Printable name for a fourcc, for error messages.
pub(crate) fn fourcc_name(value: u32) -> String {
    let bytes = [
        value as u8,
        (value >> 8) as u8,
        (value >> 16) as u8,
        (value >> 24) as u8,
    ];
    if bytes.iter().all(|byte| byte.is_ascii_graphic()) {
        format!(
            "'{}{}{}{}' ({value:#010x})",
            bytes[0] as char, bytes[1] as char, bytes[2] as char, bytes[3] as char
        )
    } else {
        format!("{value:#010x}")
    }
}

/// Parse the fixed `huuuutb` body of `ScanoutDMABUF`.
///
/// Layout: `h` at 0, width at 4, height at 8, stride at 12, fourcc at 16,
/// four bytes of D-Bus padding at 20, `t` modifier at 24, `b` at 32.
pub(crate) fn parse_scanout_dmabuf(body: &[u8]) -> Result<ScanoutDmabuf, DmaBufError> {
    const BODY_LEN: usize = 36;
    if body.len() != BODY_LEN {
        return Err(DmaBufError::Protocol(format!(
            "body is {} bytes; the huuuutb layout is {BODY_LEN}",
            body.len()
        )));
    }
    let fd_index = read_u32(body, 0).expect("length checked");
    let width = read_u32(body, 4).expect("length checked");
    let height = read_u32(body, 8).expect("length checked");
    let stride = read_u32(body, 12).expect("length checked");
    let fourcc = read_u32(body, 16).expect("length checked");
    // Bytes 20..24 are D-Bus alignment padding before the 8-byte modifier.
    let modifier = read_u64(body, 24).expect("length checked");
    let y0_top = read_u32(body, 32).expect("length checked");
    if y0_top > 1 {
        return Err(DmaBufError::Protocol(format!(
            "y0_top is {y0_top}; a D-Bus boolean must be 0 or 1"
        )));
    }
    Ok(ScanoutDmabuf {
        fd_index,
        width,
        height,
        stride,
        fourcc,
        modifier,
        y0_top: y0_top == 1,
    })
}

/// A bounds-checked little-endian reader for a variable-size D-Bus body.
struct BodyReader<'a> {
    body: &'a [u8],
    position: usize,
}

impl<'a> BodyReader<'a> {
    fn new(body: &'a [u8]) -> Self {
        Self { body, position: 0 }
    }

    fn align(&mut self, alignment: usize) {
        let remainder = self.position % alignment;
        if remainder != 0 {
            self.position += alignment - remainder;
        }
    }

    fn u32(&mut self) -> Result<u32, DmaBufError> {
        self.align(4);
        let value = read_u32(self.body, self.position).ok_or_else(|| {
            DmaBufError::Protocol(format!(
                "body ends before the uint32 at byte {}",
                self.position
            ))
        })?;
        self.position += 4;
        Ok(value)
    }

    fn u64(&mut self) -> Result<u64, DmaBufError> {
        self.align(8);
        let value = read_u64(self.body, self.position).ok_or_else(|| {
            DmaBufError::Protocol(format!(
                "body ends before the uint64 at byte {}",
                self.position
            ))
        })?;
        self.position += 8;
        Ok(value)
    }

    /// Read an `ah` array. D-Bus arrays store a byte length followed by
    /// padding to the element alignment.
    fn handles(&mut self) -> Result<Vec<u32>, DmaBufError> {
        let byte_len = self.u32()? as usize;
        if byte_len > MAX_PLANES * 4 || byte_len % 4 != 0 {
            return Err(DmaBufError::Protocol(format!(
                "handle array is {byte_len} bytes; expected a multiple of 4 for at most {MAX_PLANES} planes"
            )));
        }
        let count = byte_len / 4;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.u32()?);
        }
        Ok(values)
    }

    /// Read an `au` array.
    fn u32_array(&mut self) -> Result<Vec<u32>, DmaBufError> {
        let byte_len = self.u32()? as usize;
        if byte_len > MAX_PLANES * 4 || byte_len % 4 != 0 {
            return Err(DmaBufError::Protocol(format!(
                "uint32 array is {byte_len} bytes; expected a multiple of 4 for at most {MAX_PLANES} planes"
            )));
        }
        let count = byte_len / 4;
        let mut values = Vec::with_capacity(count);
        for _ in 0..count {
            values.push(self.u32()?);
        }
        Ok(values)
    }

    fn finish(self, layout: &str) -> Result<(), DmaBufError> {
        // The D-Bus body need not end on an 8-byte boundary, but GDBus may
        // leave a few zero alignment bytes. Reject any other trailing data.
        match self.body.get(self.position..) {
            Some(rest) if rest.iter().all(|byte| *byte == 0) => Ok(()),
            Some(rest) => Err(DmaBufError::Protocol(format!(
                "{} trailing bytes after the {layout} body",
                rest.len()
            ))),
            None => Err(DmaBufError::Protocol(format!(
                "body ends inside the {layout} layout"
            ))),
        }
    }
}

/// Parse the variable-size `ahuuuuauauuuuutb` body of `ScanoutDMABUF2`.
///
/// Layout: `ah` handles, x, y, width, height, `au` offsets, `au` strides,
/// num_planes, fourcc, backing_width, backing_height, `t` modifier at the next
/// 8-byte boundary, then `b` y0_top.
pub(crate) fn parse_scanout_dmabuf2(body: &[u8]) -> Result<ScanoutDmabuf2, DmaBufError> {
    let mut reader = BodyReader::new(body);
    let fd_indexes = reader.handles()?;
    let x = reader.u32()?;
    let y = reader.u32()?;
    let width = reader.u32()?;
    let height = reader.u32()?;
    let offsets = reader.u32_array()?;
    let strides = reader.u32_array()?;
    let num_planes = reader.u32()?;
    let fourcc = reader.u32()?;
    let backing_width = reader.u32()?;
    let backing_height = reader.u32()?;
    let modifier = reader.u64()?;
    let y0_top = reader.u32()?;
    if y0_top > 1 {
        return Err(DmaBufError::Protocol(format!(
            "y0_top is {y0_top}; a D-Bus boolean must be 0 or 1"
        )));
    }
    reader.finish("ahuuuuauauuuuutb")?;

    let planes = num_planes as usize;
    if planes == 0 || planes > MAX_PLANES {
        return Err(DmaBufError::Protocol(format!(
            "num_planes is {num_planes}; expected 1..={MAX_PLANES}"
        )));
    }
    if fd_indexes.len() != planes {
        return Err(DmaBufError::Protocol(format!(
            "num_planes is {num_planes}, but the handle array names {} descriptors",
            fd_indexes.len()
        )));
    }
    if offsets.len() != planes || strides.len() != planes {
        return Err(DmaBufError::Protocol(format!(
            "num_planes is {num_planes}, but offset/stride arrays have {}/{} entries",
            offsets.len(),
            strides.len()
        )));
    }

    Ok(ScanoutDmabuf2 {
        fd_indexes,
        x,
        y,
        width,
        height,
        offsets,
        strides,
        num_planes,
        fourcc,
        backing_width,
        backing_height,
        modifier,
        y0_top: y0_top == 1,
    })
}

/// Parse the fixed `iiii` body of `UpdateDMABUF`.
pub(crate) fn parse_update_dmabuf(body: &[u8]) -> Result<(i32, i32, i32, i32), DmaBufError> {
    const BODY_LEN: usize = 16;
    if body.len() != BODY_LEN {
        return Err(DmaBufError::Protocol(format!(
            "body is {} bytes; the iiii layout is {BODY_LEN}",
            body.len()
        )));
    }
    let value = |offset| read_u32(body, offset).expect("length checked") as i32;
    Ok((value(0), value(4), value(8), value(12)))
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum DmaBufFormat {
    Xbgr8888,
    Abgr8888,
    Xrgb8888,
    Argb8888,
    Bgra8888,
    Rgba8888,
    Nv12,
    Nv21,
    Yuv420,
    Yvu420,
}

impl DmaBufFormat {
    fn from_fourcc(fourcc: u32) -> Option<Self> {
        match fourcc {
            DRM_FORMAT_XBGR8888 => Some(Self::Xbgr8888),
            DRM_FORMAT_ABGR8888 => Some(Self::Abgr8888),
            DRM_FORMAT_XRGB8888 => Some(Self::Xrgb8888),
            DRM_FORMAT_ARGB8888 => Some(Self::Argb8888),
            DRM_FORMAT_BGRA8888 => Some(Self::Bgra8888),
            DRM_FORMAT_RGBA8888 => Some(Self::Rgba8888),
            DRM_FORMAT_NV12 => Some(Self::Nv12),
            DRM_FORMAT_NV21 => Some(Self::Nv21),
            DRM_FORMAT_YUV420 => Some(Self::Yuv420),
            DRM_FORMAT_YVU420 => Some(Self::Yvu420),
            _ => None,
        }
    }

    fn plane_count(self) -> usize {
        match self {
            Self::Nv12 | Self::Nv21 => 2,
            Self::Yuv420 | Self::Yvu420 => 3,
            _ => 1,
        }
    }

    fn is_yuv(self) -> bool {
        matches!(self, Self::Nv12 | Self::Nv21 | Self::Yuv420 | Self::Yvu420)
    }

    fn has_alpha(self) -> bool {
        matches!(
            self,
            Self::Abgr8888 | Self::Argb8888 | Self::Bgra8888 | Self::Rgba8888
        )
    }

    /// Convert one packed 32-bit source row into BGRA.
    fn convert_row(self, source: &[u8], destination: &mut [u8], width: usize) {
        let alpha = if self.has_alpha() { 0 } else { 0xff };
        for x in 0..width {
            let s = &source[x * 4..x * 4 + 4];
            let d = &mut destination[x * 4..x * 4 + 4];
            match self {
                // Memory order R,G,B,A (ABGR/XBGR are little-endian names).
                Self::Abgr8888 | Self::Xbgr8888 => {
                    d[0] = s[2];
                    d[1] = s[1];
                    d[2] = s[0];
                    d[3] = if alpha == 0 { s[3] } else { alpha };
                }
                // Memory order B,G,R,A.
                Self::Argb8888 | Self::Xrgb8888 => {
                    d[0] = s[0];
                    d[1] = s[1];
                    d[2] = s[2];
                    d[3] = if alpha == 0 { s[3] } else { alpha };
                }
                // DRM_FORMAT_BGRA8888 is [31:0] B:G:R:A, i.e. A,R,G,B bytes.
                Self::Bgra8888 => {
                    d[0] = s[3];
                    d[1] = s[2];
                    d[2] = s[1];
                    d[3] = s[0];
                }
                // DRM_FORMAT_RGBA8888 is [31:0] R:G:B:A, i.e. A,B,G,R bytes.
                Self::Rgba8888 => {
                    d[0] = s[1];
                    d[1] = s[2];
                    d[2] = s[3];
                    d[3] = s[0];
                }
                Self::Nv12 | Self::Nv21 | Self::Yuv420 | Self::Yvu420 => {
                    unreachable!("YUV formats are converted plane-wise")
                }
            }
        }
    }
}

fn clip(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// Convert one limited-range BT.601 Y/U/V sample to packed BGRA.
fn yuv_to_bgra(y: i32, u: i32, v: i32) -> [u8; 4] {
    let c = y - 16;
    let d = u - 128;
    let e = v - 128;
    [
        clip((298 * c + 516 * d + 128) >> 8),
        clip((298 * c - 100 * d - 208 * e + 128) >> 8),
        clip((298 * c + 409 * e + 128) >> 8),
        0xff,
    ]
}

fn ceil2(value: usize) -> usize {
    (value + 1) / 2
}

fn round_up2(value: usize) -> usize {
    (value + 1) & !1
}

/// One mapped DMA-BUF plane, backed by a descriptor this struct owns.
struct MappedPlane {
    /// Kept first so the mapping is unmapped before the descriptor is closed.
    map: Mmap,
    file: File,
    offset: usize,
    stride: usize,
}

impl MappedPlane {
    fn sync(&self, start: bool) -> Result<(), DmaBufError> {
        let flags = DMA_BUF_SYNC_READ | if start { 0 } else { DMA_BUF_SYNC_END };
        let request = DmaBufSync { flags };
        // SAFETY: `DmaBufSync` is a plain `#[repr(C)]` 8-byte value matching
        // `struct dma_buf_sync`, the descriptor is owned by `self.file`, and
        // the ioctl only reads that value (the kernel documents the direction
        // as `_IOW`). No Rust memory is mutated by the kernel here.
        let result = unsafe {
            nix::libc::ioctl(
                self.file.as_raw_fd(),
                DMA_BUF_IOCTL_SYNC,
                &request as *const DmaBufSync,
            )
        };
        if result == 0 {
            return Ok(());
        }
        let error = io::Error::last_os_error();
        match error.raw_os_error() {
            // Not a DMA-BUF exporter with cache operations (test files, some
            // host drivers). The mapping is coherent on this host; do not
            // reject a perfectly readable buffer just because sync is a no-op.
            Some(code)
                if code == nix::libc::ENOTTY
                    || code == nix::libc::EINVAL
                    || code == nix::libc::EOPNOTSUPP =>
            {
                tracing::debug!(
                    fd = self.file.as_raw_fd(),
                    %error,
                    "DMA_BUF_IOCTL_SYNC is not supported; assuming a coherent mapping"
                );
                Ok(())
            }
            _ => Err(DmaBufError::Unsupported(format!(
                "DMA_BUF_IOCTL_SYNC {} failed: {error}",
                if start { "start" } else { "end" }
            ))),
        }
    }
}

/// A validated, read-only mapping of the current QEMU scanout DMA-BUF.
pub(crate) struct MappedDmaBuf {
    planes: Vec<MappedPlane>,
    width: u32,
    height: u32,
    x: u32,
    y: u32,
    backing_width: u32,
    backing_height: u32,
    format: DmaBufFormat,
    y0_top: bool,
    /// The v1 signature can only describe `NV12` as one fd with Y then UV.
    contiguous_nv12: bool,
}

impl std::fmt::Debug for MappedDmaBuf {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MappedDmaBuf")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("x", &self.x)
            .field("y", &self.y)
            .field("backing_width", &self.backing_width)
            .field("backing_height", &self.backing_height)
            .field("planes", &self.planes.len())
            .field("format", &self.format)
            .field("y0_top", &self.y0_top)
            .finish_non_exhaustive()
    }
}

/// Map one received descriptor read-only for `offset + stride * rows` bytes.
#[allow(clippy::too_many_arguments)]
fn map_plane(
    fd: OwnedFd,
    offset: usize,
    stride: usize,
    rows: usize,
    fourcc: u32,
    modifier: u64,
) -> Result<MappedPlane, DmaBufError> {
    let stride_bytes = stride
        .checked_mul(rows)
        .ok_or_else(|| DmaBufError::Geometry("plane length overflows".to_owned()))?;
    let needed = offset
        .checked_add(stride_bytes)
        .ok_or_else(|| DmaBufError::Geometry("plane offset overflows".to_owned()))?;
    if needed == 0 || needed > MAX_DMABUF_BYTES {
        return Err(DmaBufError::Geometry(format!(
            "mapped size {needed} is outside 1..={MAX_DMABUF_BYTES}"
        )));
    }

    let file = File::from(fd);
    let reported = file
        .metadata()
        .map_err(|error| DmaBufError::Geometry(format!("fstat of DMA-BUF failed: {error}")))?
        .len();
    if reported > 0 && reported < needed as u64 {
        return Err(DmaBufError::Geometry(format!(
            "DMA-BUF reports {reported} bytes; {needed} are required"
        )));
    }

    // SAFETY: `memmap2` requires that the file cannot be truncated while
    // mapped. A DMA-BUF's size is fixed for the lifetime of its export, the
    // mapping is read-only, and `file` is moved into the returned struct so the
    // descriptor cannot be closed or replaced before `map` is dropped. The
    // kernel's DMA-BUF mmap implementation is the only supported way to read
    // the buffer (pread returns ESPIPE).
    let map = unsafe { memmap2::MmapOptions::new().len(needed).map(&file) }.map_err(|error| {
        DmaBufError::Unsupported(format!(
            "mmap of fourcc {} modifier {:#018x} failed: {error}",
            fourcc_name(fourcc),
            modifier
        ))
    })?;

    Ok(MappedPlane {
        map,
        file,
        offset,
        stride,
    })
}

fn validate_common(
    width: u32,
    height: u32,
    fourcc: u32,
    modifier: u64,
    max_width: u32,
    max_height: u32,
) -> Result<DmaBufFormat, DmaBufError> {
    if width == 0 || height == 0 {
        return Err(DmaBufError::Geometry(format!(
            "scanout is {width}x{height}; both dimensions must be non-zero"
        )));
    }
    if width > max_width || height > max_height {
        return Err(DmaBufError::Geometry(format!(
            "scanout {width}x{height} exceeds the configured {max_width}x{max_height} limit"
        )));
    }
    let format = DmaBufFormat::from_fourcc(fourcc).ok_or_else(|| {
        DmaBufError::Unsupported(format!(
            "fourcc {} is not a supported linear layout",
            fourcc_name(fourcc)
        ))
    })?;
    if modifier != DRM_FORMAT_MOD_LINEAR {
        return Err(DmaBufError::Unsupported(format!(
            "fourcc {} uses modifier {modifier:#018x}; only DRM_FORMAT_MOD_LINEAR (0) can be mapped",
            fourcc_name(fourcc)
        )));
    }
    Ok(format)
}

/// True when every pixel's RGB channels are zero. Alpha is ignored.
pub(crate) fn bgra_is_all_black(pixels: &[u8]) -> bool {
    pixels
        .chunks_exact(4)
        .all(|pixel| pixel[0] == 0 && pixel[1] == 0 && pixel[2] == 0)
}

impl MappedDmaBuf {
    /// Validate a v1 `ScanoutDMABUF` header and map its single descriptor.
    ///
    /// `max_width`/`max_height` are the capture source's configured limits.
    pub(crate) fn import(
        header: &ScanoutDmabuf,
        fd: OwnedFd,
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, DmaBufError> {
        let format = validate_common(
            header.width,
            header.height,
            header.fourcc,
            header.modifier,
            max_width,
            max_height,
        )?;

        let width = header.width as usize;
        let height = header.height as usize;
        let stride = header.stride as usize;
        let (planes, contiguous_nv12) = if format == DmaBufFormat::Nv12 {
            // The v1 signature has one fd, so NV12 must be Y then UV.
            let chroma_row_bytes = round_up2(width);
            if stride < chroma_row_bytes {
                return Err(DmaBufError::Geometry(format!(
                    "NV12 stride {stride} is smaller than the {chroma_row_bytes}-byte chroma row"
                )));
            }
            let chroma_rows = ceil2(height);
            let plane = map_plane(
                fd,
                0,
                stride,
                height
                    .checked_add(chroma_rows)
                    .ok_or_else(|| DmaBufError::Geometry("NV12 height overflows".to_owned()))?,
                header.fourcc,
                header.modifier,
            )?;
            (vec![plane], true)
        } else if format.plane_count() == 1 {
            let row_bytes = width
                .checked_mul(4)
                .ok_or_else(|| DmaBufError::Geometry("row length overflows".to_owned()))?;
            if stride < row_bytes {
                return Err(DmaBufError::Geometry(format!(
                    "stride {stride} is smaller than a {width}-pixel BGRA row ({row_bytes})"
                )));
            }
            let plane = map_plane(fd, 0, stride, height, header.fourcc, header.modifier)?;
            (vec![plane], false)
        } else {
            return Err(DmaBufError::Unsupported(format!(
                "ScanoutDMABUF cannot describe the {} planes required by fourcc {}; use ScanoutDMABUF2",
                format.plane_count(),
                fourcc_name(header.fourcc)
            )));
        };

        Ok(Self {
            planes,
            width: header.width,
            height: header.height,
            x: 0,
            y: 0,
            backing_width: header.width,
            backing_height: header.height,
            format,
            y0_top: header.y0_top,
            contiguous_nv12,
        })
    }

    /// Validate a multi-plane `ScanoutDMABUF2` header and map every descriptor.
    pub(crate) fn import_v2(
        header: &ScanoutDmabuf2,
        fds: Vec<OwnedFd>,
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, DmaBufError> {
        let format = validate_common(
            header.width,
            header.height,
            header.fourcc,
            header.modifier,
            max_width,
            max_height,
        )?;

        let num_planes = header.num_planes as usize;
        if fds.len() != num_planes {
            return Err(DmaBufError::Protocol(format!(
                "ScanoutDMABUF2 declares {num_planes} planes but the message carried {} descriptors",
                fds.len()
            )));
        }
        if header.offsets.len() != num_planes || header.strides.len() != num_planes {
            return Err(DmaBufError::Protocol(format!(
                "ScanoutDMABUF2 declares {num_planes} planes but has {}/{} offset/stride entries",
                header.offsets.len(),
                header.strides.len()
            )));
        }
        let expected = format.plane_count();
        if num_planes != expected {
            return Err(DmaBufError::Unsupported(format!(
                "fourcc {} requires {expected} planes, but ScanoutDMABUF2 declares {num_planes}",
                fourcc_name(header.fourcc)
            )));
        }

        let backing_width = if header.backing_width == 0 {
            header.width
        } else {
            header.backing_width
        };
        let backing_height = if header.backing_height == 0 {
            header.height
        } else {
            header.backing_height
        };
        let x = header.x as usize;
        let y = header.y as usize;
        let width = header.width as usize;
        let height = header.height as usize;
        let backing_width_px = backing_width as usize;
        let backing_height_px = backing_height as usize;
        let right = x
            .checked_add(width)
            .ok_or_else(|| DmaBufError::Geometry("scanout x extent overflows".to_owned()))?;
        let bottom = y
            .checked_add(height)
            .ok_or_else(|| DmaBufError::Geometry("scanout y extent overflows".to_owned()))?;
        if right > backing_width_px || bottom > backing_height_px {
            return Err(DmaBufError::Geometry(format!(
                "scanout rectangle {x},{y} {width}x{height} exceeds the {backing_width_px}x{backing_height_px} backing buffer"
            )));
        }
        if format.is_yuv() && x % 2 != 0 {
            return Err(DmaBufError::Geometry(format!(
                "YUV scanout x origin {x} is odd; chroma subsampling requires an even origin"
            )));
        }

        let mut planes = Vec::with_capacity(num_planes);
        for (index, fd) in fds.into_iter().enumerate() {
            let offset = header.offsets[index] as usize;
            let stride = header.strides[index] as usize;
            let (rows, row_bytes) = match format {
                DmaBufFormat::Nv12 | DmaBufFormat::Nv21 => {
                    if index == 0 {
                        (backing_height_px, backing_width_px)
                    } else {
                        (ceil2(backing_height_px), round_up2(backing_width_px))
                    }
                }
                DmaBufFormat::Yuv420 | DmaBufFormat::Yvu420 => {
                    if index == 0 {
                        (backing_height_px, backing_width_px)
                    } else {
                        (ceil2(backing_height_px), ceil2(backing_width_px))
                    }
                }
                _ => (
                    backing_height_px,
                    backing_width_px
                        .checked_mul(4)
                        .ok_or_else(|| DmaBufError::Geometry("row length overflows".to_owned()))?,
                ),
            };
            if stride < row_bytes {
                return Err(DmaBufError::Geometry(format!(
                    "plane {index} stride {stride} is smaller than its {row_bytes}-byte row"
                )));
            }
            planes.push(map_plane(
                fd,
                offset,
                stride,
                rows,
                header.fourcc,
                header.modifier,
            )?);
        }

        Ok(Self {
            planes,
            width: header.width,
            height: header.height,
            x: header.x,
            y: header.y,
            backing_width,
            backing_height,
            format,
            y0_top: header.y0_top,
            contiguous_nv12: false,
        })
    }

    /// Width in pixels.
    pub(crate) fn width(&self) -> u32 {
        self.width
    }

    /// Height in pixels.
    pub(crate) fn height(&self) -> u32 {
        self.height
    }

    fn sync(&self, start: bool) -> Result<(), DmaBufError> {
        for plane in &self.planes {
            plane.sync(start)?;
        }
        Ok(())
    }

    /// Copy the current buffer contents into a tightly packed BGRA image.
    pub(crate) fn snapshot_bgra(&self) -> Result<Vec<u8>, DmaBufError> {
        let width = self.width as usize;
        let height = self.height as usize;
        let mut output = vec![0u8; width * height * 4];
        self.copy_bgra_rect(0, 0, self.width, self.height, &mut output, width * 4)?;
        Ok(output)
    }

    /// Copy one display rectangle from the mapping into `destination`.
    ///
    /// `destination` is a full BGRA frame of `self.width()` pixels per row, so
    /// an `UpdateDMABUF` rectangle can be written in place at its display
    /// coordinates. Bounds and strides were validated at import time; this
    /// method still uses checked indexing so a corrupt mapping is a typed
    /// geometry error instead of a panic.
    pub(crate) fn copy_bgra_rect(
        &self,
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        destination: &mut [u8],
        destination_stride: usize,
    ) -> Result<(), DmaBufError> {
        if width == 0 || height == 0 {
            return Err(DmaBufError::Geometry(
                "damage rectangle has a zero dimension".to_owned(),
            ));
        }
        let x = x as usize;
        let y = y as usize;
        let width = width as usize;
        let height = height as usize;
        let right = x
            .checked_add(width)
            .ok_or_else(|| DmaBufError::Geometry("damage x extent overflows".to_owned()))?;
        let bottom = y
            .checked_add(height)
            .ok_or_else(|| DmaBufError::Geometry("damage y extent overflows".to_owned()))?;
        if right > self.width as usize || bottom > self.height as usize {
            return Err(DmaBufError::Geometry(format!(
                "damage rectangle {x},{y} {width}x{height} exceeds the {}x{} scanout",
                self.width, self.height
            )));
        }
        let row_bytes = (self.width as usize)
            .checked_mul(4)
            .ok_or_else(|| DmaBufError::Geometry("frame row length overflows".to_owned()))?;
        if destination_stride < row_bytes {
            return Err(DmaBufError::Geometry(format!(
                "destination stride {destination_stride} is smaller than the {row_bytes}-byte frame row"
            )));
        }
        let required = bottom
            .checked_mul(destination_stride)
            .ok_or_else(|| DmaBufError::Geometry("frame length overflows".to_owned()))?;
        if destination.len() < required {
            return Err(DmaBufError::Geometry(format!(
                "destination has {} bytes; {required} are required for the {width}x{height} damage at {x},{y}",
                destination.len()
            )));
        }

        self.sync(true)?;
        let result = self.copy_inner(x, y, width, height, destination, destination_stride);
        let ended = self.sync(false);
        match result {
            Err(error) => {
                let _ = ended;
                Err(error)
            }
            Ok(()) => ended,
        }
    }

    fn source_row(&self, display_y: usize) -> Result<usize, DmaBufError> {
        let row = (self.y as usize)
            .checked_add(display_y)
            .ok_or_else(|| DmaBufError::Geometry("display row overflows".to_owned()))?;
        if self.y0_top {
            Ok(row)
        } else {
            (self.backing_height as usize)
                .checked_sub(1 + row)
                .ok_or_else(|| {
                    DmaBufError::Geometry(format!(
                        "display row {row} cannot be flipped inside a {}-row backing buffer",
                        self.backing_height
                    ))
                })
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn copy_inner(
        &self,
        x: usize,
        y: usize,
        width: usize,
        height: usize,
        destination: &mut [u8],
        destination_stride: usize,
    ) -> Result<(), DmaBufError> {
        for row in 0..height {
            let display_y = y
                .checked_add(row)
                .ok_or_else(|| DmaBufError::Geometry("display row overflows".to_owned()))?;
            let source_y = self.source_row(display_y)?;
            let source_x = (self.x as usize)
                .checked_add(x)
                .ok_or_else(|| DmaBufError::Geometry("display column overflows".to_owned()))?;
            let destination_start = display_y
                .checked_mul(destination_stride)
                .and_then(|base| base.checked_add(x * 4))
                .ok_or_else(|| DmaBufError::Geometry("destination offset overflows".to_owned()))?;
            let destination_end = destination_start
                .checked_add(width * 4)
                .ok_or_else(|| DmaBufError::Geometry("destination row overflows".to_owned()))?;
            let destination_row = destination
                .get_mut(destination_start..destination_end)
                .ok_or_else(|| {
                    DmaBufError::Geometry("destination row is outside the frame".to_owned())
                })?;

            match self.format {
                DmaBufFormat::Nv12 => {
                    self.copy_nv_row(source_y, source_x, width, destination_row, false)?
                }
                DmaBufFormat::Nv21 => {
                    self.copy_nv_row(source_y, source_x, width, destination_row, true)?
                }
                DmaBufFormat::Yuv420 => {
                    self.copy_planar_row(source_y, source_x, width, destination_row, false)?
                }
                DmaBufFormat::Yvu420 => {
                    self.copy_planar_row(source_y, source_x, width, destination_row, true)?
                }
                _ => self.copy_packed_row(source_y, source_x, width, destination_row)?,
            }
        }
        Ok(())
    }

    fn row_slice<'a>(
        &self,
        plane: &'a MappedPlane,
        row: usize,
        column: usize,
        bytes: usize,
    ) -> Result<&'a [u8], DmaBufError> {
        let start = plane
            .offset
            .checked_add(
                row.checked_mul(plane.stride)
                    .ok_or_else(|| DmaBufError::Geometry("plane row overflows".to_owned()))?,
            )
            .and_then(|base| base.checked_add(column))
            .ok_or_else(|| DmaBufError::Geometry("plane offset overflows".to_owned()))?;
        let end = start
            .checked_add(bytes)
            .ok_or_else(|| DmaBufError::Geometry("plane row overflows".to_owned()))?;
        plane.map.get(start..end).ok_or_else(|| {
            DmaBufError::Geometry(format!(
                "plane row {row} at column {column} needs bytes {start}..{end}, but the mapping is {} bytes",
                plane.map.len()
            ))
        })
    }

    fn copy_packed_row(
        &self,
        source_y: usize,
        x: usize,
        width: usize,
        destination: &mut [u8],
    ) -> Result<(), DmaBufError> {
        let plane = &self.planes[0];
        let source = self.row_slice(plane, source_y, x * 4, width * 4)?;
        self.format.convert_row(source, destination, width);
        Ok(())
    }

    fn copy_nv_row(
        &self,
        source_y: usize,
        x: usize,
        width: usize,
        destination: &mut [u8],
        swap_uv: bool,
    ) -> Result<(), DmaBufError> {
        let luma = &self.planes[0];
        let luma_row = self.row_slice(luma, source_y, x, width)?;
        let (chroma_map, chroma_offset, chroma_stride) = if self.contiguous_nv12 {
            // v1 contiguous NV12: UV starts after all luma rows in the same fd.
            let offset = luma
                .offset
                .checked_add(luma.stride * self.backing_height as usize)
                .ok_or_else(|| DmaBufError::Geometry("NV12 chroma offset overflows".to_owned()))?;
            (&luma.map, offset, luma.stride)
        } else {
            let chroma = &self.planes[1];
            (&chroma.map, chroma.offset, chroma.stride)
        };
        let chroma_row = source_y / 2;
        let chroma_start_pair = x / 2;
        let chroma_column = chroma_start_pair * 2;
        let chroma_span = round_up2((x % 2) + width);
        let start = chroma_offset
            .checked_add(
                chroma_row
                    .checked_mul(chroma_stride)
                    .ok_or_else(|| DmaBufError::Geometry("chroma row overflows".to_owned()))?,
            )
            .and_then(|base| base.checked_add(chroma_column))
            .ok_or_else(|| DmaBufError::Geometry("chroma offset overflows".to_owned()))?;
        let end = start
            .checked_add(chroma_span)
            .ok_or_else(|| DmaBufError::Geometry("chroma row overflows".to_owned()))?;
        let chroma = chroma_map.get(start..end).ok_or_else(|| {
            DmaBufError::Geometry(format!(
                "NV12 chroma row {chroma_row} needs bytes {start}..{end}, but the mapping is {} bytes",
                chroma_map.len()
            ))
        })?;

        for column in 0..width {
            let pair = ((x + column) / 2) - chroma_start_pair;
            let uv = pair * 2;
            let (u, v) = if swap_uv {
                (chroma[uv + 1] as i32, chroma[uv] as i32)
            } else {
                (chroma[uv] as i32, chroma[uv + 1] as i32)
            };
            let pixel = yuv_to_bgra(luma_row[column] as i32, u, v);
            destination[column * 4..column * 4 + 4].copy_from_slice(&pixel);
        }
        Ok(())
    }

    fn copy_planar_row(
        &self,
        source_y: usize,
        x: usize,
        width: usize,
        destination: &mut [u8],
        v_first: bool,
    ) -> Result<(), DmaBufError> {
        let luma_row = self.row_slice(&self.planes[0], source_y, x, width)?;
        let chroma_row = source_y / 2;
        let chroma_start = x / 2;
        let chroma_span = round_up2((x % 2) + width) / 2;
        let (u_plane, v_plane) = if v_first {
            (&self.planes[2], &self.planes[1])
        } else {
            (&self.planes[1], &self.planes[2])
        };
        let u_row = self.row_slice(u_plane, chroma_row, chroma_start, chroma_span)?;
        let v_row = self.row_slice(v_plane, chroma_row, chroma_start, chroma_span)?;

        for column in 0..width {
            let chroma = ((x + column) / 2) - chroma_start;
            let pixel = yuv_to_bgra(
                luma_row[column] as i32,
                u_row[chroma] as i32,
                v_row[chroma] as i32,
            );
            destination[column * 4..column * 4 + 4].copy_from_slice(&pixel);
        }
        Ok(())
    }
}

/// Adopt descriptors received through `SCM_RIGHTS`.
///
/// SAFETY: each `RawFd` was just returned by the kernel in a control message
/// and is a valid, owned descriptor for this process. `MSG_CMSG_CLOEXEC` makes
/// them close-on-exec. Each descriptor is converted exactly once and then
/// closed by its [`OwnedFd`] drop; callers must not use the raw value again.
pub(crate) fn adopt_received_fds(raw: Vec<RawFd>) -> Vec<OwnedFd> {
    raw.into_iter()
        .map(|fd| unsafe { OwnedFd::from_raw_fd(fd) })
        .collect()
}

#[cfg(test)]
mod tests {
    use std::io::{Seek, SeekFrom, Write};
    use std::os::fd::{AsRawFd, IntoRawFd};

    use super::*;

    fn scanout_body(
        fd_index: u32,
        width: u32,
        height: u32,
        stride: u32,
        fourcc: u32,
        modifier: u64,
        y0_top: bool,
    ) -> Vec<u8> {
        let mut body = Vec::new();
        body.extend_from_slice(&fd_index.to_le_bytes());
        body.extend_from_slice(&width.to_le_bytes());
        body.extend_from_slice(&height.to_le_bytes());
        body.extend_from_slice(&stride.to_le_bytes());
        body.extend_from_slice(&fourcc.to_le_bytes());
        body.extend_from_slice(&[0; 4]);
        body.extend_from_slice(&modifier.to_le_bytes());
        body.extend_from_slice(&u32::from(y0_top).to_le_bytes());
        body
    }

    #[allow(clippy::too_many_arguments)]
    fn scanout2_body(
        fd_indexes: &[u32],
        x: u32,
        y: u32,
        width: u32,
        height: u32,
        offsets: &[u32],
        strides: &[u32],
        fourcc: u32,
        backing_width: u32,
        backing_height: u32,
        modifier: u64,
        y0_top: bool,
    ) -> Vec<u8> {
        assert_eq!(fd_indexes.len(), offsets.len());
        assert_eq!(fd_indexes.len(), strides.len());
        let mut body = Vec::new();
        body.extend_from_slice(&u32::try_from(fd_indexes.len() * 4).unwrap().to_le_bytes());
        for index in fd_indexes {
            body.extend_from_slice(&index.to_le_bytes());
        }
        for value in [x, y, width, height] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        body.extend_from_slice(&u32::try_from(offsets.len() * 4).unwrap().to_le_bytes());
        for offset in offsets {
            body.extend_from_slice(&offset.to_le_bytes());
        }
        body.extend_from_slice(&u32::try_from(strides.len() * 4).unwrap().to_le_bytes());
        for stride in strides {
            body.extend_from_slice(&stride.to_le_bytes());
        }
        for value in [
            u32::try_from(fd_indexes.len()).unwrap(),
            fourcc,
            backing_width,
            backing_height,
        ] {
            body.extend_from_slice(&value.to_le_bytes());
        }
        while body.len() % 8 != 0 {
            body.push(0);
        }
        body.extend_from_slice(&modifier.to_le_bytes());
        body.extend_from_slice(&u32::from(y0_top).to_le_bytes());
        body
    }

    fn temp_fd(pixels: &[u8]) -> OwnedFd {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(pixels).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let fd: OwnedFd = file.into();
        fd
    }

    /// Identify an open descriptor without retaining or closing it.
    fn fd_identity(fd: RawFd) -> (u64, u64) {
        use std::os::unix::fs::MetadataExt;
        let metadata = std::fs::metadata(format!("/proc/self/fd/{fd}")).unwrap();
        (metadata.dev(), metadata.ino())
    }

    /// True when `fd` no longer refers to the inode captured by
    /// [`fd_identity`]. A number reused by another test has a different inode,
    /// so this is safe under the parallel test runner and never closes
    /// somebody else's descriptor.
    fn fd_is_closed(fd: RawFd, identity: (u64, u64)) -> bool {
        use std::os::unix::fs::MetadataExt;
        match std::fs::metadata(format!("/proc/self/fd/{fd}")) {
            Err(_) => true,
            Ok(metadata) => (metadata.dev(), metadata.ino()) != identity,
        }
    }

    fn import(
        body: &[u8],
        fd: OwnedFd,
        max_width: u32,
        max_height: u32,
    ) -> Result<MappedDmaBuf, DmaBufError> {
        let header = parse_scanout_dmabuf(body)?;
        MappedDmaBuf::import(&header, fd, max_width, max_height)
    }

    fn import_v2(
        body: &[u8],
        fds: Vec<OwnedFd>,
        max_width: u32,
        max_height: u32,
    ) -> Result<MappedDmaBuf, DmaBufError> {
        let header = parse_scanout_dmabuf2(body)?;
        MappedDmaBuf::import_v2(&header, fds, max_width, max_height)
    }

    #[test]
    fn parses_the_live_huuuutb_layout() {
        // Bytes captured live from QEMU 11.1.3: 1280x800, stride 5120,
        // DRM_FORMAT_XBGR8888, linear, y0_top=false.
        let body = scanout_body(0, 1280, 800, 5120, DRM_FORMAT_XBGR8888, 0, false);
        let header = parse_scanout_dmabuf(&body).unwrap();
        assert_eq!(
            header,
            ScanoutDmabuf {
                fd_index: 0,
                width: 1280,
                height: 800,
                stride: 5120,
                fourcc: DRM_FORMAT_XBGR8888,
                modifier: 0,
                y0_top: false,
            }
        );
        assert_eq!(fourcc_name(header.fourcc), "'XB24' (0x34324258)");
    }

    #[test]
    fn parses_the_multi_plane_scanout_dmabuf2_layout() {
        let body = scanout2_body(
            &[0, 1],
            0,
            0,
            1280,
            800,
            &[0, 0],
            &[1280, 1280],
            DRM_FORMAT_NV12,
            1280,
            800,
            0,
            true,
        );
        let header = parse_scanout_dmabuf2(&body).unwrap();
        assert_eq!(header.fd_indexes, vec![0, 1]);
        assert_eq!((header.x, header.y), (0, 0));
        assert_eq!((header.width, header.height), (1280, 800));
        assert_eq!(header.offsets, vec![0, 0]);
        assert_eq!(header.strides, vec![1280, 1280]);
        assert_eq!(header.num_planes, 2);
        assert_eq!(header.fourcc, DRM_FORMAT_NV12);
        assert_eq!((header.backing_width, header.backing_height), (1280, 800));
        assert_eq!(header.modifier, 0);
        assert!(header.y0_top);
    }

    #[test]
    fn scanout_dmabuf2_rejects_mismatched_tables() {
        let mut body = scanout2_body(
            &[0, 1],
            0,
            0,
            2,
            2,
            &[0, 0],
            &[2, 2],
            DRM_FORMAT_NV12,
            2,
            2,
            0,
            true,
        );
        // Overwrite num_planes (after the two handle/offset/stride arrays) with
        // 3 while the arrays still describe two planes.
        let num_planes_at = 4 + 4 * 2 + 16 + (4 + 4 * 2) + (4 + 4 * 2);
        body[num_planes_at..num_planes_at + 4].copy_from_slice(&3u32.to_le_bytes());
        assert!(matches!(
            parse_scanout_dmabuf2(&body),
            Err(DmaBufError::Protocol(_))
        ));
    }

    #[test]
    fn malformed_bodies_are_typed() {
        assert!(matches!(
            parse_scanout_dmabuf(&[0; 35]),
            Err(DmaBufError::Protocol(_))
        ));
        let mut body = scanout_body(0, 2, 2, 8, DRM_FORMAT_XBGR8888, 0, false);
        body[32..36].copy_from_slice(&2u32.to_le_bytes());
        assert!(matches!(
            parse_scanout_dmabuf(&body),
            Err(DmaBufError::Protocol(_))
        ));
        assert!(matches!(
            parse_update_dmabuf(&[0; 12]),
            Err(DmaBufError::Protocol(_))
        ));
        assert!(matches!(
            parse_scanout_dmabuf2(&[0; 20]),
            Err(DmaBufError::Protocol(_))
        ));
    }

    #[test]
    fn converts_abgr_to_bgra_and_flips_bottom_up_rows() {
        // Two rows of R,G,B,A pixels; y0_top=false means the last row is the
        // top of the image.
        let pixels = vec![
            1, 2, 3, 4, // bottom row, first pixel
            5, 6, 7, 8, // bottom row, second pixel
            9, 10, 11, 12, // top row, first pixel
            13, 14, 15, 16, // top row, second pixel
        ];
        let body = scanout_body(0, 2, 2, 8, DRM_FORMAT_ABGR8888, 0, false);
        let mapped = import(&body, temp_fd(&pixels), 64, 64).unwrap();
        let bgra = mapped.snapshot_bgra().unwrap();
        assert_eq!(
            bgra,
            vec![
                11, 10, 9, 12, 15, 14, 13, 16, // top source row first
                3, 2, 1, 4, 7, 6, 5, 8,
            ]
        );
    }

    #[test]
    fn converts_argb_to_bgra_without_a_swap() {
        // DRM_FORMAT_ARGB8888 is B,G,R,A in little-endian memory.
        let pixels = vec![1, 2, 3, 4, 5, 6, 7, 8];
        let body = scanout_body(0, 2, 1, 8, DRM_FORMAT_ARGB8888, 0, true);
        let mapped = import(&body, temp_fd(&pixels), 64, 64).unwrap();
        assert_eq!(mapped.snapshot_bgra().unwrap(), pixels);

        // XRGB has no alpha; the output is opaque.
        let body = scanout_body(0, 1, 1, 4, DRM_FORMAT_XRGB8888, 0, true);
        let mapped = import(&body, temp_fd(&[1, 2, 3, 0]), 64, 64).unwrap();
        assert_eq!(mapped.snapshot_bgra().unwrap(), vec![1, 2, 3, 255]);
    }

    #[test]
    fn rejects_tiled_modifiers_with_the_modifier_and_format() {
        let body = scanout_body(0, 2, 2, 8, DRM_FORMAT_XBGR8888, 0x0100_0000_0000_0001, true);
        let error = import(&body, temp_fd(&[0; 16]), 64, 64).unwrap_err();
        let message = error.to_string();
        assert!(matches!(error, DmaBufError::Unsupported(_)));
        assert!(message.contains("0x0100000000000001"), "{message}");
        assert!(message.contains("'XB24'"), "{message}");
    }

    #[test]
    fn rejects_tiled_scanout_dmabuf2_modifiers_with_the_modifier_and_format() {
        let body = scanout2_body(
            &[0],
            0,
            0,
            2,
            2,
            &[0],
            &[8],
            DRM_FORMAT_XBGR8888,
            2,
            2,
            0x0200_0000_0000_0004,
            true,
        );
        let error = import_v2(&body, vec![temp_fd(&[0; 16])], 64, 64).unwrap_err();
        let message = error.to_string();
        assert!(matches!(error, DmaBufError::Unsupported(_)));
        assert!(message.contains("0x0200000000000004"), "{message}");
        assert!(message.contains("'XB24'"), "{message}");
    }

    #[test]
    fn rejects_unknown_fourcc_and_bad_geometry() {
        let body = scanout_body(0, 2, 2, 8, fourcc(b'J', b'P', b'E', b'G'), 0, true);
        let error = import(&body, temp_fd(&[0; 16]), 64, 64).unwrap_err();
        assert!(matches!(error, DmaBufError::Unsupported(_)));
        assert!(error.to_string().contains("'JPEG'"));

        let body = scanout_body(0, 0, 2, 8, DRM_FORMAT_XBGR8888, 0, true);
        assert!(matches!(
            import(&body, temp_fd(&[0; 16]), 64, 64),
            Err(DmaBufError::Geometry(_))
        ));

        let body = scanout_body(0, 2, 2, 7, DRM_FORMAT_XBGR8888, 0, true);
        assert!(matches!(
            import(&body, temp_fd(&[0; 16]), 64, 64),
            Err(DmaBufError::Geometry(_))
        ));

        let body = scanout_body(0, 2, 2, 8, DRM_FORMAT_XBGR8888, 0, true);
        assert!(matches!(
            import(&body, temp_fd(&[0; 8]), 64, 64),
            Err(DmaBufError::Geometry(_))
        ));

        let body = scanout_body(0, 8, 8, 32, DRM_FORMAT_XBGR8888, 0, true);
        assert!(matches!(
            import(&body, temp_fd(&[0; 256]), 4, 4),
            Err(DmaBufError::Geometry(_))
        ));
    }

    #[test]
    fn converts_contiguous_nv12_to_bgra() {
        // A 2x2 NV12 frame: four Y bytes followed by one U,V pair.
        // Y=16 (black), U=128, V=128 -> limited-range BT.601 black.
        let body = scanout_body(0, 2, 2, 2, DRM_FORMAT_NV12, 0, true);
        let mapped = import(&body, temp_fd(&[16, 16, 16, 16, 128, 128]), 64, 64).unwrap();
        assert_eq!(
            mapped.snapshot_bgra().unwrap(),
            vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );

        // Y=235, U=V=128 is limited-range white.
        let body = scanout_body(0, 2, 2, 2, DRM_FORMAT_NV12, 0, true);
        let mapped = import(&body, temp_fd(&[235, 235, 235, 235, 128, 128]), 64, 64).unwrap();
        assert_eq!(
            mapped.snapshot_bgra().unwrap(),
            vec![255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn converts_two_plane_nv12_with_offsets_and_strides() {
        // 2x2 luma plane with a 4-byte offset and 4-byte stride, then a
        // separate chroma plane with its own offset and stride.
        let mut luma = vec![0u8; 12];
        luma[4..8].copy_from_slice(&[16, 16, 16, 16]);
        let mut chroma = vec![0u8; 8];
        chroma[2..4].copy_from_slice(&[128, 128]);
        let body = scanout2_body(
            &[0, 1],
            0,
            0,
            2,
            2,
            &[4, 2],
            &[4, 4],
            DRM_FORMAT_NV12,
            2,
            2,
            0,
            true,
        );
        let mapped = import_v2(&body, vec![temp_fd(&luma), temp_fd(&chroma)], 64, 64).unwrap();
        assert_eq!(
            mapped.snapshot_bgra().unwrap(),
            vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );
    }

    #[test]
    fn scanout_dmabuf2_imports_a_sub_rectangle_with_packed_pixels() {
        // Backing 4x2 XBGR. The scanout is the 2x1 rectangle at x=1, y=1.
        let pixels: Vec<u8> = (0..32).map(|value| value as u8).collect();
        let body = scanout2_body(
            &[0],
            1,
            1,
            2,
            1,
            &[0],
            &[16],
            DRM_FORMAT_XBGR8888,
            4,
            2,
            0,
            true,
        );
        let mapped = import_v2(&body, vec![temp_fd(&pixels)], 64, 64).unwrap();
        assert_eq!(
            mapped.snapshot_bgra().unwrap(),
            vec![22, 21, 20, 255, 26, 25, 24, 255]
        );
    }

    #[test]
    fn damage_copy_only_changes_the_requested_rectangle() {
        // 3x2 ARGB (memory B,G,R,A), two damage rows at x=1.
        let pixels: Vec<u8> = (0..24).map(|value| value as u8).collect();
        let body = scanout_body(0, 3, 2, 12, DRM_FORMAT_ARGB8888, 0, true);
        let mapped = import(&body, temp_fd(&pixels), 64, 64).unwrap();
        let mut frame = vec![0xaa; 3 * 2 * 4];
        mapped
            .copy_bgra_rect(1, 0, 2, 2, &mut frame, 3 * 4)
            .unwrap();
        assert_eq!(
            frame,
            vec![
                0xaa, 0xaa, 0xaa, 0xaa, 4, 5, 6, 7, 8, 9, 10, 11, // row 0
                0xaa, 0xaa, 0xaa, 0xaa, 16, 17, 18, 19, 20, 21, 22, 23, // row 1
            ]
        );
    }

    #[test]
    fn scanout_dmabuf2_rejects_bad_plane_tables() {
        // NV12 needs two planes, not one.
        let body = scanout2_body(&[0], 0, 0, 2, 2, &[0], &[2], DRM_FORMAT_NV12, 2, 2, 0, true);
        let error = import_v2(&body, vec![temp_fd(&[0; 8])], 64, 64).unwrap_err();
        assert!(matches!(error, DmaBufError::Unsupported(_)));

        // The rectangle must fit inside the backing buffer.
        let body = scanout2_body(
            &[0],
            1,
            0,
            2,
            2,
            &[0],
            &[8],
            DRM_FORMAT_XBGR8888,
            2,
            2,
            0,
            true,
        );
        assert!(matches!(
            import_v2(&body, vec![temp_fd(&[0; 16])], 64, 64),
            Err(DmaBufError::Geometry(_))
        ));

        // A plane that is shorter than offset + stride * rows is rejected.
        let body = scanout2_body(
            &[0],
            0,
            0,
            2,
            2,
            &[8],
            &[8],
            DRM_FORMAT_XBGR8888,
            2,
            2,
            0,
            true,
        );
        assert!(matches!(
            import_v2(&body, vec![temp_fd(&[0; 16])], 64, 64),
            Err(DmaBufError::Geometry(_))
        ));
    }

    #[test]
    fn a_failed_import_closes_the_descriptor() {
        // A pipe cannot be mmap'd, so `import` must fail and drop the fd.
        let (read, write) = nix::unistd::pipe().unwrap();
        let raw = read.as_raw_fd();
        let identity = fd_identity(raw);
        let body = scanout_body(0, 2, 2, 8, DRM_FORMAT_XBGR8888, 0, true);
        let error = import(&body, read, 64, 64).unwrap_err();
        assert!(matches!(error, DmaBufError::Unsupported(_)));
        assert!(
            fd_is_closed(raw, identity),
            "descriptor {raw} was not closed"
        );
        drop(write);
    }

    #[test]
    fn a_failed_multi_plane_import_closes_every_descriptor() {
        let (read, write) = nix::unistd::pipe().unwrap();
        let file = tempfile::tempfile().unwrap();
        let body = scanout2_body(
            &[0, 1],
            0,
            0,
            2,
            2,
            &[0, 0],
            &[2, 2],
            DRM_FORMAT_NV12,
            2,
            2,
            0,
            true,
        );
        let error = import_v2(&body, vec![read, file.into()], 64, 64).unwrap_err();
        assert!(matches!(error, DmaBufError::Unsupported(_)));
        drop(write);
    }

    #[test]
    fn a_successful_import_closes_the_descriptor_when_dropped() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(16).unwrap();
        let raw = file.as_raw_fd();
        let identity = fd_identity(raw);
        let fd: OwnedFd = file.into();
        let body = scanout_body(0, 2, 2, 8, DRM_FORMAT_XBGR8888, 0, true);
        let mapped = import(&body, fd, 64, 64).unwrap();
        assert_eq!(mapped.snapshot_bgra().unwrap().len(), 16);
        drop(mapped);
        assert!(
            fd_is_closed(raw, identity),
            "descriptor {raw} was not closed"
        );
    }

    #[test]
    fn a_successful_multi_plane_import_closes_all_descriptors() {
        let luma = tempfile::tempfile().unwrap();
        luma.set_len(8).unwrap();
        let chroma = tempfile::tempfile().unwrap();
        chroma.set_len(4).unwrap();
        let body = scanout2_body(
            &[0, 1],
            0,
            0,
            2,
            2,
            &[0, 0],
            &[2, 2],
            DRM_FORMAT_NV12,
            2,
            2,
            0,
            true,
        );
        let mapped = import_v2(&body, vec![luma.into(), chroma.into()], 64, 64).unwrap();
        assert_eq!(mapped.snapshot_bgra().unwrap().len(), 16);
        drop(mapped);
    }

    #[test]
    fn adopted_fds_are_owned_and_closed() {
        let (read, _write) = nix::unistd::pipe().unwrap();
        let raw = read.into_raw_fd();
        let identity = fd_identity(raw);
        let adopted = adopt_received_fds(vec![raw]);
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].as_raw_fd(), raw);
        drop(adopted);
        assert!(fd_is_closed(raw, identity));
    }

    #[test]
    fn all_black_detection_ignores_alpha() {
        assert!(bgra_is_all_black(&[0, 0, 0, 0, 0, 0, 0, 255]));
        assert!(!bgra_is_all_black(&[0, 0, 0, 0, 1, 0, 0, 255]));
    }
}
