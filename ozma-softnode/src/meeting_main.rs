//! Entry point for the `ozma-meeting-detect` binary.
//!
//! Exposes a Unix-domain socket IPC server. Connected clients receive
//! newline-delimited JSON [`ServerMessage`] frames and may send
//! [`ClientMessage`] frames to query state or force a re-scan.
//!
//! # Socket path
//! `$XDG_RUNTIME_DIR/ozma-meeting-detect.sock`
//! (fallback: `/tmp/ozma-meeting-detect.sock`)
//!
//! # Usage
//! ```text
//! ozma-meeting-detect [--interval <secs>] [--ics-path <path>] ...
//! ```

mod ipc;
mod meeting_detect;

use std::{path::PathBuf, sync::Arc, time::Duration};

use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixListener, UnixStream},
    sync::broadcast,
};
use tracing::{error, info, warn};
use tracing_subscriber::EnvFilter;

use ipc::{ClientMessage, ServerMessage};
use meeting_detect::{DetectorConfig, MeetingDetector};

// ── CLI args ─────────────────────────────────────────────────────────────────

struct Args {
    poll_interval_secs: u64,
    extra_ics_paths:    Vec<PathBuf>,
    socket_path:        PathBuf,
}

fn parse_args() -> Args {
    let mut iter = std::env::args().skip(1).peekable();
    let mut interval: u64 = 5;
    let mut ics_paths: Vec<PathBuf> = Vec::new();

    while let Some(arg) = iter.next() {
        match arg.as_str() {
            "--interval" | "-i" => {
                if let Some(v) = iter.next() {
                    interval = v.parse().unwrap_or(5);
                }
            }
            "--ics-path" | "-c" => {
                if let Some(v) = iter.next() {
                    ics_paths.push(PathBuf::from(v));
                }
            }
            _ => {}
        }
    }

    let socket_path = std::env::var_os("XDG_RUNTIME_DIR")
        .map(|d| PathBuf::from(d).join("ozma-meeting-detect.sock"))
        .unwrap_or_else(|| PathBuf::from("/tmp/ozma-meeting-detect.sock"));

    Args { poll_interval_secs: interval, extra_ics_paths: ics_paths, socket_path }
}

// ── IPC client handler ───────────────────────────────────────────────────────

async fn handle_client(
    stream:     UnixStream,
    mut rx:     broadcast::Receiver<ServerMessage>,
    force_scan: Arc<tokio::sync::Notify>,
) {
    let (reader, mut writer) = stream.into_split();
    let mut lines = BufReader::new(reader).lines();

    // Trigger an immediate snapshot for the newly connected client.
    force_scan.notify_one();

    loop {
        tokio::select! {
            // Outbound: forward broadcast messages to the client.
            msg = rx.recv() => {
                match msg {
                    Ok(m) => {
                        let mut json = match serde_json::to_string(&m) {
                            Ok(j)  => j,
                            Err(e) => { warn!("Serialise error: {e}"); continue; }
                        };
                        json.push('\n');
                        if writer.write_all(json.as_bytes()).await.is_err() {
                            break; // client disconnected
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        warn!("IPC client lagged by {n} messages");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }

            // Inbound: handle client commands.
            line = lines.next_line() => {
                match line {
                    Ok(Some(l)) => match serde_json::from_str::<ClientMessage>(&l) {
                        Ok(ClientMessage::GetSnapshot | ClientMessage::ForceScan) => {
                            force_scan.notify_one();
                        }
                        Err(e) => warn!("Bad client message: {e}"),
                    },
                    _ => break, // EOF or read error
                }
            }
        }
    }
}

// ── Main ─────────────────────────────────────────────────────────────────────

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::from_default_env()
                .add_directive("ozma_softnode=info".parse().unwrap()),
        )
        .init();

    let args = parse_args();

    // Remove a stale socket from a previous run.
    let _ = std::fs::remove_file(&args.socket_path);

    let listener = match UnixListener::bind(&args.socket_path) {
        Ok(l)  => l,
        Err(e) => {
            error!("Cannot bind socket {:?}: {e}", args.socket_path);
            std::process::exit(1);
        }
    };
    info!("IPC socket: {:?}", args.socket_path);

    // Broadcast channel — capacity 64 keeps memory bounded.
    let (tx, _) = broadcast::channel::<ServerMessage>(64);
    let force_scan = Arc::new(tokio::sync::Notify::new());

    // Spawn the detector polling loop.
    let config = DetectorConfig {
        poll_interval:   Duration::from_secs(args.poll_interval_secs),
        extra_ics_paths: args.extra_ics_paths,
    };
    let _detector = MeetingDetector::new(config, tx.clone()).spawn();

    // Graceful shutdown on SIGINT / SIGTERM.
    {
        let tx_shutdown = tx.clone();
        tokio::spawn(async move {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigint  = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv()  => {}
                _ = sigterm.recv() => {}
            }
            info!("Shutdown signal received");
            drop(tx_shutdown); // closes the broadcast channel → all clients disconnect
            std::process::exit(0);
        });
    }

    // Accept IPC connections.
    loop {
        match listener.accept().await {
            Ok((stream, _addr)) => {
                info!("IPC client connected");
                tokio::spawn(handle_client(
                    stream,
                    tx.subscribe(),
                    Arc::clone(&force_scan),
                ));
            }
            Err(e) => error!("Accept error: {e}"),
        }
    }
}
