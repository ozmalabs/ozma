//! ozma-drivers — virtual display / IDD driver helper daemon.
//!
//! Spawns one async task per detected control surface and exposes a
//! Unix-domain-socket IPC server that streams newline-delimited JSON
//! [`ControlEvent`] messages to every connected client (e.g. the Python
//! controller).
//!
//! Usage:
//!   ozma-drivers --ipc /tmp/ozma-drivers.sock

mod surface;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing::info;
use tokio::{io::AsyncWriteExt, net::UnixListener};

/// ozma-drivers daemon — bridges hardware surfaces to the ozma controller.
#[derive(Parser, Debug)]
#[command(author, version, about)]
struct Cli {
    /// Path for the Unix-domain IPC socket.
    #[arg(long, default_value = "/tmp/ozma-drivers.sock")]
    ipc: PathBuf,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::from_default_env()
                .add_directive("ozma_drivers=debug".parse()?),
        )
        .init();

    let cli = Cli::parse();

    info!("ozma-drivers starting (ipc={})", cli.ipc.display());

    // Broadcast channel: surface tasks → IPC server → all connected clients.
    // Capacity 256 — slow clients drop old events rather than blocking surfaces.
    let (tx, _) = broadcast::channel::<String>(256);

    // Detect surfaces and spawn a task for each.
    let surfaces = surface::detect().await;
    if surfaces.is_empty() {
        info!("No control surfaces detected — daemon running in IPC-only mode");
    }
    for s in surfaces {
        let tx2 = tx.clone();
        tokio::spawn(async move {
            surface::run(s, tx2).await;
        });
    }

    // Run the IPC server (blocks until signal).
    run_unix_socket_server(cli.ipc, tx).await?;

    info!("ozma-drivers stopped");
    Ok(())
}

async fn run_unix_socket_server(
    socket_path: PathBuf,
    tx: broadcast::Sender<String>,
) -> Result<()> {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(&socket_path);
    
    let listener = UnixListener::bind(&socket_path)?;
    info!("Listening on Unix socket: {}", socket_path.display());

    // Accept connections
    loop {
        let (mut socket, addr) = listener.accept().await?;
        info!("New IPC client connected: {:?}", addr);
        
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            let mut rx = tx_clone.subscribe();
            loop {
                match rx.recv().await {
                    Ok(event_json) => {
                        let msg = format!("{}\n", event_json);
                        if let Err(e) = socket.write_all(msg.as_bytes()).await {
                            tracing::warn!("Failed to send event to client: {}", e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        tracing::warn!("Missed events for client due to channel overflow");
                    }
                }
            }
        });
    }
}
