//! ozma-drivers — evdev capture, uinput virtual devices, HID forwarding.
//!
//! Run with `RUST_LOG=ozma_drivers=debug` for verbose output.
//!
//! Usage:
//!   ozma-drivers kbd                  # list detected keyboard devices
//!   ozma-drivers mouse                # list detected mouse devices
//!   ozma-drivers forward <host> <port> # capture all devices and forward to node

use anyhow::{Context, Result};
use std::sync::{Arc, Mutex};
use tracing::info;

use ozma_drivers::hid_forwarder::{
    ActiveNode, HidForwarder, HidForwarderConfig, NodeTarget,
    find_keyboard_devices, find_mouse_devices,
};

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_drivers=debug".parse()?),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "kbd" => {
            println!("Detected keyboard devices:");
            for dev in find_keyboard_devices(false) {
                println!(
                    "  {} — {}",
                    dev.physical_path().unwrap_or("?"),
                    dev.name().unwrap_or("?"),
                );
            }
        }

        "mouse" => {
            println!("Detected mouse devices:");
            for dev in find_mouse_devices(false) {
                println!(
                    "  {} — {}",
                    dev.physical_path().unwrap_or("?"),
                    dev.name().unwrap_or("?"),
                );
            }
        }

        "forward" => {
            let host = args.get(2).context("missing <host>")?.clone();
            let port: u16 = args
                .get(3)
                .context("missing <port>")?
                .parse()
                .context("port must be a number")?;

            info!(host = %host, port = port, "starting HID forwarder");

            let active_node: ActiveNode = Arc::new(Mutex::new(Some(NodeTarget {
                id: format!("{}:{}", host, port),
                host,
                port,
            })));

            let config = HidForwarderConfig {
                debug: std::env::var("OZMA_DEBUG").is_ok(),
                ..Default::default()
            };

            let mut forwarder = HidForwarder::new(config, active_node)
                .await
                .context("failed to create HID forwarder")?;

            forwarder.start().await;

            info!("capturing — press Ctrl-C to stop");
            tokio::signal::ctrl_c().await?;
            forwarder.stop().await;
            info!("stopped");
        }

        _ => {
            eprintln!("ozma-drivers — evdev capture and HID forwarding");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  kbd                    List detected keyboard devices");
            eprintln!("  mouse                  List detected mouse devices");
            eprintln!("  forward <host> <port>  Capture and forward HID to node");
        }
    }

    Ok(())
}
