// SPDX-License-Identifier: AGPL-3.0-only
//! ozma-drivers binary entry point.
//!
//! Run with `RUST_LOG=ozma_drivers=debug` for verbose output.
//!
//! Usage:
//!   ozma-drivers kbd                   # list detected keyboard devices
//!   ozma-drivers mouse                 # list detected mouse devices
//!   ozma-drivers surface <config.json> # run a config-driven evdev surface

use anyhow::{Context, Result};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("ozma_drivers=debug".parse()?),
        )
        .init();

    let args: Vec<String> = std::env::args().collect();
    let cmd = args.get(1).map(String::as_str).unwrap_or("help");

    match cmd {
        "kbd" => {
            let devices =
                ozma_drivers::evdev_capture::list_devices(ozma_drivers::evdev_capture::is_keyboard);
            println!("Detected keyboard devices ({}):", devices.len());
            for path in &devices {
                if let Ok(dev) = evdev::Device::open(path) {
                    println!("  {:?} — {}", path, dev.name().unwrap_or("?"));
                }
            }
        }

        "mouse" => {
            let devices =
                ozma_drivers::evdev_capture::list_devices(ozma_drivers::evdev_capture::is_mouse);
            println!("Detected mouse devices ({}):", devices.len());
            for path in &devices {
                if let Ok(dev) = evdev::Device::open(path) {
                    println!("  {:?} — {}", path, dev.name().unwrap_or("?"));
                }
            }
        }

        "surface" => {
            use ozma_drivers::control_surface::ControlSurface;

            let config_path = args.get(2).context("missing <config.json>")?;
            let raw = std::fs::read_to_string(config_path)
                .with_context(|| format!("reading {config_path}"))?;
            let config: serde_json::Value =
                serde_json::from_str(&raw).context("parsing config JSON")?;

            let surface_id = config
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or("surface0")
                .to_string();

            let mut surface =
                ozma_drivers::evdev_surface::EvdevSurface::new(surface_id, &config);
            surface.start().await.context("starting surface")?;

            info!("Surface running — press Ctrl-C to stop");
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    evt = surface.next_event() => {
                        match evt {
                            Some(e) => info!(
                                surface = %e.surface_id,
                                control = %e.control_name,
                                action  = %e.binding.action,
                                value   = %e.value,
                                "control event"
                            ),
                            None => {
                                info!("Surface shut down");
                                break;
                            }
                        }
                    }
                }
            }

            surface.stop().await.context("stopping surface")?;
            info!("stopped");
        }

        _ => {
            eprintln!("ozma-drivers — evdev capture and uinput virtual devices");
            eprintln!();
            eprintln!("Commands:");
            eprintln!("  kbd                    List detected keyboard devices");
            eprintln!("  mouse                  List detected mouse devices");
            eprintln!("  surface <config.json>  Run a config-driven evdev control surface");
        }
    }

    Ok(())
}
