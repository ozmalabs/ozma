//! ozma-clipboard binary — clipboard agent for the Ozma mesh.
//!
//! Usage:
//!   ozma-clipboard [--name <machine-name>] [--watch-interval-ms <ms>]
//!
//! Starts the IPC server and a background clipboard-watch loop.

use std::sync::Arc;

use ozma_clipboard::{ipc, ClipboardManager};
use tracing::info;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() {
    // Logging: RUST_LOG=ozma_clipboard=debug or default INFO
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("ozma_clipboard=info")),
        )
        .init();

    // CLI args (minimal — no clap dep to keep binary small)
    let args: Vec<String> = std::env::args().collect();
    let name = arg_value(&args, "--name").unwrap_or_else(|| {
        hostname::get()
            .ok()
            .and_then(|h| h.into_string().ok())
            .unwrap_or_else(|| "ozma-node".to_owned())
    });
    let watch_ms: u64 = arg_value(&args, "--watch-interval-ms")
        .and_then(|v| v.parse().ok())
        .unwrap_or(500);

    info!("ozma-clipboard starting (node={name}, watch={watch_ms}ms)");

    let mgr = ClipboardManager::new(&name);

    // Background clipboard-change watcher
    let watch_mgr = Arc::clone(&mgr);
    tokio::spawn(async move {
        watch_mgr.start_watch_loop(watch_ms).await;
    });

    // IPC server (blocks until process exits)
    ipc::serve(mgr).await;
}

fn arg_value(args: &[String], flag: &str) -> Option<String> {
    args.windows(2)
        .find(|w| w[0] == flag)
        .map(|w| w[1].clone())
}
