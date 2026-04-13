//! Screen and audio capture task.
//!
//! Platform backends
//! -----------------
//! Linux   — PipeWire (audio) + XDG desktop portal / KMS (video)
//! Windows — DXGI Desktop Duplication (video) + WASAPI (audio)
//! macOS   — ScreenCaptureKit (video) + CoreAudio (audio)
//!
//! This module owns the capture loop; encoded frames are pushed into a
//! broadcast channel consumed by the WebRTC / HLS pipeline (future task).

use anyhow::Result;
use tracing::{debug, info};

/// Run the capture loop indefinitely.
pub async fn run() -> Result<()> {
    info!("capture task starting");

    loop {
        // TODO: initialise platform capture backend, encode frames, push to
        //       broadcast channel.  For now we just yield to avoid busy-spin.
        debug!("capture tick");
        tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;
    }
}
