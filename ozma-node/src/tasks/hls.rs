//! HLS / REST API HTTP server — serves the video stream playlist and static
//! assets over HTTP on the node's API port.

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::info;

pub async fn run(port: u16, cancel: CancellationToken) -> Result<()> {
    info!(port, "HLS/REST server starting");
    // TODO: implement axum HTTP server for HLS playlist and REST endpoints.
    cancel.cancelled().await;
    info!("HLS task shutting down");
    Ok(())
}
