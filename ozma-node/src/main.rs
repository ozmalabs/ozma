//! ozma-node — hardware KVM node daemon.

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_node=debug".parse()?),
        )
        .init();

    info!("ozma-node starting");
    // TODO: initialise USB HID gadget, mesh tunnel, and packet loop.
    Ok(())
}
