//! V4L2 device enumeration via the `v4l` crate.
//!
//! Walks `/dev/video*`, opens each device, and collects the supported
//! pixel formats and the maximum resolution for each format.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use v4l::{format::fourcc::FourCC, prelude::*, video::Capture};

/// A discovered V4L2 capture device.
#[derive(Debug, Clone)]
pub struct CaptureDevice {
    /// Kernel device path, e.g. `/dev/video0`.
    pub path: PathBuf,
    /// Human-readable card name reported by the driver.
    pub name: String,
    /// Supported pixel-format FourCC strings, e.g. `["MJPG", "YUYV"]`.
    pub formats: Vec<String>,
    /// Maximum width across all supported formats.
    pub max_width: u32,
    /// Maximum height across all supported formats.
    pub max_height: u32,
    /// Associated ALSA device string if the driver exposes one (heuristic).
    pub audio_device: Option<String>,
}

/// Enumerate all V4L2 capture devices present in `/dev`.
pub fn enumerate() -> Vec<CaptureDevice> {
    let mut devices = Vec::new();

    let mut paths: Vec<PathBuf> = match std::fs::read_dir("/dev") {
        Ok(rd) => rd
            .filter_map(|e| e.ok())
            .map(|e| e.path())
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with("video"))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return devices,
    };
    paths.sort();

    for path in paths {
        match probe_device(&path) {
            Ok(Some(dev)) => devices.push(dev),
            Ok(None) => {}
            Err(e) => tracing::debug!("Skipping {}: {}", path.display(), e),
        }
    }

    devices
}

/// Open a single V4L2 device and extract its capabilities.
/// Returns `Ok(None)` if the device is not a capture device.
fn probe_device(path: &Path) -> Result<Option<CaptureDevice>> {
    let dev = Device::with_path(path)
        .with_context(|| format!("open {}", path.display()))?;

    let caps = dev
        .query_caps()
        .with_context(|| format!("query_caps {}", path.display()))?;

    use v4l::capability::Flags;
    if !caps.capabilities.contains(Flags::VIDEO_CAPTURE) {
        return Ok(None);
    }

    let name = caps
        .card
        .to_string_lossy()
        .trim_end_matches('\0')
        .to_owned();

    let mut formats: Vec<String> = Vec::new();
    let mut max_width: u32 = 0;
    let mut max_height: u32 = 0;

    for fmt in dev.enum_formats().unwrap_or_default() {
        let fourcc = fmt.fourcc;
        let tag = fourcc_to_str(fourcc);
        if !formats.contains(&tag) {
            formats.push(tag);
        }

        for fs in dev.enum_framesizes(fourcc).unwrap_or_default() {
            use v4l::framesize::FrameSizeEnum;
            match fs.size {
                FrameSizeEnum::Discrete(d) => {
                    if d.width > max_width {
                        max_width = d.width;
                    }
                    if d.height > max_height {
                        max_height = d.height;
                    }
                }
                FrameSizeEnum::Stepwise(s) => {
                    if s.max_width > max_width {
                        max_width = s.max_width;
                    }
                    if s.max_height > max_height {
                        max_height = s.max_height;
                    }
                }
            }
        }
    }

    // Fallback: read the current format if no frame sizes were found.
    if max_width == 0 {
        if let Ok(fmt) = dev.format() {
            max_width = fmt.width;
            max_height = fmt.height;
        }
    }

    // Heuristic: map /dev/videoN → hw:N,0 for ALSA.
    let audio_device = path
        .file_name()
        .and_then(|n| n.to_str())
        .and_then(|n| n.strip_prefix("video"))
        .and_then(|idx| idx.parse::<u32>().ok())
        .map(|idx| format!("hw:{},0", idx));

    Ok(Some(CaptureDevice {
        path: path.to_owned(),
        name,
        formats,
        max_width,
        max_height,
        audio_device,
    }))
}

fn fourcc_to_str(fourcc: FourCC) -> String {
    let b = fourcc.repr;
    std::str::from_utf8(&b)
        .unwrap_or("????")
        .trim_end()
        .to_owned()
}
