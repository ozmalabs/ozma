// SPDX-License-Identifier: AGPL-3.0-only
//! Safe, C-bridge-friendly display input for an outbound SPICE adapter.
//!
//! This module deliberately stops at the display surface boundary.  It turns
//! Ozma's composed RGBA frames into validated BGRX surfaces and provides a
//! small stdin/socket framing format for a system `spice-server` bridge.  The
//! SPICE main/display channel handshake remains in that bridge, where the
//! platform's maintained SPICE library can be used without duplicating its
//! authentication and channel state machine in Rust.

use std::io::{self, Read};

use crate::frame_output::{ComposedFrame, DamageRect};

pub const FRAME_MAGIC: [u8; 4] = *b"OZFR";
pub const MAX_DIMENSION: u32 = 16_384;
pub const MAX_FRAME_BYTES: u64 = 512 * 1024 * 1024;

#[derive(Debug, PartialEq, Eq)]
pub enum SpiceFrameError {
    Io(io::ErrorKind),
    BadMagic,
    InvalidDimensions,
    InvalidStride,
    FrameTooLarge,
}

/// One complete RGBA8 surface from the Ozma-to-SPICE bridge.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiceInputFrame {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
    pub rgba: Vec<u8>,
}

impl SpiceInputFrame {
    pub fn from_composed(frame: &ComposedFrame) -> Self {
        Self {
            width: frame.width,
            height: frame.height,
            stride: frame.width * 4,
            rgba: frame.rgba.clone(),
        }
    }

    /// Convert RGBA8 rows to the BGRX byte order used by the common SPICE
    /// 32-bit surface format. Alpha is intentionally discarded.
    pub fn to_bgrx(&self) -> Vec<u8> {
        let mut bgrx = Vec::with_capacity(self.rgba.len());
        for row in self.rgba.chunks(self.stride as usize) {
            for pixel in row[..self.width as usize * 4].chunks_exact(4) {
                bgrx.extend_from_slice(&[pixel[2], pixel[1], pixel[0], 0xff]);
            }
        }
        bgrx
    }
}

/// Read the fixed Ozma frame stream proposed for a system SPICE bridge.
///
/// Each frame is `OZFR`, followed by big-endian `width`, `height`, and
/// `stride` u32 values, then `stride * height` RGBA8 bytes. EOF between frames
/// is clean; a truncated frame is an error.
pub struct SpiceFrameReader<R> {
    reader: R,
}

impl<R: Read> SpiceFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self { reader }
    }

    pub fn read_frame(&mut self) -> Result<Option<SpiceInputFrame>, SpiceFrameError> {
        let mut header = [0u8; 16];
        match self.reader.read(&mut header[..1]) {
            Ok(0) => return Ok(None),
            Ok(1) => {}
            Ok(_) => unreachable!(),
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => return Ok(None),
            Err(error) => return Err(SpiceFrameError::Io(error.kind())),
        }
        self.reader
            .read_exact(&mut header[1..])
            .map_err(|error| SpiceFrameError::Io(error.kind()))?;
        if header[..4] != FRAME_MAGIC {
            return Err(SpiceFrameError::BadMagic);
        }
        let width = u32::from_be_bytes(header[4..8].try_into().unwrap());
        let height = u32::from_be_bytes(header[8..12].try_into().unwrap());
        let stride = u32::from_be_bytes(header[12..16].try_into().unwrap());
        if width == 0 || height == 0 || width > MAX_DIMENSION || height > MAX_DIMENSION {
            return Err(SpiceFrameError::InvalidDimensions);
        }
        let min_stride = width.checked_mul(4).ok_or(SpiceFrameError::InvalidStride)?;
        if stride < min_stride {
            return Err(SpiceFrameError::InvalidStride);
        }
        let size = u64::from(stride) * u64::from(height);
        if size > MAX_FRAME_BYTES {
            return Err(SpiceFrameError::FrameTooLarge);
        }
        let mut rgba = vec![0u8; size as usize];
        self.reader
            .read_exact(&mut rgba)
            .map_err(|error| SpiceFrameError::Io(error.kind()))?;
        Ok(Some(SpiceInputFrame {
            width,
            height,
            stride,
            rgba,
        }))
    }
}

/// Damage is retained separately so a SPICE bridge can choose a partial draw
/// command or fall back to a full surface update after reconnect.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SpiceSurfaceUpdate {
    pub revision: u64,
    pub surface: SpiceInputFrame,
    pub damage: Vec<DamageRect>,
}

impl SpiceSurfaceUpdate {
    pub fn from_composed(frame: &ComposedFrame) -> Self {
        Self {
            revision: frame.revision,
            surface: SpiceInputFrame::from_composed(frame),
            damage: frame.damage.clone(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn framed(width: u32, height: u32, stride: u32, pixels: &[u8]) -> Vec<u8> {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&FRAME_MAGIC);
        bytes.extend_from_slice(&width.to_be_bytes());
        bytes.extend_from_slice(&height.to_be_bytes());
        bytes.extend_from_slice(&stride.to_be_bytes());
        bytes.extend_from_slice(pixels);
        bytes
    }

    #[test]
    fn reads_frame_and_converts_bgrx() {
        let input = framed(2, 1, 8, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut reader = SpiceFrameReader::new(Cursor::new(input));
        let frame = reader.read_frame().unwrap().unwrap();
        assert_eq!(frame.to_bgrx(), vec![3, 2, 1, 255, 7, 6, 5, 255]);
        assert_eq!(reader.read_frame().unwrap(), None);
    }

    #[test]
    fn rejects_bad_stride_and_magic() {
        let mut reader = SpiceFrameReader::new(Cursor::new(framed(2, 1, 4, &[0; 4])));
        assert_eq!(reader.read_frame(), Err(SpiceFrameError::InvalidStride));
        let mut bad = framed(1, 1, 4, &[0; 4]);
        bad[0] = b'X';
        let mut reader = SpiceFrameReader::new(Cursor::new(bad));
        assert_eq!(reader.read_frame(), Err(SpiceFrameError::BadMagic));
    }

    #[test]
    fn preserves_damage_for_surface_bridge() {
        let frame = ComposedFrame::new(7, 1, 1, vec![1, 2, 3, 4], vec![DamageRect::full(1, 1)])
            .unwrap();
        let update = SpiceSurfaceUpdate::from_composed(&frame);
        assert_eq!(update.revision, 7);
        assert_eq!(update.damage, frame.damage);
        assert_eq!(update.surface.to_bgrx(), vec![3, 2, 1, 255]);
    }
}
