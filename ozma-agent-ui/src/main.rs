use anyhow::Result;
use clap::Parser;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

mod app;
mod assets;
mod client;
mod tray;

use crate::app::OzmaApp;
use crate::tray::{TrayManager, TrayStatus};

/// CLI arguments for the Ozma Agent UI
#[derive(Parser, Debug)]
#[command(author, version, about = "Ozma Agent Manager GUI", long_about = None)]
struct Args {
    /// URL of the Ozma Agent to connect to
    #[arg(long, default_value = "http://localhost:7381")]
    agent_url: String,
}

fn main() -> Result<()> {
    // 1. Initialize tracing/logging
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    // 2. Parse CLI arguments
    let args = Args::parse();

    tracing::info!("Starting Ozma Agent UI");
    tracing::info!("Target agent URL: {}", args.agent_url);

    // 3. Create the tray icon with initial status (grey = disconnected)
    let _tray_manager = TrayManager::new(TrayStatus::Disconnected)?;

    // 4. Run the eframe event loop (GUI)
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Ozma Agent Manager",
        native_options,
        Box::new(|_cc| Ok(Box::new(OzmaApp::new(args.agent_url)))),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    Ok(())
}
