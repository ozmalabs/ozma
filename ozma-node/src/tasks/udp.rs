//! UDP HID receiver — listens on the node's HID UDP port and writes reports
//! to /dev/hidg0 (keyboard) and /dev/hidg1 (mouse).

use anyhow::Result;
use tokio::net::UdpSocket;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

pub async fn run(port: u16, cancel: CancellationToken) -> Result<()> {
    let addr = format!("0.0.0.0:{port}");
    let sock = UdpSocket::bind(&addr).await?;
    info!(port, "UDP HID receiver listening");

    let mut buf = [0u8; 65535];
    loop {
        tokio::select! {
            _ = cancel.cancelled() => {
                info!("UDP task shutting down");
                break;
            }
            result = sock.recv_from(&mut buf) => {
                match result {
                    Ok((n, peer)) => {
                        debug!(peer = %peer, bytes = n, "HID packet received");
                        // TODO: parse HID report type and write to /dev/hidg0 or /dev/hidg1
                    }
                    Err(e) => warn!("UDP recv error: {e}"),
                }
            }
        }
    }

    Ok(())
}
