//! Prometheus metrics registry and scrape endpoint.
//!
//! Exposes a `/metrics` endpoint on a dedicated port so Prometheus can scrape
//! the agent without going through the main API server.

use anyhow::Result;
use axum::{routing::get, Router};
use prometheus::{Encoder, Registry, TextEncoder, process_collector::ProcessCollector};
use std::sync::Arc;
use tracing::info;

/// Build a Prometheus registry pre-populated with process metrics.
pub fn build_registry() -> Arc<Registry> {
    let registry = Registry::new();
    let pc = ProcessCollector::for_self();
    registry.register(Box::new(pc)).ok();
    Arc::new(registry)
}

/// Axum handler: render all metrics as Prometheus text exposition format.
async fn scrape(
    axum::extract::State(registry): axum::extract::State<Arc<Registry>>,
) -> axum::response::Response<String> {
    let encoder = TextEncoder::new();
    let mut buf = Vec::new();
    encoder
        .encode(&registry.gather(), &mut buf)
        .unwrap_or_default();
    axum::response::Response::builder()
        .header("Content-Type", encoder.format_type())
        .body(String::from_utf8_lossy(&buf).into_owned())
        .unwrap()
}

/// Start the metrics scrape server and block until it exits.
pub async fn serve(addr: String, registry: Arc<Registry>) -> Result<()> {
    let app = Router::new()
        .route("/metrics", get(scrape))
        .with_state(registry);

    let listener = tokio::net::TcpListener::bind(&addr).await?;
    info!(addr, "metrics server listening");
    axum::serve(listener, app).await?;
    Ok(())
}
