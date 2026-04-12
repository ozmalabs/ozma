//! ozma-node — V4L2 capture → ffmpeg-sidecar → HLS, served on port 7382.
//!
//! Routes
//! ------
//! GET /stream/stream.m3u8        — HLS manifest
//! GET /stream/seg_NNNNN.ts       — HLS segments
//! GET /health                    — liveness probe
//! GET /api/v1/devices            — list detected V4L2 devices (JSON)

mod capture;
mod hotplug;

use std::{
    net::SocketAddr,
    path::PathBuf,
    sync::{Arc, Mutex},
};

use anyhow::Result;
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use capture::{enumerate_v4l2_devices, CaptureDevice, EncoderConfig, MediaCapture};
use hotplug::{watch_v4l2_hotplug, HotplugEvent};
use serde_json::json;
use tokio::sync::mpsc;
use tower_http::{cors::CorsLayer, services::ServeDir};
use tracing::{error, info, warn};

// ── Application state ─────────────────────────────────────────────────────────

#[derive(Clone)]
struct AppState {
    out_dir: PathBuf,
    devices: Arc<Mutex<Vec<CaptureDevice>>>,
}

// ── Route handlers ────────────────────────────────────────────────────────────

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn list_devices(State(state): State<AppState>) -> impl IntoResponse {
    let devs = state.devices.lock().unwrap();
    let list: Vec<serde_json::Value> = devs
        .iter()
        .map(|d| {
            json!({
                "path": d.path,
                "max_width": d.max_width,
                "max_height": d.max_height,
                "formats": d.formats,
                "audio_device": d.audio_device,
            })
        })
        .collect();
    Json(json!({ "devices": list }))
}

// ── Entry point ───────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_node=debug".parse()?),
        )
        .init();

    // Ensure ffmpeg-sidecar can locate (or download) an ffmpeg binary.
    ffmpeg_sidecar::download::auto_download().ok();

    let out_dir = PathBuf::from(
        std::env::var("OZMA_STREAM_DIR").unwrap_or_else(|_| "/tmp/ozma-stream".into()),
    );
    std::fs::create_dir_all(&out_dir)?;

    // Initial device enumeration.
    let devices = enumerate_v4l2_devices();
    info!("Found {} V4L2 device(s)", devices.len());

    let state = AppState {
        out_dir: out_dir.clone(),
        devices: Arc::new(Mutex::new(devices.clone())),
    };

    // Start capture on the first available device (if any).
    let mut capture: Option<MediaCapture> = None;
    if let Some(dev) = devices.into_iter().next() {
        let enc = EncoderConfig::software_h264();
        let mut mc = MediaCapture::new(dev, enc, out_dir.clone());
        mc.start().await?;
        capture = Some(mc);
    } else {
        warn!("No V4L2 devices found at startup — waiting for hot-plug");
    }

    // Hot-plug watcher.
    let (hp_tx, mut hp_rx) = mpsc::channel::<HotplugEvent>(16);
    watch_v4l2_hotplug(hp_tx).await?;

    let state_hp = state.clone();
    let out_dir_hp = out_dir.clone();
    tokio::spawn(async move {
        while let Some(event) = hp_rx.recv().await {
            match event {
                HotplugEvent::Added(path) => {
                    info!("Hot-plug: device added {path}");
                    let new_devs = enumerate_v4l2_devices();
                    *state_hp.devices.lock().unwrap() = new_devs.clone();

                    if capture.as_ref().map(|c| !c.is_active()).unwrap_or(true) {
                        if let Some(dev) = new_devs.into_iter().find(|d| d.path == path) {
                            let enc = EncoderConfig::software_h264();
                            let mut mc = MediaCapture::new(dev, enc, out_dir_hp.clone());
                            match mc.start().await {
                                Ok(()) => capture = Some(mc),
                                Err(e) => error!("Failed to start capture on {path}: {e}"),
                            }
                        }
                    }
                }
                HotplugEvent::Removed(path) => {
                    info!("Hot-plug: device removed {path}");
                    let new_devs = enumerate_v4l2_devices();
                    *state_hp.devices.lock().unwrap() = new_devs;

                    if let Some(ref mc) = capture {
                        if !mc.is_active() {
                            capture = None;
                        }
                    }
                }
            }
        }
    });

    // Axum router — serve HLS files from out_dir under /stream/.
    let app = Router::new()
        .route("/health", get(health))
        .route("/api/v1/devices", get(list_devices))
        .nest_service("/stream", ServeDir::new(&out_dir))
        .layer(CorsLayer::permissive())
        .with_state(state);

    let addr: SocketAddr = "0.0.0.0:7382".parse()?;
    info!("ozma-node listening on {addr}");
    let listener = tokio::net::TcpListener::bind(addr).await?;

    tokio::select! {
        res = axum::serve(listener, app) => {
            if let Err(e) = res { error!("HTTP server error: {e}"); }
        }
        _ = tokio::signal::ctrl_c() => {
            info!("Shutting down…");
        }
    }

    Ok(())
}
