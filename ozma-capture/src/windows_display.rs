// SPDX-License-Identifier: AGPL-3.0-only
//! Windows desktop capture provider backed by Win32 GDI (`BitBlt` + `GetDIBits`).
//!
//! Produces packed RGB24 [`Frame`]s at a paced frame rate so `ozma-node`
//! (`--profile desktop`) can serve `/snapshot`, HLS, and WebRTC/transport
//! streams on Windows without external capture drivers.

use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError};
use ozma_protocol::{codes, ApiError, CaptureCapability, CaptureKind};

use crate::provider::CaptureProvider;

pub const WINDOWS_DEFAULT_ID: &str = "windows-desktop0";
pub const WINDOWS_DEFAULT_NAME: &str = "Windows Desktop";
pub const WINDOWS_DEFAULT_FPS: u32 = 30;

/// Discovers and opens the primary Windows desktop display.
#[derive(Clone, Debug)]
pub struct WindowsDisplayProvider {
    fps: u32,
}

impl Default for WindowsDisplayProvider {
    fn default() -> Self {
        Self::new()
    }
}

impl WindowsDisplayProvider {
    pub fn new() -> Self {
        Self {
            fps: WINDOWS_DEFAULT_FPS,
        }
    }
}

fn primary_screen_size() -> (u32, u32) {
    use windows::Win32::UI::WindowsAndMessaging::{GetSystemMetrics, SM_CXSCREEN, SM_CYSCREEN};
    let w = unsafe { GetSystemMetrics(SM_CXSCREEN) }.max(1) as u32;
    let h = unsafe { GetSystemMetrics(SM_CYSCREEN) }.max(1) as u32;
    (w, h)
}

impl CaptureProvider for WindowsDisplayProvider {
    fn kind(&self) -> CaptureKind {
        CaptureKind::PipeWire
    }

    fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
        let (w, h) = primary_screen_size();
        Ok(vec![CaptureCapability {
            id: WINDOWS_DEFAULT_ID.to_owned(),
            name: WINDOWS_DEFAULT_NAME.to_owned(),
            kind: CaptureKind::PipeWire,
            formats: vec!["rgb24".to_owned()],
            max_width: w,
            max_height: h,
            max_fps: self.fps,
            compressed: false,
            hdr: false,
            has_audio: false,
        }])
    }

    fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
        if capability.id != WINDOWS_DEFAULT_ID {
            return Err(ApiError::new(
                codes::NOT_FOUND,
                format!("unknown Windows capture source {:?}", capability.id),
            ));
        }
        let (w, h) = primary_screen_size();
        Ok(Box::new(WindowsDisplaySource {
            format: FrameFormat::rgb24(w, h),
            interval: Duration::from_secs_f64(1.0 / f64::from(self.fps.max(1))),
            sequence: 0,
            start: Instant::now(),
            next_tick: Instant::now(),
            stopped: false,
        }))
    }
}

/// Active GDI desktop capture source.
pub struct WindowsDisplaySource {
    format: FrameFormat,
    interval: Duration,
    sequence: u64,
    start: Instant,
    next_tick: Instant,
    stopped: bool,
}

#[async_trait]
impl CaptureSource for WindowsDisplaySource {
    fn id(&self) -> &str {
        WINDOWS_DEFAULT_ID
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

        let (w, h) = primary_screen_size();
        self.format = FrameFormat::rgb24(w, h);
        let data = tokio::task::spawn_blocking(move || capture_rgb24(w, h))
            .await
            .map_err(|e| {
                MediaError::capture(WINDOWS_DEFAULT_ID, format!("capture task join failed: {e}"))
            })??;

        let seq = self.sequence;
        self.sequence = self.sequence.wrapping_add(1);
        let pts_micros = self.start.elapsed().as_micros() as u64;

        Frame::with_data(self.format, pts_micros, seq, true, data)
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        self.stopped = true;
        Ok(())
    }
}

fn capture_rgb24(width: u32, height: u32) -> Result<Vec<u8>, MediaError> {
    use windows::Win32::Graphics::Gdi::{
        BitBlt, CreateCompatibleBitmap, CreateCompatibleDC, DeleteDC, DeleteObject, GetDC,
        GetDIBits, ReleaseDC, SelectObject, BITMAPINFO, BITMAPINFOHEADER, BI_RGB, DIB_RGB_COLORS,
        SRCCOPY,
    };

    unsafe {
        let screen_dc = GetDC(None);
        if screen_dc.is_invalid() {
            return Err(MediaError::capture(
                WINDOWS_DEFAULT_ID,
                "GetDC(NULL) failed",
            ));
        }
        let mem_dc = CreateCompatibleDC(screen_dc);
        if mem_dc.is_invalid() {
            ReleaseDC(None, screen_dc);
            return Err(MediaError::capture(
                WINDOWS_DEFAULT_ID,
                "CreateCompatibleDC failed",
            ));
        }
        let bitmap = CreateCompatibleBitmap(screen_dc, width as i32, height as i32);
        if bitmap.is_invalid() {
            let _ = DeleteDC(mem_dc);
            ReleaseDC(None, screen_dc);
            return Err(MediaError::capture(
                WINDOWS_DEFAULT_ID,
                "CreateCompatibleBitmap failed",
            ));
        }

        let old = SelectObject(mem_dc, bitmap);
        let blt_ok = BitBlt(
            mem_dc,
            0,
            0,
            width as i32,
            height as i32,
            screen_dc,
            0,
            0,
            SRCCOPY,
        );

        let mut bgra = vec![0u8; (width as usize) * (height as usize) * 4];
        if blt_ok.is_ok() {
            let mut bmi = BITMAPINFO {
                bmiHeader: BITMAPINFOHEADER {
                    biSize: std::mem::size_of::<BITMAPINFOHEADER>() as u32,
                    biWidth: width as i32,
                    biHeight: -(height as i32), // top-down
                    biPlanes: 1,
                    biBitCount: 32,
                    biCompression: BI_RGB.0,
                    ..Default::default()
                },
                ..Default::default()
            };
            GetDIBits(
                mem_dc,
                bitmap,
                0,
                height,
                Some(bgra.as_mut_ptr().cast()),
                &mut bmi,
                DIB_RGB_COLORS,
            );
        }

        SelectObject(mem_dc, old);
        let _ = DeleteObject(bitmap);
        let _ = DeleteDC(mem_dc);
        ReleaseDC(None, screen_dc);

        if blt_ok.is_err() {
            return Err(MediaError::capture(WINDOWS_DEFAULT_ID, "BitBlt failed"));
        }

        // Convert BGRA -> RGB24
        let mut rgb = Vec::with_capacity((width as usize) * (height as usize) * 3);
        for px in bgra.chunks_exact(4) {
            rgb.push(px[2]);
            rgb.push(px[1]);
            rgb.push(px[0]);
        }
        Ok(rgb)
    }
}
