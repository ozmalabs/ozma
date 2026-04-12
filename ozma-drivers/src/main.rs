//! ozma-drivers — virtual display / IDD driver helper daemon.

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_drivers=debug".parse()?),
        )
        .init();

    info!("ozma-drivers starting");
    // TODO: virtual display driver control (Ozma VDD, Parsec VDD, Amyuni).
    Ok(())
}
