//! mDNS service advertisement
//!
//! Advertises `_ozma._udp.local.` so the controller can discover this node
//! without manual configuration.  TXT records mirror the fields expected by
//! `controller/state.py` NodeInfo.

use anyhow::Result;
use mdns_sd::{ServiceDaemon, ServiceInfo};
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use crate::Cli;

pub async fn run(cli: Cli, cancel: CancellationToken) -> Result<()> {
    let mdns = ServiceDaemon::new()?;

    let host_name = format!("{}.local.", cli.name);

    // Build http_port string for TXT records — must outlive the properties slice
    let http_port_str = cli.http_port.to_string();

    let mut props: Vec<(&str, &str)> = vec![
        ("proto", "1"),
        ("role", cli.role.as_str()),
        ("hw", cli.hw.as_str()),
        ("fw", cli.fw.as_str()),
        ("cap", cli.cap.as_str()),
        ("api_port", http_port_str.as_str()),
        ("machine_class", "workstation"),
    ];

    // Advertise stream info when video capability is present
    if cli.cap.split(',').any(|c| c.trim() == "video") {
        props.push(("stream_port", http_port_str.as_str()));
        props.push(("stream_path", "/stream/stream.m3u8"));
    }

    let service = ServiceInfo::new(
        crate::SERVICE_TYPE,
        &cli.name,
        &host_name,
        "",                // IP resolved by mdns-sd from local interfaces
        cli.hid_udp_port,
        props.as_slice(),
    )?;

    mdns.register(service)?;
    info!(
        instance = %cli.name,
        port = cli.hid_udp_port,
        "mDNS service registered (_ozma._udp.local.)"
    );

    // Keep the daemon alive until cancelled
    cancel.cancelled().await;

    info!("mDNS shutting down");
    if let Err(e) = mdns.shutdown() {
        warn!("mDNS shutdown error: {e}");
    }

    Ok(())
}
