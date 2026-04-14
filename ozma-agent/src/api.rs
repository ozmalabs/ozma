//! Axum HTTP API server.
//!
//! Public read-only endpoints only — sensitive approval/event traffic
//! goes through the privileged IPC socket (ipc_server.rs).

use anyhow::Result;
use axum::{routing::get, Json, Router};
use serde::Serialize;

#[derive(Serialize)]
struct Status {
    status:  &'static str,
    version: &'static str,
}

async fn healthz() -> &'static str {
    "ok"
}

async fn status() -> Json<Status> {
    Json(Status {
        status:  "running",
        version: env!("CARGO_PKG_VERSION"),
    })
}

async fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Start the public read-only HTTP API server and block until it exits.
/// Approval endpoints are NOT exposed here — see ipc_server.rs for that traffic.
pub async fn serve(addr: String) -> Result<()> {
    let app = Router::new()
        .route("/healthz",        get(healthz))
        .route("/api/v1/status",  get(status))
        .route("/api/v1/version", get(version));

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    tracing::info!(addr, "API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
