//! USB HID gadget UDP bridge.
//!
//! Binds UDP port 7331, receives newline-delimited JSON `HidFrame` datagrams
//! from ozma-proto, and writes the encoded HID reports to `/dev/hidg0`
//! (keyboard) and `/dev/hidg1` (mouse).
//!
//! Frame format (JSON):
//!   {"keyboard": {"modifiers": 2, "keys": [4]}}
//!   {"mouse": {"buttons": 1, "x": 16383, "y": 16383, "scroll": 0}}

mod gadget;
mod report;

use std::net::SocketAddr;

use tokio::net::UdpSocket;
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use crate::gadget::HidGadget;
use crate::report::HidFrame;

const UDP_PORT:    u16  = 7331;
const MAX_DGRAM:   usize = 4096;
const KBD_PATH:    &str = "/dev/hidg0";
const MOUSE_PATH:  &str = "/dev/hidg1";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env()
            .add_directive("usb_hid_udp=info".parse()?))
        .init();

    let bind_addr: SocketAddr = format!("0.0.0.0:{UDP_PORT}").parse()?;
    let sock = UdpSocket::bind(bind_addr).await?;
    info!("Listening on UDP {bind_addr}");

    let mut gadget = HidGadget::open(KBD_PATH, MOUSE_PATH).await;

    let mut buf = vec![0u8; MAX_DGRAM];
    loop {
        let (n, peer) = match sock.recv_from(&mut buf).await {
            Ok(v)  => v,
            Err(e) => { error!("UDP recv error: {e}"); continue; }
        };

        let data = &buf[..n];
        let frame: HidFrame = match serde_json::from_slice(data) {
            Ok(f)  => f,
            Err(e) => {
                warn!("Bad HidFrame from {peer}: {e}");
                continue;
            }
        };

        if let Some(kbd) = frame.keyboard {
            gadget.write_keyboard(&kbd).await;
        }
        if let Some(mouse) = frame.mouse {
            gadget.write_mouse(&mouse).await;
        }
    }
}
