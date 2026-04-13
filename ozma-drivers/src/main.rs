//! ozma-drivers — virtual display / IDD driver helper daemon.
//!
//! Spawns one async task per detected control surface and exposes a
//! Unix-domain-socket IPC server that streams newline-delimited JSON
//! [`ControlEvent`] messages to every connected client (e.g. the Python
//! controller).
//!
//! Usage:
//!   ozma-drivers --ipc /tmp/ozma-drivers.sock

mod ipc;
mod surface;

use anyhow::Result;
use clap::Parser;
use std::path::PathBuf;
use tokio::sync::broadcast;
use tracing::info;

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
    ipc::serve(cli.ipc, tx).await?;

    info!("ozma-drivers stopped");
    Ok(())
}
//! Ozma Drivers - Control surface daemon
//!
//! Detects and manages control surfaces (MIDI, HID, etc.) and communicates
//! with the Python controller via IPC (Unix domain socket).

use clap::Parser;
use tokio::{io::AsyncWriteExt, net::UnixListener};
use tracing::{info, warn};

mod surface;

/// Ozma Drivers CLI
#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// IPC socket path
    #[arg(short, long, default_value = "/tmp/ozma-drivers.sock")]
    ipc: String,
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    tracing_subscriber::fmt::init();
    
    let args = Args::parse();
    
    info!("Starting ozma-drivers daemon");
    
    // Create broadcast channel for control events
    let (tx, _rx) = tokio::sync::broadcast::channel::<String>(100);
    
    // Detect surfaces
    let surfaces = surface::detect().await;
    info!("Detected {} surfaces", surfaces.len());
    
    // Spawn surface tasks
    for surface in surfaces {
        let tx_clone = tx.clone();
        tokio::spawn(async move {
            surface::run(surface, tx_clone).await;
        });
    }
    
    // Start IPC server
    run_unix_socket_server(&args.ipc, tx).await?;
    
    Ok(())
}

async fn run_unix_socket_server(
    socket_path: &str,
    tx: tokio::sync::broadcast::Sender<String>,
) -> Result<(), Box<dyn std::error::Error>> {
    // Remove existing socket file if it exists
    let _ = std::fs::remove_file(socket_path);
    
    let listener = UnixListener::bind(socket_path)?;
    info!("Listening on Unix socket: {}", socket_path);
    
    // Spawn receiver task to forward events to all connected clients
    let mut rx = tx.subscribe();
    tokio::spawn(async move {
        loop {
            match rx.recv().await {
                Ok(event_json) => {
                    // In a real implementation, we'd send this to connected clients
                    info!("Event: {}", event_json);
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                    warn!("Missed events due to channel overflow");
                }
            }
        }
    });
    
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
                            warn!("Failed to send event to client: {}", e);
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        warn!("Missed events for client due to channel overflow");
                    }
                }
            }
        });
    }
}
