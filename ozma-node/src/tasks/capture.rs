//! Display capture pipeline — spawns ffmpeg to capture a V4L2 device and
//! segment it into an HLS playlist for streaming.

use anyhow::Result;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

pub async fn run(device: String, cancel: CancellationToken) -> Result<()> {
    info!(device = %device, "Capture pipeline starting");
    // TODO: spawn ffmpeg -f v4l2 -i <device> -codec:v libx264 -hls_time 1 ...
    cancel.cancelled().await;
    warn!("Capture pipeline shutting down (not yet implemented)");
    Ok(())
}
