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
    // ServiceDaemon spawns its own internal thread; we just hold the handle.
    let mdns = ServiceDaemon::new()?;

    let host_name    = format!("{}.local.", cli.name);
    let http_port_str = cli.http_port.to_string();

    // Build TXT record property list — all values must outlive `service`.
    let stream_port_str = http_port_str.clone();
    let mut props: Vec<(&str, &str)> = vec![
        ("proto",         "1"),
        ("role",          cli.role.as_str()),
        ("hw",            cli.hw.as_str()),
        ("fw",            cli.fw.as_str()),
        ("cap",           cli.cap.as_str()),
        ("api_port",      http_port_str.as_str()),
        ("machine_class", "workstation"),
    ];

    let has_video = cli.cap.split(',').any(|c| c.trim() == "video");
    if has_video {
        props.push(("stream_port", stream_port_str.as_str()));
        props.push(("stream_path", "/stream/stream.m3u8"));
    }

    let service = ServiceInfo::new(
        crate::SERVICE_TYPE,
        &cli.name,
        &host_name,
        "",               // IP resolved by mdns-sd from local interfaces
        cli.hid_udp_port,
        props.as_slice(),
    )?;

    mdns.register(service)?;
    info!(
        instance = %cli.name,
        port     = cli.hid_udp_port,
        "mDNS service registered (_ozma._udp.local.)"
    );

    // Block until the CancellationToken fires (Ctrl-C / SIGTERM).
    cancel.cancelled().await;

    info!("mDNS shutting down");
    if let Err(e) = mdns.shutdown() {
        warn!("mDNS shutdown error: {e}");
    }

    Ok(())
}
