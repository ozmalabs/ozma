use axum::{
    extract::{State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;
use tower_http::cors::CorsLayer;

use crate::agent::state::AgentState;

pub fn create_router(state: Arc<RwLock<AgentState>>) -> Router {
    Router::new()
        .route("/health", get(health_check))
        .route("/api/v1/displays", get(get_displays))
        .route("/api/v1/audio/devices", get(get_audio_devices))
        .route("/api/v1/hid/inject", post(inject_hid))
        .route("/api/v1/clipboard", get(get_clipboard).post(set_clipboard))
        .route("/api/v1/metrics", get(get_metrics))
        // WebSocket route would be added here
        .layer(CorsLayer::permissive())
        .with_state(state)
}

async fn health_check() -> impl IntoResponse {
    Json(serde_json::json!({"status": "ok"}))
}

async fn get_displays(
    State(state): State<Arc<RwLock<AgentState>>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let state = state.read().await;
    // In a real implementation, this would fetch actual display information
    let displays = serde_json::json!({
        "displays": [
            {
                "id": "display-0",
                "width": 1920,
                "height": 1080,
                "name": "Primary Display"
            }
        ]
    });
    Ok(Json(displays))
}

async fn get_audio_devices(
    State(state): State<Arc<RwLock<AgentState>>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let state = state.read().await;
    // In a real implementation, this would fetch actual audio devices
    let devices = serde_json::json!({
        "devices": [
            {
                "id": "audio-0",
                "name": "Default Audio Device",
                "type": "output"
            }
        ]
    });
    Ok(Json(devices))
}

#[derive(Deserialize)]
struct HidInjectRequest {
    report_type: String, // "keyboard" or "mouse"
    data: Vec<u8>,
}

async fn inject_hid(
    State(state): State<Arc<RwLock<AgentState>>>,
    Json(payload): Json<HidInjectRequest>,
) -> Result<impl IntoResponse, AppError> {
    let state = state.read().await;
    // In a real implementation, this would inject HID events
    match payload.report_type.as_str() {
        "keyboard" => {
            // Inject keyboard event
        }
        "mouse" => {
            // Inject mouse event
        }
        _ => return Err(AppError::BadRequest("Invalid report_type".to_string())),
    }
    
    Ok(Json(serde_json::json!({"status": "ok"})))
}

async fn get_clipboard(
    State(state): State<Arc<RwLock<AgentState>>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let state = state.read().await;
    // In a real implementation, this would fetch actual clipboard content
    let clipboard = serde_json::json!({
        "content": "Sample clipboard content"
    });
    Ok(Json(clipboard))
}

#[derive(Deserialize)]
struct SetClipboardRequest {
    content: String,
}

async fn set_clipboard(
    State(state): State<Arc<RwLock<AgentState>>>,
    Json(payload): Json<SetClipboardRequest>,
) -> Result<impl IntoResponse, AppError> {
    let state = state.read().await;
    // In a real implementation, this would set the clipboard content
    Ok(Json(serde_json::json!({"status": "ok"})))
}

async fn get_metrics(
    State(state): State<Arc<RwLock<AgentState>>>,
) -> Result<Json<serde_json::Value>, AppError> {
    let state = state.read().await;
    // In a real implementation, this would fetch actual system metrics
    let metrics = serde_json::json!({
        "cpu_usage": 0.25,
        "memory_usage": 0.45,
        "disk_usage": 0.65
    });
    Ok(Json(metrics))
}

// Error handling
#[derive(Debug)]
enum AppError {
    BadRequest(String),
    InternalError(anyhow::Error),
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, error_message) = match self {
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg),
            AppError::InternalError(err) => {
                eprintln!("Internal error: {:?}", err);
                (StatusCode::INTERNAL_SERVER_ERROR, "Internal Server Error".to_string())
            }
        };

        let body = Json(serde_json::json!({
            "error": error_message,
        }));

        (status, body).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(inner: anyhow::Error) -> Self {
        AppError::InternalError(inner)
    }
}
