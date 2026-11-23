// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Ozma Node — discovery, registration, session API, and UAC2 bridge.
//!
//! The node is a device, not a desktop app:
//! 1. resolve the local LAN address (or use `--register-host`);
//! 2. advertise `_ozma._udp.local.` over mDNS;
//! 3. optionally register directly with a controller (SLIRP/QEMU, or a
//!    controller that does not browse mDNS);
//! 4. serve the versioned node API;
//! 5. run the UAC2 audio bridge when the `audio` capability is present.

use clap::Parser;
use ozma_protocol::NodeId;
use tracing::{error, info};

use ozma_node::{api, config::NodeConfig, mdns, platform, registration, usb_audio};

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

    /// HTTP port for the node API, HLS fallback, and snapshots.
    #[arg(long, default_value_t = 7382)]
    pub http_port: u16,

    /// RTSP listener port (0 disables the compatibility listener).
    #[arg(long, env = "OZMA_RTSP_PORT", default_value_t = 0)]
    pub rtsp_port: u16,

    /// Comma-separated capability list advertised in mDNS TXT and registration.
    /// Example: `hid,video,audio`
    #[arg(long, default_value = "hid,video,audio")]
    pub cap: String,

    /// Node role advertised to the controller.
    #[arg(long, default_value = "compute")]
    pub role: String,

    /// Machine class: `workstation`, `server`, or `kiosk`.
    #[arg(long, default_value = "workstation")]
    pub machine_class: String,

    /// Require local consent for workstation-class sessions.
    #[arg(long)]
    pub workstation_consent: bool,

    /// Hardware platform string (e.g. `rpi4`, `x86_64`).
    #[arg(long, default_value_t = default_hw())]
    pub hw: String,

    /// Firmware / software version string.
    #[arg(long, default_value = "0.1.0")]
    pub fw: String,

    /// POST registration directly to this controller URL instead of relying
    /// on mDNS multicast. Required in QEMU/SLIRP environments.
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

    /// Replay a trusted local browser scene fixture in scene-capable sessions.
    #[arg(long)]
    pub scene_replay: Option<std::path::PathBuf>,

    /// Require this bearer token for the node's HTTP API.
    #[arg(long, env = "OZMA_API_TOKEN")]
    pub api_token: Option<String>,

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

    let level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .init();

    let node_id = NodeConfig::node_id_from_name(&args.name);
    let config = NodeConfig {
        name: args.name.clone(),
        node_id: node_id.clone(),
        hid_udp_port: args.hid_udp_port,
        http_port: args.http_port,
        rtsp_port: args.rtsp_port,
        capabilities: args
            .cap
            .split(',')
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect(),
        hw: args.hw.clone(),
        fw: args.fw.clone(),
        register_url: args.register_url.clone(),
        register_host: args.register_host.clone(),
        heartbeat_interval_secs: args.heartbeat_interval,
        role: args.role.clone(),
        machine_class: ozma_auth::MachineClass::parse(&args.machine_class)
            .unwrap_or(ozma_auth::MachineClass::Workstation),
        workstation_consent: args.workstation_consent,
        api_token: args.api_token.clone(),
    };

    let capabilities = platform::detect(&config);

    info!(
        name = %config.name,
        node_id = %node_id,
        hid_udp_port = config.hid_udp_port,
        http_port = config.http_port,
        cap = %args.cap,
        hw = %config.hw,
        platform = %capabilities.platform,
        "Ozma node starting"
    );

    let local_ip = resolve_local_ip(&args);
    info!(ip = %local_ip, "Resolved local IP");

    let txt = build_txt(&args, &local_ip);

    let _mdns_handle = match mdns::advertise(&config.name, config.hid_udp_port, &txt) {
        Ok(handle) => {
            info!("mDNS advertising {}.{}", config.name, mdns::SERVICE_TYPE);
            mdns::Handle::Real(handle)
        }
        Err(error) => {
            error!("mDNS advertisement failed: {error}");
            mdns::Handle::Dummy
        }
    };

    if let Some(ref url) = config.register_url {
        match registration::register(url, node_id.as_str(), &local_ip, config.hid_udp_port, &txt)
            .await
        {
            Ok(()) => info!("Direct registration succeeded → {url}"),
            Err(error) => error!("Direct registration failed: {error}"),
        }

        let url = url.clone();
        let node_id = node_id.clone();
        let interval = config.heartbeat_interval_secs;
        tokio::spawn(async move {
            registration::heartbeat_loop(&url, node_id.as_str(), interval).await;
        });
    }

    if config.has_capability("audio") {
        tokio::spawn(async move {
            match usb_audio::UsbAudioGadget::open(true).await {
                Ok(gadget) => {
                    if let Err(error) = gadget.run_bridge().await {
                        tracing::error!("UAC2 bridge exited with error: {error:#}");
                    }
                }
                Err(error) => tracing::error!("UAC2 gadget init failed: {error:#}"),
            }
        });
    }

    let http_port = config.http_port;
    let quic_port = http_port.saturating_add(2);
    let scene_replay = match args
        .scene_replay
        .as_ref()
        .map(|path| ozma_node::scene_replay::JsonlSceneProducer::from_path(path, 4 * 1024 * 1024))
        .transpose()
    {
        Ok(replay) => replay,
        Err(error) => {
            error!("Scene replay could not be loaded: {error}");
            std::process::exit(1);
        }
    };
    let api_capabilities = capabilities.clone();
    let consent_policy = config.consent_policy();
    let api_token = config.api_token.clone();
    tokio::spawn(async move {
        if let Err(error) = api::serve_with_scene_replay(
            http_port,
            quic_port,
            api_capabilities,
            consent_policy,
            scene_replay,
            api_token,
            config.rtsp_port,
        )
        .await
        {
            error!("Node API exited: {error:#}");
        }
    });

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
    txt.insert("role".into(), args.role.clone());
    txt.insert("hw".into(), args.hw.clone());
    txt.insert("fw".into(), args.fw.clone());
    txt.insert("cap".into(), args.cap.clone());
    txt.insert("api_port".into(), args.http_port.to_string());
    txt.insert("quic_port".into(), (args.http_port + 2).to_string());
    if args.rtsp_port != 0 {
        txt.insert("rtsp_port".into(), args.rtsp_port.to_string());
    }
    if args.cap.split(',').any(|value| value.trim() == "video") {
        txt.insert("stream_port".into(), args.http_port.to_string());
        txt.insert("stream_path".into(), "/stream/stream.m3u8".into());
    }
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

/// Keep `NodeId` imported in the binary's public surface for integrations that
/// build a node id from the same convention.
pub fn node_id_for(name: &str) -> NodeId {
    NodeConfig::node_id_from_name(name)
}
