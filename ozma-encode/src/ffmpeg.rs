// SPDX-License-Identifier: AGPL-3.0-only
//! Optional, non-fatal detection of an `ffmpeg` binary.
//!
//! FFmpeg is a useful practical backend, but a node must start and advertise
//! software/fake capability even when it is absent. Detection therefore never
//! panics and never makes a missing binary an error.

use std::path::PathBuf;
use std::process::Command;

/// Result of looking for `ffmpeg` on `PATH`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct FfmpegStatus {
    /// Whether an `ffmpeg` binary ran successfully.
    pub available: bool,
    /// Path or command name used, when available.
    pub path: Option<PathBuf>,
    /// Parsed version token, for example `6.1.1`.
    pub version: Option<String>,
}

impl FfmpegStatus {
    /// A status representing a missing or unusable binary.
    pub fn unavailable() -> Self {
        Self::default()
    }
}

/// Run `ffmpeg -version` with a short, platform-safe check.
///
/// This is a best-effort capability hint. Callers must not require it, and
/// tests must not assume ffmpeg is installed.
pub fn detect_ffmpeg() -> FfmpegStatus {
    let output = match Command::new("ffmpeg").arg("-version").output() {
        Ok(output) => output,
        Err(_) => return FfmpegStatus::unavailable(),
    };
    if !output.status.success() {
        return FfmpegStatus::unavailable();
    }

    let first_line = String::from_utf8_lossy(&output.stdout)
        .lines()
        .next()
        .unwrap_or_default()
        .to_owned();
    let version = first_line
        .split_whitespace()
        .nth(2)
        .filter(|version| !version.is_empty())
        .map(str::to_owned);

    FfmpegStatus {
        available: true,
        path: Some(PathBuf::from("ffmpeg")),
        version,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detection_is_non_fatal() {
        let status = detect_ffmpeg();
        if status.available {
            assert!(status.path.is_some());
        } else {
            assert!(status.path.is_none());
            assert!(status.version.is_none());
        }
    }
}
