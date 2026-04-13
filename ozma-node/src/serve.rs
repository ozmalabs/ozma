//! Axum HTTP server — serves HLS manifest and segments on port 7382.
//!
//! Routes:
//!   GET /stream/stream.m3u8   — HLS manifest
//!   GET /stream/seg_*.ts      — HLS segments
//!   GET /health               — liveness probe → 200 OK
//!   GET /devices              — JSON list of detected V4L2 devices

use std::path::PathBuf;
use std::sync::Arc;

use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use serde::Serialize;
use tower_http::services::ServeDir;

use crate::v4l_enum::CaptureDevice;

#[derive(Clone)]
pub struct AppState {
    pub devices: Arc<Vec<CaptureDevice>>,
}

#[derive(Serialize)]
struct DeviceInfo {
    path: String,
    name: String,
    formats: Vec<String>,
    max_width: u32,
    max_height: u32,
    audio_device: Option<String>,
}

pub fn build_router(hls_dir: PathBuf, state: AppState) -> Router {
    Router::new()
        .route("/health", get(health))
        .route("/devices", get(list_devices))
        // Serve the entire HLS output directory under /stream/
        .nest_service("/stream", ServeDir::new(hls_dir))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, "ok")
}

async fn list_devices(State(state): State<AppState>) -> impl IntoResponse {
    let infos: Vec<DeviceInfo> = state
        .devices
        .iter()
        .map(|d| DeviceInfo {
            path: d.path.to_string_lossy().into_owned(),
            name: d.name.clone(),
            formats: d.formats.clone(),
            max_width: d.max_width,
            max_height: d.max_height,
            audio_device: d.audio_device.clone(),
        })
        .collect();
    Json(infos)
}
