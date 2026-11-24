// SPDX-License-Identifier: AGPL-3.0-only
//! QEMU D-Bus `ScanoutDMABUF` import.
//!
//! QEMU's `-display dbus,gl=on` listener does not send the pixman
//! `Scanout`/`Update` byte arrays. It sends:
//!
//! ```text
//! ScanoutDMABUF(h dmabuf, u width, u height, u stride, u fourcc,
//!               t modifier, b y0_top)          signature "huuuutb"
//! UpdateDMABUF(i x, i y, i w, i h)             signature "iiii"
//! ```
//!
//! `ScanoutDMABUF` carries a DMA-BUF descriptor in the message's SCM_RIGHTS
//! list. `UpdateDMABUF` carries only a damage rectangle: it refers to the most
//! recent scanout buffer, whose contents the GPU has already updated. This
//! module validates the header, maps the descriptor read-only, and copies it
//! into the tightly packed BGRA layout the rest of the capture pipeline uses.
//!
//! ## Supported layouts
//!
//! Only `DRM_FORMAT_MOD_LINEAR` (`0`) is mappable without a format-specific
//! decoder. The packed 32-bit formats below are converted; `NV12` is accepted
//! only in the single-fd, Y-then-UV contiguous layout that the v1
//! `ScanoutDMABUF` signature can describe. A tiled or compressed modifier is
//! rejected with a typed error naming the modifier and fourcc rather than
//! being interpreted as linear pixels.
//!
//! ## The isolated `unsafe` operations
//!
//! `memmap2`'s mapping constructor is `unsafe` because a regular file can be
//! truncated while mapped. A DMA-BUF's size is fixed for the lifetime of its
//! export, this mapping is read-only, and the [`File`] (the only owner of the
//! descriptor) is held for the mapping's whole lifetime. `OwnedFd::from_raw_fd`
//! is also isolated here: the descriptors come directly from `SCM_RIGHTS`,
//! have `MSG_CMSG_CLOEXEC` set by the receiver, and are adopted exactly once.

use std::fs::File;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};

use memmap2::Mmap;

/// `DRM_FORMAT_MOD_LINEAR`: the only modifier this importer can interpret.
pub(crate) const DRM_FORMAT_MOD_LINEAR: u64 = 0;

/// Upper bound on a mapped DMABUF, matching the D-Bus body bound.
const MAX_DMABUF_BYTES: usize = 512 * 1024 * 1024;

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

/// Typed failures from DMA-BUF header parsing and import.
#[derive(Debug, thiserror::Error)]
pub(crate) enum DmaBufError {
    /// The message body did not match the `huuuutb` layout.
    #[error("malformed ScanoutDMABUF: {0}")]
    Protocol(String),
    /// The format, modifier, or mapping is not supported.
    #[error("unsupported DMA-BUF scanout: {0}")]
    Unsupported(String),
    /// The declared dimensions or buffer size are invalid.
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
            _ => None,
        }
    }

    fn is_nv12(self) -> bool {
        self == Self::Nv12
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
                Self::Nv12 => unreachable!("NV12 is converted plane-wise"),
            }
        }
    }
}

fn clip(value: i32) -> u8 {
    value.clamp(0, 255) as u8
}

/// A validated, read-only mapping of the current QEMU scanout DMA-BUF.
pub(crate) struct MappedDmaBuf {
    /// Kept first so the mapping is unmapped before the descriptor is closed.
    map: Mmap,
    /// Sole owner of the received descriptor.
    _file: File,
    width: u32,
    height: u32,
    stride: usize,
    format: DmaBufFormat,
    y0_top: bool,
}

impl std::fmt::Debug for MappedDmaBuf {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MappedDmaBuf")
            .field("width", &self.width)
            .field("height", &self.height)
            .field("stride", &self.stride)
            .field("format", &self.format)
            .field("y0_top", &self.y0_top)
            .finish_non_exhaustive()
    }
}

impl MappedDmaBuf {
    /// Validate `header` and map `fd` read-only.
    ///
    /// `max_width`/`max_height` are the capture source's configured limits.
    pub(crate) fn import(
        header: &ScanoutDmabuf,
        fd: OwnedFd,
        max_width: u32,
        max_height: u32,
    ) -> Result<Self, DmaBufError> {
        if header.width == 0 || header.height == 0 {
            return Err(DmaBufError::Geometry(format!(
                "scanout is {}x{}; both dimensions must be non-zero",
                header.width, header.height
            )));
        }
        if header.width > max_width || header.height > max_height {
            return Err(DmaBufError::Geometry(format!(
                "scanout {}x{} exceeds the configured {max_width}x{max_height} limit",
                header.width, header.height
            )));
        }

        let format = DmaBufFormat::from_fourcc(header.fourcc).ok_or_else(|| {
            DmaBufError::Unsupported(format!(
                "fourcc {} is not a supported linear layout",
                fourcc_name(header.fourcc)
            ))
        })?;
        if header.modifier != DRM_FORMAT_MOD_LINEAR {
            return Err(DmaBufError::Unsupported(format!(
                "fourcc {} uses modifier {:#018x}; only DRM_FORMAT_MOD_LINEAR (0) can be mapped",
                fourcc_name(header.fourcc),
                header.modifier
            )));
        }

        let stride = header.stride as usize;
        let width = header.width as usize;
        let height = header.height as usize;
        let needed = if format.is_nv12() {
            let chroma_row_bytes = (width + 1) & !1;
            if stride < chroma_row_bytes {
                return Err(DmaBufError::Geometry(format!(
                    "NV12 stride {stride} is smaller than the {chroma_row_bytes}-byte chroma row"
                )));
            }
            let luma = stride
                .checked_mul(height)
                .ok_or_else(|| DmaBufError::Geometry("NV12 luma length overflows".to_owned()))?;
            let chroma = stride
                .checked_mul((height + 1) / 2)
                .ok_or_else(|| DmaBufError::Geometry("NV12 chroma length overflows".to_owned()))?;
            luma.checked_add(chroma)
                .ok_or_else(|| DmaBufError::Geometry("NV12 buffer length overflows".to_owned()))?
        } else {
            let row_bytes = width
                .checked_mul(4)
                .ok_or_else(|| DmaBufError::Geometry("row length overflows".to_owned()))?;
            if stride < row_bytes {
                return Err(DmaBufError::Geometry(format!(
                    "stride {stride} is smaller than a {width}-pixel BGRA row ({row_bytes})"
                )));
            }
            stride
                .checked_mul(height)
                .ok_or_else(|| DmaBufError::Geometry("buffer length overflows".to_owned()))?
        };
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
        // mapping is read-only, and `file` is moved into the returned struct so
        // the descriptor cannot be closed or replaced before `map` is dropped.
        // The kernel's DMA-BUF mmap implementation is the only supported way to
        // read the buffer (pread returns ESPIPE).
        let map =
            unsafe { memmap2::MmapOptions::new().len(needed).map(&file) }.map_err(|error| {
                DmaBufError::Unsupported(format!(
                    "mmap of fourcc {} modifier {:#018x} failed: {error}",
                    fourcc_name(header.fourcc),
                    header.modifier
                ))
            })?;

        Ok(Self {
            map,
            _file: file,
            width: header.width,
            height: header.height,
            stride,
            format,
            y0_top: header.y0_top,
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

    /// Copy the current buffer contents into a tightly packed BGRA image.
    ///
    /// The copy is a best-effort snapshot: the GPU may still be writing the
    /// most recent damage while the reader copies. QEMU flushes before sending
    /// `UpdateDMABUF`; on coherent hosts (x86) the copy observes that data.
    pub(crate) fn snapshot_bgra(&self) -> Vec<u8> {
        let width = self.width as usize;
        let height = self.height as usize;
        let row_bytes = width * 4;
        let mut output = vec![0u8; row_bytes * height];

        if self.format.is_nv12() {
            self.copy_nv12(&mut output, width, height);
            return output;
        }

        for output_row in 0..height {
            let source_row = if self.y0_top {
                output_row
            } else {
                height - 1 - output_row
            };
            let source = &self.map[source_row * self.stride..];
            let destination = &mut output[output_row * row_bytes..(output_row + 1) * row_bytes];
            self.format.convert_row(source, destination, width);
        }
        output
    }

    fn copy_nv12(&self, output: &mut [u8], width: usize, height: usize) {
        let uv_offset = self.stride * height;
        let uv_stride = self.stride;
        for output_row in 0..height {
            let source_row = if self.y0_top {
                output_row
            } else {
                height - 1 - output_row
            };
            let luma_row = source_row * self.stride;
            let chroma_row = uv_offset + (source_row / 2) * uv_stride;
            let destination_row = output_row * width * 4;
            for x in 0..width {
                let y = self.map[luma_row + x] as i32;
                let uv = chroma_row + (x / 2) * 2;
                let u = self.map[uv] as i32;
                let v = self.map[uv + 1] as i32;
                let c = y - 16;
                let d = u - 128;
                let e = v - 128;
                let r = clip((298 * c + 409 * e + 128) >> 8);
                let g = clip((298 * c - 100 * d - 208 * e + 128) >> 8);
                let b = clip((298 * c + 516 * d + 128) >> 8);
                let pixel = destination_row + x * 4;
                output[pixel] = b;
                output[pixel + 1] = g;
                output[pixel + 2] = r;
                output[pixel + 3] = 0xff;
            }
        }
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

    fn temp_fd(pixels: &[u8]) -> OwnedFd {
        let mut file = tempfile::tempfile().unwrap();
        file.write_all(pixels).unwrap();
        file.seek(SeekFrom::Start(0)).unwrap();
        let fd: OwnedFd = file.into();
        fd
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
        let bgra = mapped.snapshot_bgra();
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
        assert_eq!(mapped.snapshot_bgra(), pixels);

        // XRGB has no alpha; the output is opaque.
        let body = scanout_body(0, 1, 1, 4, DRM_FORMAT_XRGB8888, 0, true);
        let mapped = import(&body, temp_fd(&[1, 2, 3, 0]), 64, 64).unwrap();
        assert_eq!(mapped.snapshot_bgra(), vec![1, 2, 3, 255]);
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
            mapped.snapshot_bgra(),
            vec![0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255]
        );

        // Y=235, U=V=128 is limited-range white.
        let body = scanout_body(0, 2, 2, 2, DRM_FORMAT_NV12, 0, true);
        let mapped = import(&body, temp_fd(&[235, 235, 235, 235, 128, 128]), 64, 64).unwrap();
        assert_eq!(
            mapped.snapshot_bgra(),
            vec![255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255, 255]
        );
    }

    #[test]
    fn a_failed_import_closes_the_descriptor() {
        // A pipe cannot be mmap'd, so `import` must fail and drop the fd.
        let (read, write) = nix::unistd::pipe().unwrap();
        let raw = read.as_raw_fd();
        let body = scanout_body(0, 2, 2, 8, DRM_FORMAT_XBGR8888, 0, true);
        let error = import(&body, read, 64, 64).unwrap_err();
        assert!(matches!(error, DmaBufError::Unsupported(_)));
        assert!(
            nix::unistd::close(raw).is_err(),
            "descriptor {raw} was not closed"
        );
        drop(write);
    }

    #[test]
    fn a_successful_import_closes_the_descriptor_when_dropped() {
        let file = tempfile::tempfile().unwrap();
        file.set_len(16).unwrap();
        let raw = file.as_raw_fd();
        let fd: OwnedFd = file.into();
        let body = scanout_body(0, 2, 2, 8, DRM_FORMAT_XBGR8888, 0, true);
        let mapped = import(&body, fd, 64, 64).unwrap();
        assert_eq!(mapped.snapshot_bgra().len(), 16);
        drop(mapped);
        assert!(
            nix::unistd::close(raw).is_err(),
            "descriptor {raw} was not closed"
        );
    }

    #[test]
    fn adopted_fds_are_owned_and_closed() {
        let (read, _write) = nix::unistd::pipe().unwrap();
        let raw = read.into_raw_fd();
        let adopted = adopt_received_fds(vec![raw]);
        assert_eq!(adopted.len(), 1);
        assert_eq!(adopted[0].as_raw_fd(), raw);
        drop(adopted);
        assert!(nix::unistd::close(raw).is_err());
    }
}
