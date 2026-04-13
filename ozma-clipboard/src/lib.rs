//! ozma-clipboard — cross-platform clipboard access for the Ozma agent.
//!
//! Provides:
//!   - [`ClipboardManager`]: read/write text and PNG images via `arboard`.
//!     On Linux, falls back to `wl-clipboard` (`wl-copy` / `wl-paste`) when
//!     arboard cannot open a display (headless Wayland compositors, etc.).
//!   - [`ClipboardRing`]: bounded history ring (port of clipboard_ring.py).
//!   - [`IpcServer`]: Unix-socket (Linux/macOS) / named-pipe (Windows) server
//!     that exposes GET / SET / WATCH / LIST / SEARCH / PIN / UNPIN / CLEAR
//!     commands to the controller.

pub mod ring;
pub mod ipc;

use std::sync::Arc;

use arboard::Clipboard;
use base64::{engine::general_purpose::STANDARD as B64, Engine as _};
use image::{DynamicImage, ImageFormat, RgbaImage};
use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, Mutex};
use tracing::{debug, info, warn};

// ── Public re-exports ─────────────────────────────────────────────────────────

pub use ring::{ClipboardEntry, ClipboardRing, ContentType, EntrySummary};

// ── Clipboard content ─────────────────────────────────────────────────────────

/// A clipboard payload — either plain text or a PNG image (base64-encoded).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum ClipboardContent {
    Text { text: String },
    Image { png_b64: String, width: u32, height: u32 },
}

impl ClipboardContent {
    pub fn content_type(&self) -> ContentType {
        match self {
            ClipboardContent::Text { .. } => ContentType::Text,
            ClipboardContent::Image { .. } => ContentType::Image,
        }
    }

    pub fn as_text(&self) -> Option<&str> {
        match self {
            ClipboardContent::Text { text } => Some(text),
            _ => None,
        }
    }
}

// ── Change event ──────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClipboardChangeEvent {
    pub entry: EntrySummary,
}

// ── ClipboardManager ──────────────────────────────────────────────────────────

/// Cross-platform clipboard read/write.
///
/// Wraps `arboard::Clipboard` with:
///   - async-safe `Mutex` (arboard is `!Send` on some platforms)
///   - PNG image encode/decode
///   - Linux wl-clipboard subprocess fallback
///   - Change-event broadcast channel for WATCH subscribers
pub struct ClipboardManager {
    inner: Mutex<Option<Clipboard>>,
    pub ring: Mutex<ClipboardRing>,
    pub tx: broadcast::Sender<ClipboardChangeEvent>,
    node_name: String,
}

impl ClipboardManager {
    /// Create a new manager.  `node_name` is embedded in ring entries.
    pub fn new(node_name: impl Into<String>) -> Arc<Self> {
        let (tx, _) = broadcast::channel(64);
        Arc::new(Self {
            inner: Mutex::new(Self::open_clipboard()),
            ring: Mutex::new(ClipboardRing::new()),
            tx,
            node_name: node_name.into(),
        })
    }

    fn open_clipboard() -> Option<Clipboard> {
        match Clipboard::new() {
            Ok(cb) => Some(cb),
            Err(e) => {
                warn!("arboard: could not open clipboard: {e}");
                None
            }
        }
    }

    // ── Read ──────────────────────────────────────────────────────────────────

    /// Read the current clipboard contents.
    pub async fn get(&self) -> Option<ClipboardContent> {
        // Try arboard first
        {
            let mut guard = self.inner.lock().await;
            if let Some(cb) = guard.as_mut() {
                // Try text
                if let Ok(text) = cb.get_text() {
                    debug!("clipboard get: text ({} bytes)", text.len());
                    return Some(ClipboardContent::Text { text });
                }
                // Try image
                if let Ok(img) = cb.get_image() {
                    if let Some(content) = Self::arboard_image_to_content(img) {
                        return Some(content);
                    }
                }
            }
        }

        // Linux fallback: wl-paste
        #[cfg(target_os = "linux")]
        if let Some(text) = wl_paste().await {
            return Some(ClipboardContent::Text { text });
        }

        None
    }

    // ── Write ─────────────────────────────────────────────────────────────────

    /// Write content to the clipboard and push it onto the ring.
    pub async fn set(&self, content: ClipboardContent) -> Result<(), String> {
        let result = self.set_raw(&content).await;

        // Always push to ring regardless of clipboard write success
        let entry = {
            let mut ring = self.ring.lock().await;
            let text = match &content {
                ClipboardContent::Text { text } => text.clone(),
                ClipboardContent::Image { width, height, .. } => {
                    format!("[image {}×{}]", width, height)
                }
            };
            ring.push(text, &self.node_name, "", content.content_type())
        };

        // Broadcast change event
        let _ = self.tx.send(ClipboardChangeEvent {
            entry: entry.to_summary(),
        });

        // OS clipboard write failure is non-fatal — ring was updated.
        if let Err(e) = result {
            warn!("OS clipboard write failed (headless?): {e}");
        }
        Ok(())
    }

    async fn set_raw(&self, content: &ClipboardContent) -> Result<(), String> {
        {
            let mut guard = self.inner.lock().await;
            if let Some(cb) = guard.as_mut() {
                match content {
                    ClipboardContent::Text { text } => {
                        cb.set_text(text).map_err(|e| e.to_string())?;
                        info!("clipboard set: text ({} bytes)", text.len());
                        return Ok(());
                    }
                    ClipboardContent::Image { png_b64, .. } => {
                        let png_bytes = B64.decode(png_b64).map_err(|e| e.to_string())?;
                        let img = image::load_from_memory_with_format(&png_bytes, ImageFormat::Png)
                            .map_err(|e| e.to_string())?
                            .into_rgba8();
                        let (w, h) = img.dimensions();
                        let img_data = arboard::ImageData {
                            width: w as usize,
                            height: h as usize,
                            bytes: img.into_raw().into(),
                        };
                        cb.set_image(img_data).map_err(|e| e.to_string())?;
                        info!("clipboard set: image {w}×{h}");
                        return Ok(());
                    }
                }
            }
        }

        // Linux fallback: wl-copy
        #[cfg(target_os = "linux")]
        if let ClipboardContent::Text { text } = content {
            return wl_copy(text).await;
        }

        Err("clipboard not available".into())
    }

    // ── Helpers ───────────────────────────────────────────────────────────────

    fn arboard_image_to_content(img: arboard::ImageData<'_>) -> Option<ClipboardContent> {
        let rgba = RgbaImage::from_raw(
            img.width as u32,
            img.height as u32,
            img.bytes.into_owned(),
        )?;
        let (w, h) = rgba.dimensions();
        let mut buf = Vec::new();
        DynamicImage::ImageRgba8(rgba)
            .write_to(&mut std::io::Cursor::new(&mut buf), ImageFormat::Png)
            .ok()?;
        Some(ClipboardContent::Image {
            png_b64: B64.encode(&buf),
            width: w,
            height: h,
        })
    }

    /// Start a background polling loop that detects external clipboard changes
    /// (e.g. user copies something) and pushes them onto the ring.
    pub async fn start_watch_loop(self: Arc<Self>, interval_ms: u64) {
        let mut last: Option<String> = None;
        loop {
            tokio::time::sleep(tokio::time::Duration::from_millis(interval_ms)).await;
            if let Some(content) = self.get().await {
                if let Some(text) = content.as_text() {
                    let changed = last.as_deref() != Some(text);
                    if changed {
                        last = Some(text.to_owned());
                        let entry = {
                            let mut ring = self.ring.lock().await;
                            ring.push(text, &self.node_name, "", ContentType::Text)
                        };
                        let _ = self.tx.send(ClipboardChangeEvent {
                            entry: entry.to_summary(),
                        });
                    }
                }
            }
        }
    }
}

// ── Linux wl-clipboard fallback ───────────────────────────────────────────────

#[cfg(target_os = "linux")]
async fn wl_paste() -> Option<String> {
    // Only attempt if wl-paste is on PATH
    if which::which("wl-paste").is_err() {
        return None;
    }
    let out = tokio::process::Command::new("wl-paste")
        .arg("--no-newline")
        .output()
        .await
        .ok()?;
    if out.status.success() {
        String::from_utf8(out.stdout).ok()
    } else {
        None
    }
}

#[cfg(target_os = "linux")]
async fn wl_copy(text: &str) -> Result<(), String> {
    which::which("wl-copy").map_err(|_| "wl-copy not found".to_string())?;
    let status = tokio::process::Command::new("wl-copy")
        .arg("--")
        .arg(text)
        .status()
        .await
        .map_err(|e| e.to_string())?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("wl-copy exited with {status}"))
    }
}
