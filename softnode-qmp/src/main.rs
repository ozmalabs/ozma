// SPDX-License-Identifier: AGPL-3.0-only
//! CLI entry point — minimal smoke-test / manual exercise tool.
//!
//! Usage:
//!   softnode-qmp --ctrl /tmp/vm-ctrl.qmp --input /tmp/vm-input.qmp
//!
//! Connects to QEMU, types "hello" and queries VM status.

use std::time::Duration;

use clap::Parser;
use tracing::info;
use tracing_subscriber::EnvFilter;

use softnode_qmp::hid_to_qmp::KeyboardReportState;
use softnode_qmp::qmp_client::QmpClient;

#[derive(Parser)]
#[command(name = "softnode-qmp", about = "QMP client smoke-test")]
struct Args {
    /// Path to QMP control socket
    #[arg(long, default_value = "/tmp/vm-ctrl.qmp")]
    ctrl: String,

    /// Path to QMP input socket (omit for single-socket legacy mode)
    #[arg(long, default_value = "")]
    input: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env().add_directive("softnode_qmp=debug".parse().unwrap()))
        .init();

    let args = Args::parse();

    let client = if args.input.is_empty() {
        QmpClient::new_single(&args.ctrl)
    } else {
        QmpClient::new_dual(&args.ctrl, &args.input)
    };

    client.start().await;
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Type "hello" via HID boot reports
    let mut kbd = KeyboardReportState::new();

    // Key sequence for "hello" (lowercase, no modifiers)
    // h=0x0B, e=0x08, l=0x0F, l=0x0F, o=0x12
    let keys: &[u8] = &[0x0B, 0x08, 0x0F, 0x0F, 0x12];
    for &hid in keys {
        // Press
        let mut report = [0u8; 8];
        report[2] = hid;
        let events = kbd.diff(&report);
        client.send_input_events(&events).await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        // Release
        let report = [0u8; 8];
        let events = kbd.diff(&report);
        client.send_input_events(&events).await;
        tokio::time::sleep(Duration::from_millis(50)).await;
    }

    // Query VM status
    if let Some(status) = client.query_status().await {
        info!("VM status: {}", status);
    }

    client.stop().await;
}
