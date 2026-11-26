// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Packed BGRA shared-memory frame validation and bounded reading.
//!
//! The node replaces the whole file atomically (`write` to `<path>.tmp`, then
//! `rename`), so a reader never observes a torn frame. This module still
//! validates the exact byte length before handing a buffer to the encoder; a
//! short or oversized file is a skipped frame, not a reinterpreted image.

use std::path::Path;

use thiserror::Error;

/// Geometry of the shared-memory frame file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameSpec {
    pub width: u32,
    pub height: u32,
}

impl FrameSpec {
    pub fn new(width: u32, height: u32) -> Result<Self, FrameError> {
        if width < 2 || height < 2 {
            return Err(FrameError::InvalidGeometry { width, height });
        }
        width
            .checked_mul(height)
            .and_then(|pixels| pixels.checked_mul(4))
            .ok_or(FrameError::InvalidGeometry { width, height })?;
        Ok(Self { width, height })
    }

    /// Exact packed BGRA byte length.
    pub fn frame_len(self) -> usize {
        self.width as usize * self.height as usize * 4
    }
}

/// A shared-memory frame could not be used.
#[derive(Debug, Error)]
pub enum FrameError {
    #[error("frame dimensions {width}x{height} are not usable")]
    InvalidGeometry { width: u32, height: u32 },
    #[error("shared-memory file is {actual} bytes, expected exactly {expected} for {width}x{height} BGRA")]
    InvalidLength {
        width: u32,
        height: u32,
        expected: usize,
        actual: usize,
    },
    #[error("could not read shared-memory frame `{path}`: {message}")]
    Io { path: String, message: String },
}

/// Validate an in-memory buffer against the configured geometry.
pub fn validate_frame(buffer: &[u8], spec: FrameSpec) -> Result<(), FrameError> {
    let expected = spec.frame_len();
    if buffer.len() != expected {
        return Err(FrameError::InvalidLength {
            width: spec.width,
            height: spec.height,
            expected,
            actual: buffer.len(),
        });
    }
    Ok(())
}

/// Read one frame after checking the file length, so an unexpected huge file
/// is rejected before it is loaded.
pub async fn read_frame(path: &Path, spec: FrameSpec) -> Result<Vec<u8>, FrameError> {
    let expected = spec.frame_len();
    let metadata = tokio::fs::metadata(path)
        .await
        .map_err(|error| FrameError::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
    let actual = usize::try_from(metadata.len()).unwrap_or(usize::MAX);
    if actual != expected {
        return Err(FrameError::InvalidLength {
            width: spec.width,
            height: spec.height,
            expected,
            actual,
        });
    }
    let buffer = tokio::fs::read(path)
        .await
        .map_err(|error| FrameError::Io {
            path: path.display().to_string(),
            message: error.to_string(),
        })?;
    validate_frame(&buffer, spec)?;
    Ok(buffer)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spec_computes_packed_bgra_length() {
        assert_eq!(FrameSpec::new(640, 480).unwrap().frame_len(), 1_228_800);
        assert_eq!(FrameSpec::new(2, 2).unwrap().frame_len(), 16);
    }

    #[test]
    fn zero_geometry_is_rejected() {
        assert!(FrameSpec::new(0, 10).is_err());
        assert!(FrameSpec::new(10, 0).is_err());
    }

    #[test]
    fn exact_length_passes_and_short_or_long_length_fails() {
        let spec = FrameSpec::new(4, 4).unwrap();
        assert!(validate_frame(&vec![0u8; 64], spec).is_ok());
        assert!(validate_frame(&vec![0u8; 63], spec).is_err());
        assert!(validate_frame(&vec![0u8; 65], spec).is_err());
    }

    #[tokio::test]
    async fn async_reader_rejects_a_wrong_sized_file() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("frame.bgra");
        tokio::fs::write(&path, vec![0u8; 12]).await.unwrap();
        let error = read_frame(&path, FrameSpec::new(2, 2).unwrap())
            .await
            .expect_err("wrong length");
        assert!(matches!(error, FrameError::InvalidLength { .. }));
    }

    #[tokio::test]
    async fn async_reader_accepts_a_valid_frame() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("frame.bgra");
        tokio::fs::write(&path, vec![7u8; 16]).await.unwrap();
        let frame = read_frame(&path, FrameSpec::new(2, 2).unwrap())
            .await
            .expect("valid frame");
        assert_eq!(frame.len(), 16);
    }
}
