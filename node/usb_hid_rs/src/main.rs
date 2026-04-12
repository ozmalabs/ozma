//! ozma-usb-hid — USB HID gadget writer
//!
//! Binds UDP port 7331, receives newline-delimited JSON HidReport frames from
//! ozma-proto, and writes them to /dev/hidg0 (keyboard) / /dev/hidg1 (mouse).
//!
//! Environment variables:
//!   OZMA_HID_KBD_PATH   — keyboard gadget device (default /dev/hidg0)
//!   OZMA_HID_MOUSE_PATH — mouse gadget device    (default /dev/hidg1)
//!   OZMA_HID_BIND       — UDP bind address        (default 0.0.0.0:7331)
//!   RUST_LOG            — tracing filter           (default info)

mod gadget;
mod report;

use std::sync::Arc;

use tokio::net::UdpSocket;
use tracing::{debug, error, info, warn};

use crate::gadget::UsbHidGadget;
use crate::report::HidReport;

const DEFAULT_KBD_PATH:   &str = "/dev/hidg0";
const DEFAULT_MOUSE_PATH: &str = "/dev/hidg1";
const DEFAULT_BIND:       &str = "0.0.0.0:7331";
/// Maximum UDP datagram we'll accept (well above any HID JSON frame).
const MAX_DATAGRAM: usize = 4096;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .init();

    let kbd_path   = std::env::var("OZMA_HID_KBD_PATH")
        .unwrap_or_else(|_| DEFAULT_KBD_PATH.to_string());
    let mouse_path = std::env::var("OZMA_HID_MOUSE_PATH")
        .unwrap_or_else(|_| DEFAULT_MOUSE_PATH.to_string());
    let bind_addr  = std::env::var("OZMA_HID_BIND")
        .unwrap_or_else(|_| DEFAULT_BIND.to_string());

    let gadget = Arc::new(UsbHidGadget::open(&kbd_path, &mouse_path).await);

    let sock = UdpSocket::bind(&bind_addr).await?;
    info!("ozma-usb-hid listening on udp/{}", bind_addr);

    let mut buf = vec![0u8; MAX_DATAGRAM];

    loop {
        let (len, peer) = match sock.recv_from(&mut buf).await {
            Ok(v) => v,
            Err(e) => {
                error!("UDP recv error: {}", e);
                continue;
            }
        };

        let payload = &buf[..len];
        debug!("recv {} bytes from {}", len, peer);

        // Each datagram may contain one JSON object (newline optional).
        let text = match std::str::from_utf8(payload) {
            Ok(s) => s.trim(),
            Err(e) => {
                warn!("non-UTF-8 datagram from {}: {}", peer, e);
                continue;
            }
        };

        let report: HidReport = match serde_json::from_str(text) {
            Ok(r) => r,
            Err(e) => {
                warn!("bad HID frame from {}: {} — {:?}", peer, e, text);
                continue;
            }
        };

        let g = Arc::clone(&gadget);
        tokio::spawn(async move {
            match report {
                HidReport::Keyboard(k) => g.write_keyboard(&k).await,
                HidReport::Mouse(m)    => g.write_mouse(&m).await,
            }
        });
    }
}
