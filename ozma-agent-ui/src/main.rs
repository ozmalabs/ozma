use anyhow::Result;
use clap::Parser;
use crate::app::OzmaApp;
use crate::tray::TrayManager;
use tracing_subscriber::{fmt, prelude::*, EnvFilter};

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// URL of the Ozma Agent
    #[arg(long, default_value = "http://localhost:7381")]
    agent_url: String,
}

mod app;
mod client;
mod tray;

fn main() -> Result<()> {
    // 1. Init tracing
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(EnvFilter::from_default_env().add_directive(tracing::Level::INFO.into()))
        .init();

    // 2. Parse CLI args
    let args = Args::parse();

    // 3. Create the tray icon
    let _tray_manager = TrayManager::new()?;

    // 4. Run the eframe event loop
    let native_options = eframe::NativeOptions::default();
    eframe::run_native(
        "Ozma Agent Manager",
        native_options,
        Box::new(|_cc| Ok(Box::new(OzmaApp::new(args.agent_url)))),
    )
    .map_err(|e| anyhow::anyhow!(e.to_string()))?;

    Ok(())
}
