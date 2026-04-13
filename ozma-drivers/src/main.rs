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
            let devices = ozma_drivers::find_keyboard_devices(false);
            println!("Detected keyboard devices ({}):", devices.len());
            for dev in &devices {
                println!("  {} — {}", dev.physical_path().map(|p| p.display().to_string()).unwrap_or_default(), dev.name().unwrap_or("?"));
            }
        }

        "mouse" => {
            let devices = ozma_drivers::find_mouse_devices(false);
            println!("Detected mouse devices ({}):", devices.len());
            for dev in &devices {
                println!("  {} — {}", dev.physical_path().map(|p| p.display().to_string()).unwrap_or_default(), dev.name().unwrap_or("?"));
            }
        }

        "surface" => {
            use ozma_drivers::control_surface::ControlSurface;
            use ozma_drivers::evdev_capture::{EvdevSurface, EvdevSurfaceConfig};

            let config_path = args.get(2).context("missing <config.json>")?;
            let raw = std::fs::read_to_string(config_path)
                .with_context(|| format!("reading {config_path}"))?;
            let cfg: EvdevSurfaceConfig =
                serde_json::from_str(&raw).context("parsing config JSON")?;

            let surface_id = cfg.device.clone();
            let mut surface = EvdevSurface::new(surface_id, cfg);
            let mut rx = surface
                .start()
                .await
                .context("starting surface")?;

            info!("Surface running — press Ctrl-C to stop");
            loop {
                tokio::select! {
                    _ = tokio::signal::ctrl_c() => break,
                    evt = rx.recv() => {
                        match evt {
                            Some(e) => info!(
                                surface = %e.surface_id,
                                control = %e.control_name,
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

            surface.stop().await;
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
