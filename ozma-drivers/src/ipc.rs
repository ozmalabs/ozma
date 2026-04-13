//! Unix-domain-socket IPC server.
//!
//! Each connected client receives every [`ControlEvent`] as a
//! newline-delimited JSON string.  The server also accepts newline-delimited
//! JSON commands from clients (reserved for future use; currently echoed as
//! `{"ok":true}`).

use anyhow::Result;
use std::path::PathBuf;
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::UnixListener,
    sync::broadcast,
};
use tracing::{debug, info, warn};

/// Start the IPC server and block until the process receives SIGINT/SIGTERM.
pub async fn serve(sock_path: PathBuf, tx: broadcast::Sender<String>) -> Result<()> {
    // Remove stale socket file from a previous run.
    if sock_path.exists() {
        std::fs::remove_file(&sock_path)?;
    }

    let listener = UnixListener::bind(&sock_path)?;
    info!("IPC server listening on {}", sock_path.display());

    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                let rx = tx.subscribe();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(stream, rx).await {
                        debug!("IPC client disconnected: {e}");
                    }
                });
            }
            Err(e) => {
                warn!("IPC accept error: {e}");
            }
        }
    }
}

async fn handle_client(
    stream: tokio::net::UnixStream,
    mut rx: broadcast::Receiver<String>,
) -> Result<()> {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    info!("IPC client connected");

    loop {
        tokio::select! {
            // Forward broadcast events to this client.
            event = rx.recv() => {
                match event {
                    Ok(json) => {
                        writer.write_all(json.as_bytes()).await?;
                        writer.write_all(b"\n").await?;
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("IPC client lagged, dropped {n} events");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            // Read commands from client (reserved; respond with ok).
            line = lines.next_line() => {
                match line? {
                    Some(cmd) => {
                        debug!("IPC command received: {cmd}");
                        writer.write_all(b"{\"ok\":true}\n").await?;
                    }
                    None => break, // client disconnected
                }
            }
        }
    }

    info!("IPC client disconnected");
    Ok(())
}
