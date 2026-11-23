// SPDX-License-Identifier: AGPL-3.0-only
//! The stable frame seam for an outbound RDP adapter.
//!
//! `RdpFrameReader` consumes the deliberately boring `OZFR` stream emitted by
//! the composed-frame producer. Keeping this boundary independent of the
//! FreeRDP C ABI lets the direct-surface C bridge in `examples/rdp-direct` own
//! authentication, peer lifecycle, and surface-update negotiation. The original
//! Xvfb/ffplay shadow prototype also consumes the same records.

use std::io::{self, Read};

use thiserror::Error;

const HEADER_LEN: usize = 16;
const MAGIC: &[u8; 4] = b"OZFR";
const MAX_DIMENSION: u32 = 16_384;
const MAX_FRAME_BYTES: usize = 64 * 1024 * 1024;

#[derive(Debug, Error)]
pub enum RdpFrameError {
    #[error("frame stream ended before a complete header or frame")]
    Truncated,
    #[error("invalid OZFR magic")]
    InvalidMagic,
    #[error("invalid frame dimensions {width}x{height} with stride {stride}")]
    InvalidLayout {
        width: u32,
        height: u32,
        stride: u32,
    },
    #[error("frame is too large ({0} bytes)")]
    TooLarge(usize),
    #[error("frame dimensions changed from {expected:?} to {actual:?}")]
    DimensionsChanged {
        expected: (u32, u32, u32),
        actual: (u32, u32, u32),
    },
    #[error("I/O while reading frame stream: {0}")]
    Io(#[from] io::Error),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RdpFrameLayout {
    pub width: u32,
    pub height: u32,
    pub stride: u32,
}

impl RdpFrameLayout {
    fn validate(self) -> Result<usize, RdpFrameError> {
        if self.width == 0
            || self.height == 0
            || self.width > MAX_DIMENSION
            || self.height > MAX_DIMENSION
            || self.stride < self.width.saturating_mul(4)
        {
            return Err(RdpFrameError::InvalidLayout {
                width: self.width,
                height: self.height,
                stride: self.stride,
            });
        }
        (self.stride as usize)
            .checked_mul(self.height as usize)
            .filter(|size| *size <= MAX_FRAME_BYTES)
            .ok_or_else(|| {
                RdpFrameError::TooLarge((self.stride as usize).saturating_mul(self.height as usize))
            })
    }
}

/// Reads fixed-dimension packed RGBA8 frames from the `OZFR` stream.
pub struct RdpFrameReader<R> {
    reader: R,
    layout: Option<RdpFrameLayout>,
    frame_index: u64,
}

impl<R: Read> RdpFrameReader<R> {
    pub fn new(reader: R) -> Self {
        Self {
            reader,
            layout: None,
            frame_index: 0,
        }
    }

    pub fn layout(&self) -> Option<RdpFrameLayout> {
        self.layout
    }

    /// Reads one frame. `Ok(None)` means clean EOF between frames.
    pub fn next_frame(&mut self) -> Result<Option<(u64, RdpFrameLayout, Vec<u8>)>, RdpFrameError> {
        let mut header = [0_u8; HEADER_LEN];
        let first = self.reader.read(&mut header[..1])?;
        if first == 0 {
            return Ok(None);
        }
        self.reader.read_exact(&mut header[1..]).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                RdpFrameError::Truncated
            } else {
                RdpFrameError::Io(error)
            }
        })?;
        if &header[..4] != MAGIC {
            return Err(RdpFrameError::InvalidMagic);
        }
        let layout = RdpFrameLayout {
            width: u32::from_be_bytes(header[4..8].try_into().unwrap()),
            height: u32::from_be_bytes(header[8..12].try_into().unwrap()),
            stride: u32::from_be_bytes(header[12..16].try_into().unwrap()),
        };
        let size = layout.validate()?;
        if let Some(expected) = self.layout {
            if expected != layout {
                return Err(RdpFrameError::DimensionsChanged {
                    expected: (expected.width, expected.height, expected.stride),
                    actual: (layout.width, layout.height, layout.stride),
                });
            }
        } else {
            self.layout = Some(layout);
        }
        let mut frame = vec![0_u8; size];
        self.reader.read_exact(&mut frame).map_err(|error| {
            if error.kind() == io::ErrorKind::UnexpectedEof {
                RdpFrameError::Truncated
            } else {
                RdpFrameError::Io(error)
            }
        })?;
        let index = self.frame_index;
        self.frame_index += 1;
        Ok(Some((index, layout, frame)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn stream(layout: RdpFrameLayout, pixels: &[u8]) -> Vec<u8> {
        let mut data = b"OZFR".to_vec();
        data.extend(layout.width.to_be_bytes());
        data.extend(layout.height.to_be_bytes());
        data.extend(layout.stride.to_be_bytes());
        data.extend(pixels);
        data
    }

    #[test]
    fn reads_fixed_rgba_frames() {
        let layout = RdpFrameLayout {
            width: 2,
            height: 1,
            stride: 8,
        };
        let bytes = stream(layout, &[1, 2, 3, 4, 5, 6, 7, 8]);
        let mut reader = RdpFrameReader::new(bytes.as_slice());
        let frame = reader.next_frame().unwrap().unwrap();
        assert_eq!(frame.0, 0);
        assert_eq!(frame.1, layout);
        assert_eq!(frame.2, vec![1, 2, 3, 4, 5, 6, 7, 8]);
        assert!(reader.next_frame().unwrap().is_none());
    }

    #[test]
    fn rejects_dimension_changes() {
        let first = stream(
            RdpFrameLayout {
                width: 1,
                height: 1,
                stride: 4,
            },
            &[0; 4],
        );
        let second = stream(
            RdpFrameLayout {
                width: 2,
                height: 1,
                stride: 8,
            },
            &[0; 8],
        );
        let bytes = [first, second].concat();
        let mut reader = RdpFrameReader::new(bytes.as_slice());
        reader.next_frame().unwrap();
        assert!(matches!(
            reader.next_frame(),
            Err(RdpFrameError::DimensionsChanged { .. })
        ));
    }

    #[test]
    fn rejects_invalid_layout() {
        for layout in [
            RdpFrameLayout {
                width: 0,
                height: 1,
                stride: 4,
            },
            RdpFrameLayout {
                width: 2,
                height: 1,
                stride: 7,
            },
        ] {
            let bytes = stream(layout, &[0; 8]);
            let mut reader = RdpFrameReader::new(bytes.as_slice());
            assert!(matches!(
                reader.next_frame(),
                Err(RdpFrameError::InvalidLayout { .. })
            ));
        }
    }

    #[test]
    fn reports_truncated_frames() {
        let layout = RdpFrameLayout {
            width: 2,
            height: 1,
            stride: 8,
        };
        let mut bytes = stream(layout, &[1, 2, 3]);
        bytes.truncate(16 + 3);
        let mut reader = RdpFrameReader::new(bytes.as_slice());
        assert!(matches!(reader.next_frame(), Err(RdpFrameError::Truncated)));
    }
}
