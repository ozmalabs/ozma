//! ozma-softnode — software-only node daemon (evdev → QMP bridge).

use anyhow::Result;
use tracing::info;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_softnode=debug".parse()?),
        )
        .init();

    info!("ozma-softnode starting");
    // TODO: open evdev devices, connect to QEMU QMP socket, forward HID events.
    Ok(())
}
