//! ozma-node — single binary node agent
//!
//! Spawns independent tokio tasks for:
//!   - UDP HID receiver  (port 7331)
//!   - HLS / REST server (port 7382)
//!   - mDNS advertisement
//!   - Controller registration + heartbeat
//!   - Display capture pipeline (optional, requires ffmpeg + V4L2 device)
//!
//! Cross-compile for aarch64 (Pi4, Milk-V Duo S):
//!   cross build -p ozma-node --release --target aarch64-unknown-linux-gnu

mod tasks;

use anyhow::Result;
use clap::Parser;
use tokio_util::sync::CancellationToken;
use tracing::{error, info};
use tracing_subscriber::{fmt, EnvFilter, prelude::*};

use tasks::{capture, heartbeat, hls, mdns, udp};

/// Ozma KVM node agent
#[derive(Parser, Debug, Clone)]
#[command(author, version, about, long_about = None)]
pub struct Cli {
    /// Node name / mDNS instance name (default: hostname)
    #[arg(long, env = "OZMA_NODE_NAME", default_value_t = default_node_id())]
    pub name: String,

    /// UDP port for HID event reception
    #[arg(long, env = "OZMA_UDP_PORT", default_value_t = 7331)]
    pub hid_udp_port: u16,

    /// TCP port for HLS / REST API (matches node.py default)
    #[arg(long, env = "OZMA_HTTP_PORT", default_value_t = 7382)]
    pub http_port: u16,

    /// Comma-separated capability list advertised in mDNS TXT and registration
    /// Example: `hid,video,audio`
    #[arg(long, env = "OZMA_CAP", default_value = "hid")]
    pub cap: String,

    /// Hardware identifier string (e.g. rpi4, milkv-duos)
    #[arg(long, env = "OZMA_HW", default_value_t = default_hw())]
    pub hw: String,

    /// Firmware / software version string
    #[arg(long, env = "OZMA_FW", default_value = env!("CARGO_PKG_VERSION"))]
    pub fw: String,

    /// POST registration directly to this controller URL instead of relying on
    /// mDNS multicast.  Required in QEMU/SLIRP environments.
    /// Example: `http://10.0.2.2:7380`
    #[arg(long, env = "OZMA_REGISTER_URL")]
    pub register_url: Option<String>,

    /// Override the host IP reported to the controller.
    /// Useful with SLIRP port-forwarding where the controller sees `localhost`
    /// but the node's real LAN IP is different.
    #[arg(long, env = "OZMA_REGISTER_HOST")]
    pub register_host: Option<String>,

    /// Heartbeat interval in seconds
    #[arg(long, env = "OZMA_HEARTBEAT_SECS", default_value_t = 30)]
    pub heartbeat_secs: u64,

    /// V4L2 capture device path (empty = disabled)
    #[arg(long, env = "OZMA_CAPTURE_DEVICE", default_value = "")]
    pub capture_device: String,

    /// Node role advertised via mDNS TXT record
    #[arg(long, env = "OZMA_ROLE", default_value = "compute")]
    pub role: String,

    /// Enable verbose debug logging
    #[arg(long, env = "OZMA_DEBUG")]
    pub debug: bool,
}

impl Cli {
    /// The mDNS instance name, matching node.py: `<name>._ozma._udp.local.`
    pub fn node_id(&self) -> String {
        format!("{}.{}.", self.name, SERVICE_TYPE.trim_end_matches('.'))
    }
}

pub const SERVICE_TYPE: &str = "_ozma._udp.local.";

fn default_node_id() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string())
}

fn default_hw() -> String {
    // Best-effort: read /proc/device-tree/model (Raspberry Pi, Milk-V etc.)
    if let Ok(model) = std::fs::read_to_string("/proc/device-tree/model") {
        let model = model.trim_end_matches('\0').trim();
        if !model.is_empty() {
            return model
                .chars()
                .take(32)
                .collect::<String>()
                .to_lowercase()
                .replace(' ', "-");
        }
    }
    std::env::consts::ARCH.to_string()
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let log_level = if cli.debug { "debug" } else { "info" };
    tracing_subscriber::registry()
        .with(fmt::layer())
        .with(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new(log_level)),
        )
        .init();

    // Notify systemd that initialisation is complete (no-op on non-Linux).
    #[cfg(target_os = "linux")]
    {
        let _ = sd_notify::notify(true, &[sd_notify::NotifyState::Ready]);
    }

    info!(
        name      = %cli.name,
        node_id   = %cli.node_id(),
        udp_port  = cli.hid_udp_port,
        http_port = cli.http_port,
        cap       = %cli.cap,
        hw        = %cli.hw,
        fw        = %cli.fw,
        "ozma-node starting"
    );

    // Shared cancellation token — cancelled on Ctrl-C / SIGTERM
    let token = CancellationToken::new();
    let mut handles = Vec::new();

    // ── UDP HID receiver ──────────────────────────────────────────────────
    {
        let (port, tok) = (cli.hid_udp_port, token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = udp::run(port, tok).await {
                error!("UDP task exited: {e:#}");
            }
        }));
    }

    // ── HLS / REST server ─────────────────────────────────────────────────
    {
        let (port, tok) = (cli.http_port, token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = hls::run(port, tok).await {
                error!("HLS task exited: {e:#}");
            }
        }));
    }

    // ── mDNS advertisement ────────────────────────────────────────────────
    {
        let (cfg, tok) = (cli.clone(), token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = mdns::run(cfg, tok).await {
                error!("mDNS task exited: {e:#}");
            }
        }));
    }

    // ── Controller registration + heartbeat ───────────────────────────────
    {
        let (cfg, tok) = (cli.clone(), token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = heartbeat::run(cfg, tok).await {
                error!("Heartbeat task exited: {e:#}");
            }
        }));
    }

    // ── Display capture pipeline (optional) ───────────────────────────────
    if !cli.capture_device.is_empty() || cli.cap.split(',').any(|c| c.trim() == "video") {
        let dev = if cli.capture_device.is_empty() {
            "/dev/video0".to_string()
        } else {
            cli.capture_device.clone()
        };
        let (dev, tok) = (dev, token.clone());
        handles.push(tokio::spawn(async move {
            if let Err(e) = capture::run(dev, tok).await {
                error!("Capture task exited: {e:#}");
            }
        }));
    }

    // ── Shutdown ──────────────────────────────────────────────────────────
    wait_for_shutdown().await;
    info!("Shutdown signal received — stopping all tasks");
    token.cancel();

    for h in handles {
        let _ = h.await;
    }

    info!("ozma-node stopped");
    Ok(())
}

async fn wait_for_shutdown() {
    use tokio::signal;
    #[cfg(unix)]
    {
        let mut sigterm =
            signal::unix::signal(signal::unix::SignalKind::terminate()).expect("SIGTERM handler");
        tokio::select! {
            _ = signal::ctrl_c() => {},
            _ = sigterm.recv() => {},
        }
    }
    #[cfg(not(unix))]
    {
        signal::ctrl_c().await.expect("Ctrl-C handler");
    }
}
