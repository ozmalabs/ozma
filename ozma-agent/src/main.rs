//! ozma-agent — desktop seat agent daemon.

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_agent=debug".parse()?),
        )
        .init();

    info!("ozma-agent starting");
    // TODO: seat management, audio sink lifecycle, display backend.
    Ok(())
}
