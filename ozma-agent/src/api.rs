//! Axum HTTP API server.
//!
//! Public read-only endpoints only — sensitive approval/event traffic
//! goes through the privileged IPC socket (ipc_server.rs).

use crate::approvals::ApprovalQueue;
use anyhow::Result;
use axum::{
    extract::{
        ws::{Message, WebSocket, WebSocketUpgrade},
        State,
    },
    http::StatusCode,
    Json, Router,
};
use serde::Serialize;
use std::sync::Arc;
use tokio::sync::broadcast;
use tokio_stream::wrappers::BroadcastStream;

#[derive(Clone)]
pub struct AppState {
    pub queue: Arc<ApprovalQueue>,
}

#[derive(Serialize)]
struct Status {
    status: &'static str,
    version: &'static str,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn status(State(state): State<AppState>) -> Json<serde_json::Value> {
    let approvals = state.queue.list_pending().await;
    Json(serde_json::json!({
        "status": "running",
        "version": env!("CARGO_PKG_VERSION"),
        "pending_approvals": approvals.len(),
    }))
}

async fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// WebSocket endpoint for real-time agent events.
/// Clients subscribe to receive approval requests and status updates.
async fn ws_events_handler(
    ws: WebSocketUpgrade,
    State(state): State<AppState>,
) -> axum::response::Response {
    ws.on_upgrade(|socket| ws_events_stream(socket, state.queue))
}

async fn ws_events_stream(socket: WebSocket, queue: Arc<ApprovalQueue>) {
    use crate::approvals::AgentEvent;

    let mut rx = queue.tx.subscribe();

    // Send initial connected event
    let connected_msg = serde_json::json!({
        "type": "connected",
        "message": "WebSocket connection established"
    });

    if socket.send(Message::Text(connected_msg.to_string())).await.is_err() {
        return;
    }

    // Create a BroadcastStream for better async compatibility
    let stream = BroadcastStream::new(rx);
    tokio::pin!(stream);

    loop {
        tokio::select! {
            // Handle incoming WebSocket messages
            msg = socket.recv() => {
                match msg {
                    Ok(Message::Ping(data)) => {
                        if socket.send(Message::Pong(data)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Message::Text(text)) => {
                        // Handle incoming client messages (e.g., ping)
                        if text.trim() == "ping" {
                            let pong = serde_json::json!({"type": "pong"});
                            if socket.send(Message::Text(pong.to_string())).await.is_err() {
                                break;
                            }
                        }
                    }
                    Ok(Message::Close(_)) | Err(_) => {
                        break;
                    }
                    _ => {}
                }
            }
            // Forward events to WebSocket client
            event = stream.next() => {
                match event {
                    Some(Ok(event)) => {
                        let json = serde_json::to_string(&event).unwrap_or_default();
                        if socket.send(Message::Text(json)).await.is_err() {
                            break;
                        }
                    }
                    Some(Err(broadcast::error::RecvError::Lagged(_))) => {
                        // Client fell behind, skip
                        continue;
                    }
                    None => {
                        // Broadcast channel closed
                        break;
                    }
                }
            }
        }
    }
}

/// Start the HTTP API server with pre-created state.
pub async fn serve_with_queue(addr: String, queue: Arc<ApprovalQueue>) -> Result<()> {
    let app = Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/api/v1/status", axum::routing::get(status))
        .route("/api/v1/version", axum::routing::get(version))
        .route("/ws/events", axum::routing::get(ws_events_handler))
        .with_state(AppState { queue });

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr, "API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}

/// Start the HTTP API server with auto-created state (for standalone use).
pub async fn serve(addr: String) -> Result<()> {
    let queue = ApprovalQueue::new();
    serve_with_queue(addr, queue).await
}

/// Start the HTTP API server with custom state.
pub async fn serve_with_state(addr: String, state: AppState) -> Result<()> {
    let app = Router::new()
        .route("/healthz", axum::routing::get(healthz))
        .route("/api/v1/status", axum::routing::get(status))
        .route("/api/v1/version", axum::routing::get(version))
        .route("/ws/events", axum::routing::get(ws_events_handler))
        .with_state(state);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr, "API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
