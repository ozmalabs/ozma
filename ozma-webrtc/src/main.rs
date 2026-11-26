// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! `ozma-webrtc-native` — a native Rust WebRTC helper for `ozma-node`.
//!
//! It speaks the node's existing GStreamer-helper contract:
//!
//! * argv: `--sock --shm --width --height --fps [--audio-*] [--stun-server]
//!   [--turn-server --turn-user --turn-password]`;
//! * packed BGRA frames in the shared-memory file (whole-file replacement by
//!   the node);
//! * newline-delimited JSON on the Unix socket (`offer`, `add-ice-candidate`,
//!   `bitrate`).
//!
//! The difference is the media core: webrtc-rs handles ICE/DTLS/SRTP/RTP and
//! OpenH264 encodes the frames as a real H.264 track. This is a prototype, not
//! the product default; see `docs/phase4-native-webrtc-report.md`.

mod audio;
mod cli;
mod encoder;
mod sdp;
mod server;
mod shm;
mod signaling;

use std::sync::Arc;

use clap::Parser;
use tokio::sync::Notify;
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::audio::AudioConfig;
use crate::cli::Args;
use crate::server::{ice_servers, NativeHelper, ServerConfig, VideoConfig};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .with_target(false)
        .init();

    let args = Args::parse();
    if let Err(message) = args.validate() {
        anyhow::bail!("invalid configuration: {message}");
    }

    let servers = ice_servers(
        args.stun_server.as_deref(),
        args.turn_server.as_deref(),
        args.turn_user.as_deref(),
        args.turn_password.as_deref(),
    )
    .map_err(|message| anyhow::anyhow!(message))?;
    if servers.is_empty() {
        info!("ICE: host candidates only (no STUN/TURN configured)");
    } else {
        info!(
            servers = servers.len(),
            "ICE: STUN/TURN configured; gathering host and server-reflexive/relay candidates"
        );
    }

    let config = ServerConfig {
        socket_path: args.sock.clone(),
        video: VideoConfig {
            shm_path: args.shm.clone(),
            width: args.width,
            height: args.height,
            fps: args.fps,
            bitrate_kbps: encoder::clamp_bitrate_bps(u64::from(args.bitrate) * 1_000) / 1_000,
        },
        audio: args.audio_source.as_ref().map(|source| AudioConfig {
            source: source.clone(),
            backend: args.audio_backend,
        }),
        ice_servers: servers,
        gather_timeout: std::time::Duration::from_secs(args.gather_timeout_seconds),
        max_sessions: args.max_sessions,
    };

    let helper = NativeHelper::new(config)
        .await
        .map_err(|message| anyhow::anyhow!("could not start the native helper: {message}"))?;

    let shutdown = Arc::new(Notify::new());
    let signal_shutdown = shutdown.clone();
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut term = match signal(SignalKind::terminate()) {
                Ok(signal) => signal,
                Err(error) => {
                    warn!(error = %error, "could not install the SIGTERM handler");
                    return;
                }
            };
            let mut int = match signal(SignalKind::interrupt()) {
                Ok(signal) => signal,
                Err(error) => {
                    warn!(error = %error, "could not install the SIGINT handler");
                    return;
                }
            };
            tokio::select! {
                _ = term.recv() => info!("received SIGTERM"),
                _ = int.recv() => info!("received SIGINT"),
            }
            signal_shutdown.notify_waiters();
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
            signal_shutdown.notify_waiters();
        }
    });

    helper
        .run(shutdown)
        .await
        .map_err(|message| anyhow::anyhow!(message))?;
    info!("native WebRTC helper stopped");
    Ok(())
}
