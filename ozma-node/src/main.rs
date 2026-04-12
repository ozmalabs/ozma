//! ozma-node — V4L2 capture → ffmpeg HLS pipeline
//!
//! Enumerates V4L2 devices, starts the capture pipeline for the first
//! available device, serves HLS on port 7382, and handles hot-plug events.

mod capture;
mod hotplug;
mod serve;
mod v4l_enum;

use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Result;
use tracing::{error, info, warn};

const HLS_DIR: &str = "/tmp/ozma-stream";
const HTTP_PORT: u16 = 7382;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_node=info".parse()?),
        )
        .init();

    // Ensure ffmpeg-sidecar can locate (or download) an ffmpeg binary.
    ffmpeg_sidecar::download::auto_download().ok();

    let hls_dir = PathBuf::from(HLS_DIR);
    std::fs::create_dir_all(&hls_dir)?;

    // Initial device enumeration.
    let devices = v4l_enum::enumerate();
    info!("Found {} V4L2 capture device(s)", devices.len());
    for d in &devices {
        info!("  {} — {} {:?}", d.path.display(), d.name, d.formats);
    }

    // Start capture on the first available device.
    let mut active_capture: Option<capture::MediaCapture> = None;
    if let Some(dev) = devices.first().cloned() {
        let enc = capture::EncoderConfig::software_h264();
        let mut mc = capture::MediaCapture::new(dev, enc, hls_dir.clone());
        mc.start().await?;
        active_capture = Some(mc);
    } else {
        warn!("No V4L2 capture devices found — waiting for hot-plug");
    }

    // HTTP server.
    let state = serve::AppState {
        devices: Arc::new(devices),
    };
    let router = serve::build_router(hls_dir.clone(), state);
    let listener = tokio::net::TcpListener::bind(("0.0.0.0", HTTP_PORT)).await?;
    info!("HLS server listening on http://0.0.0.0:{}", HTTP_PORT);

    // Hot-plug watcher.
    let (_watcher, mut hp_rx) = hotplug::watch_v4l2_devices()?;

    // Main event loop — runs until Ctrl-C.
    tokio::select! {
        res = axum::serve(listener, router) => {
            if let Err(e) = res { error!("HTTP server error: {}", e); }
        }
        _ = hotplug_loop(&mut hp_rx, &mut active_capture, &hls_dir) => {}
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down…");
        }
    }

    if let Some(mut mc) = active_capture {
        mc.stop().await;
    }

    Ok(())
}

/// Drive hot-plug events, restarting capture as devices appear/disappear.
async fn hotplug_loop(
    rx: &mut tokio::sync::mpsc::Receiver<hotplug::HotplugEvent>,
    active: &mut Option<capture::MediaCapture>,
    hls_dir: &PathBuf,
) {
    while let Some(event) = rx.recv().await {
        match event {
            hotplug::HotplugEvent::Added(path) => {
                info!("Hot-plug: device added {}", path.display());
                let already_running = active.as_ref().map(|c| c.is_active()).unwrap_or(false);
                if !already_running {
                    let devs = v4l_enum::enumerate();
                    if let Some(dev) = devs.into_iter().find(|d| d.path == path) {
                        let enc = capture::EncoderConfig::software_h264();
                        let mut mc = capture::MediaCapture::new(dev, enc, hls_dir.clone());
                        match mc.start().await {
                            Ok(()) => *active = Some(mc),
                            Err(e) => error!("Failed to start capture on {}: {}", path.display(), e),
                        }
                    }
                }
            }
            hotplug::HotplugEvent::Removed(path) => {
                info!("Hot-plug: device removed {}", path.display());
                if let Some(ref mut mc) = active {
                    if mc.is_active() {
                        mc.stop().await;
                        info!("Capture stopped due to device removal");
                    }
                }
            }
        }
    }
}
