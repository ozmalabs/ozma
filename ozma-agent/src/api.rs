//! Axum HTTP API server.
//!
//! Endpoints
//! ---------
//! GET  /healthz          — liveness probe (200 OK)
//! GET  /api/v1/status    — agent status JSON
//! GET  /api/v1/version   — version string

use anyhow::Result;
use axum::{routing::get, Json, Router};
use serde::Serialize;
use tracing::info;

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

/// Start the API server and block until it exits.
pub async fn serve(addr: String) -> Result<()> {
    let app = Router::new()
        .route("/healthz",         get(healthz))
        .route("/api/v1/status",   get(status))
        .route("/api/v1/version",  get(version));

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(addr, "API server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
