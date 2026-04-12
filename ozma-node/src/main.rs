//! Ozma Node — mDNS advertisement and controller registration.
//!
//! Ports the mDNS + direct-registration logic from `node/node.py` to Rust.
//!
//! # What this binary does
//!
//! 1. Resolves the local LAN IP (or uses `--register-host` override).
//! 2. Advertises `<name>._ozma._udp.local.` via mDNS (libmdns).
//! 3. If `--register-url` is set, POSTs to `<url>/api/v1/nodes/register`
//!    (SLIRP / QEMU workaround — mDNS multicast can't cross the SLIRP boundary).
//! 4. Runs a heartbeat loop that POSTs to `<url>/api/v1/nodes/heartbeat`
//!    every 30 seconds so the controller keeps the node marked online.

mod mdns;
mod registration;
mod usb_audio;

use clap::Parser;
use tracing::{error, info};

/// Ozma hardware KVM node process.
#[derive(Parser, Debug)]
#[command(author, version, about)]
pub struct Args {
    /// Node name (default: system hostname).
    #[arg(long, default_value_t = default_hostname())]
    pub name: String,

    /// UDP port for HID packets.
    #[arg(long, default_value_t = 7331)]
    pub hid_udp_port: u16,

    /// HTTP port for the node's local API / HLS stream.
    #[arg(long, default_value_t = 7382)]
    pub http_port: u16,

    /// Comma-separated capability list advertised in mDNS TXT and registration.
    /// Example: `hid,video,audio`
    #[arg(long, default_value = "hid")]
    pub cap: String,

    /// Hardware platform string (e.g. `rpi4`, `x86_64`).
    #[arg(long, default_value_t = default_hw())]
    pub hw: String,

    /// Firmware / software version string.
    #[arg(long, default_value = "1.0.0")]
    pub fw: String,

    /// POST registration directly to this controller URL instead of relying on
    /// mDNS multicast.  Required in QEMU/SLIRP environments.
    /// Example: `http://10.0.2.2:7380`
    #[arg(long)]
    pub register_url: Option<String>,

    /// Override the host IP reported to the controller.
    /// Useful with SLIRP port-forwarding where the controller sees `localhost`
    /// but the node's real LAN IP is different.
    /// Example: `localhost`
    #[arg(long)]
    pub register_host: Option<String>,

    /// Heartbeat interval in seconds (default 30).
    #[arg(long, default_value_t = 30)]
    pub heartbeat_interval: u64,

    /// Enable verbose debug logging.
    #[arg(long)]
    pub debug: bool,
}

fn default_hostname() -> String {
    hostname::get()
        .ok()
        .and_then(|h| h.into_string().ok())
        .unwrap_or_else(|| "ozma-node".to_string())
}

fn default_hw() -> String {
    // Best-effort: read /proc/device-tree/model (Raspberry Pi etc.)
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
async fn main() {
    let args = Args::parse();

    // Initialise logging.
    let level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .init();

    info!(
        name = %args.name,
        hid_udp_port = args.hid_udp_port,
        http_port = args.http_port,
        cap = %args.cap,
        hw = %args.hw,
        "Ozma node starting"
    );

    // Resolve local IP (or use override).
    let local_ip = resolve_local_ip(&args);
    info!(ip = %local_ip, "Resolved local IP");

    // Build the TXT record map (mirrors node.py's `txt` dict).
    let txt = build_txt(&args, &local_ip);

    // Start mDNS advertisement.
    let _mdns_handle = match mdns::advertise(&args.name, args.hid_udp_port, &txt) {
        Ok(h) => {
            info!("mDNS advertising {}.{}", args.name, mdns::SERVICE_TYPE);
            mdns::Handle::Real(h)
        }
        Err(e) => {
            error!("mDNS advertisement failed: {e}");
            // Non-fatal — direct registration may still work.
            mdns::Handle::Dummy
        }
    };

    // Direct registration (SLIRP / QEMU workaround).
    if let Some(ref url) = args.register_url {
        // node_id mirrors the mDNS instance name used by node.py:
        //   "<name>._ozma._udp.local."
        let node_id = format!("{}.{}.", args.name, mdns::SERVICE_TYPE.trim_end_matches('.'));
        match registration::register(url, &node_id, &local_ip, args.hid_udp_port, &txt).await {
            Ok(()) => info!("Direct registration succeeded → {url}"),
            Err(e) => error!("Direct registration failed: {e}"),
        }

        // Heartbeat loop — keeps the node marked online in the controller.
        let url_clone = url.clone();
        let node_id_clone = node_id.clone();
        let interval = args.heartbeat_interval;
        tokio::spawn(async move {
            registration::heartbeat_loop(&url_clone, &node_id_clone, interval).await;
        });
    }

    // Start UAC2 audio gadget bridge if the "audio" capability is requested.
    if args.cap.split(',').any(|c| c.trim() == "audio") {
        tokio::spawn(async {
            match usb_audio::UsbAudioGadget::open(true).await {
                Ok(gadget) => {
                    if let Err(e) = gadget.run_bridge().await {
                        tracing::error!("UAC2 bridge exited with error: {:#}", e);
                    }
                }
                Err(e) => tracing::error!("UAC2 gadget init failed: {:#}", e),
            }
        });
    }

    // Block until SIGINT / SIGTERM.
    wait_for_shutdown().await;
    info!("Ozma node shutting down");
}

/// Resolve the IP address to advertise / register.
///
/// Priority:
///   1. `--register-host` CLI override (used for SLIRP: "localhost")
///   2. Best local LAN IP via `local-ip-address`
///   3. Fallback: "127.0.0.1"
fn resolve_local_ip(args: &Args) -> String {
    if let Some(ref host) = args.register_host {
        return host.clone();
    }
    local_ip_address::local_ip()
        .map(|ip| ip.to_string())
        .unwrap_or_else(|_| "127.0.0.1".to_string())
}

/// Build the TXT record map that mirrors `node.py`'s `txt` dict.
fn build_txt(args: &Args, local_ip: &str) -> std::collections::HashMap<String, String> {
    let mut txt = std::collections::HashMap::new();
    txt.insert("proto".into(), "1".into());
    txt.insert("role".into(), "compute".into());
    txt.insert("hw".into(), args.hw.clone());
    txt.insert("fw".into(), args.fw.clone());
    txt.insert("cap".into(), args.cap.clone());
    txt.insert("api_port".into(), args.http_port.to_string());
    // Advertise stream info if video capability is present.
    if args.cap.split(',').any(|c| c.trim() == "video") {
        txt.insert("stream_port".into(), args.http_port.to_string());
        txt.insert("stream_path".into(), "/stream/stream.m3u8".into());
    }
    // Include the resolved IP so the controller can use it directly.
    txt.insert("host".into(), local_ip.to_string());
    txt
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
