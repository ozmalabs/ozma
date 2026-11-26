// SPDX-License-Identifier: AGPL-3.0-only
//! macOS desktop capture provider backed by `/usr/sbin/screencapture`.
//!
//! Uses `screencapture -x -t bmp` so `ozma-node --profile desktop` can capture
//! the primary display in both real Macs and QEMU/KVM macOS VMs (which lack
//! Metal/GPU acceleration for `ScreenCaptureKit`) with zero extra crate deps.

use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};

use crate::provider::CaptureProvider;

pub const MACOS_DEFAULT_ID: &str = "macos-desktop0";
pub const MACOS_DEFAULT_NAME: &str = "macOS Desktop";
pub const MACOS_DEFAULT_FPS: u32 = 15;
pub const MACOS_DEFAULT_WIDTH: u32 = 1920;
pub const MACOS_DEFAULT_HEIGHT: u32 = 1080;

static CAPTURE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Discovers and opens the primary macOS display.
#[derive(Clone, Debug)]
pub struct MacOsDisplayProvider {
    fps: u32,
}

impl Default for MacOsDisplayProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl MacOsDisplayProvider {
    pub fn new() -> Self {
        Self {
            fps: MACOS_DEFAULT_FPS,
        }
    }
}

impl CaptureProvider for MacOsDisplayProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::PipeWire
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        Ok(vec![CaptureCapability {
            id: MACOS_DEFAULT_ID.to_owned(),
            name: MACOS_DEFAULT_NAME.to_owned(),
            kind: CaptureKind::PipeWire,
            formats: vec!["rgb24".to_owned()],
            max_width: MACOS_DEFAULT_WIDTH,
            max_height: MACOS_DEFAULT_HEIGHT,
            max_fps: self.fps,
            compressed: false,
            hdr: false,
            has_audio: false,
        }])
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.id != MACOS_DEFAULT_ID {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("unknown macOS capture source {:?}", capability.id),
            ));
        }
        Ok(Box::new(MacOsDisplaySource {
            format: FrameFormat::rgb24(MACOS_DEFAULT_WIDTH, MACOS_DEFAULT_HEIGHT),
            interval: Duration::from_secs_f64(1.0 / f64::from(self.fps.max(1))),
            sequence: 0,
            start: Instant::now(),
            next_tick: Instant::now(),
            stopped: false,
        }))
    }
}

/// Active macOS `screencapture` desktop source.
pub struct MacOsDisplaySource {
    format: FrameFormat,
    interval: Duration,
    sequence: u64,
    start: Instant,
    next_tick: Instant,
    stopped: bool,
}

#[async_trait]
impl CaptureSource for MacOsDisplaySource {
    fn id(&self) -> &str {
        MACOS_DEFAULT_ID
    }

    fn format(&self) -> FrameFormat {
        self.format
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        if self.stopped {
            return Err(MediaError::EndOfStream);
        }

        let now = Instant::now();
        if self.next_tick > now {
            tokio::time::sleep(self.next_tick - now).await;
        }
        self.next_tick += self.interval;

        let (w, h, rgb) = tokio::task::spawn_blocking(capture_bmp_rgb24)
            .await
            .map_err(|e| {
                MediaError::capture(MACOS_DEFAULT_ID, format!("capture join failed: {e}"))
            })??;

        self.format = FrameFormat::rgb24(w, h);
        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let pts_micros = self.start.elapsed().as_micros() as u64;

        Frame::with_data(self.format, pts_micros, seq, true, rgb)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        self.stopped = true;
        Ok(())
    }
}

fn capture_bmp_rgb24() -> Result<(u32, u32, Vec<u8>), MediaError> {
    let seq = CAPTURE_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = std::env::temp_dir().join(format!("ozma-macos-cap-{}-{seq}.bmp", std::process::id()));
    let status = std::process::Command::new("/usr/sbin/screencapture")
        .args(["-x", "-t", "bmp"])
        .arg(&tmp)
        .status()
        .map_err(|e| MediaError::capture(MACOS_DEFAULT_ID, format!("screencapture failed: {e}")))?;

    let bytes = std::fs::read(&tmp);
    let _ = std::fs::remove_file(&tmp);

    if !status.success() {
        return Err(MediaError::capture(
            MACOS_DEFAULT_ID,
            format!("screencapture exited with {status}"),
        ));
    }
    let bmp = bytes.map_err(|e| {
        MediaError::capture(
            MACOS_DEFAULT_ID,
            format!("read screencapture bmp failed: {e}"),
        )
    })?;
    parse_bmp_rgb24(&bmp)
}

/// Parse an uncompressed 24-bit or 32-bit BMP into packed top-down RGB24.
pub fn parse_bmp_rgb24(bmp: &[u8]) -> Result<(u32, u32, Vec<u8>), MediaError> {
    if bmp.len() < 54 || &bmp[0..2] != b"BM" {
        return Err(MediaError::capture(MACOS_DEFAULT_ID, "invalid BMP header"));
    }
    let data_offset = u32::from_le_bytes([bmp[10], bmp[11], bmp[12], bmp[13]]) as usize;
    let width_i32 = i32::from_le_bytes([bmp[18], bmp[19], bmp[20], bmp[21]]);
    let height_i32 = i32::from_le_bytes([bmp[22], bmp[23], bmp[24], bmp[25]]);
    let bpp = u16::from_le_bytes([bmp[28], bmp[29]]);
    let compression = u32::from_le_bytes([bmp[30], bmp[31], bmp[32], bmp[33]]);

    if width_i32 <= 0 || height_i32 == 0 || (compression != 0 && compression != 3) {
        return Err(MediaError::capture(
            MACOS_DEFAULT_ID,
            format!("unsupported BMP geometry/compression ({width_i32}x{height_i32}, comp={compression})"),
        ));
    }
    let width = width_i32 as usize;
    let top_down = height_i32 < 0;
    let height = height_i32.unsigned_abs() as usize;
    let bytes_per_px = match bpp {
        24 => 3,
        32 => 4,
        _ => {
            return Err(MediaError::capture(
                MACOS_DEFAULT_ID,
                format!("unsupported BMP bpp: {bpp}"),
            ))
        }
    };
    let row_stride = (width * bytes_per_px).div_ceil(4) * 4;
    let needed = data_offset.saturating_add(row_stride.saturating_mul(height));
    if bmp.len() < needed {
        return Err(MediaError::capture(
            MACOS_DEFAULT_ID,
            "truncated BMP pixel data",
        ));
    }

    let mut rgb = Vec::with_capacity(width * height * 3);
    for y in 0..height {
        let src_y = if top_down { y } else { height - 1 - y };
        let row = &bmp[data_offset + src_y * row_stride
            ..data_offset + src_y * row_stride + width * bytes_per_px];
        for px in row.chunks_exact(bytes_per_px) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
    }
    Ok((width as u32, height as u32, rgb))
}
