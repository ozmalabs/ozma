//! ozma-virtual-node — CLI entry point.
//!
//! Parses arguments and drives `VirtualNodeManager`.

mod agent;
mod discovery;
mod manager;
mod vm_info;
mod softnode;

use clap::Parser;
use manager::VirtualNodeManager;
use softnode::SoftNode;
use std::sync::Arc;
use tracing::info;

/// Ozma Virtual Node — auto-manage all VMs on a hypervisor.
///
/// Every running VM automatically gets a soft node (HID + capture + power)
/// and an agent provisioning attempt.
#[derive(Parser, Debug)]
#[command(
    name = "ozma-virtual-node",
    about = "Auto-manage all QEMU/KVM/Proxmox VMs as ozma nodes",
    long_about = None,
)]
struct Args {
    /// Controller URL (e.g. http://10.0.0.1:7380)
    #[arg(long, default_value = "")]
    controller: String,

    /// Directory to scan for QMP sockets (fallback discovery)
    #[arg(long, default_value = "")]
    qmp_dir: String,

    /// Starting UDP port for soft nodes
    #[arg(long, default_value_t = 7332)]
    base_port: u16,

    /// Audio sink name prefix
    #[arg(long, default_value = "ozma-")]
    audio_prefix: String,

    /// Comma-separated VM name patterns to exclude (e.g. 'template-*,infra-*')
    #[arg(long, default_value = "")]
    exclude: String,

    /// Don't auto-create nodes for discovered VMs
    #[arg(long)]
    no_auto_manage: bool,

    /// Don't attempt agent provisioning inside VMs
    #[arg(long)]
    no_auto_agent: bool,

    /// Enable debug logging
    #[arg(long)]
    debug: bool,

    /// Run as a single softnode instead of virtual node manager
    #[arg(long, conflicts_with = "controller")]
    softnode: bool,

    /// Softnode name (required when using --softnode)
    #[arg(long, requires = "softnode")]
    name: Option<String>,

    /// Softnode UDP port (when using --softnode)
    #[arg(long, requires = "softnode", default_value_t = 7332)]
    port: u16,

    /// Softnode host bind address
    #[arg(long, requires = "softnode", default_value = "0.0.0.0")]
    host: String,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    let level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .with_target(false)
        .init();

    if args.softnode {
        // Run as a single softnode
        let name = args.name.unwrap_or_else(|| "unnamed".to_string());
        let node = SoftNode::new(name, args.host, args.port);
        node.run().await?;
        return Ok(());
    }

    // Run as virtual node manager (existing behavior)
    let exclude: Vec<String> = if args.exclude.is_empty() {
        Vec::new()
    } else {
        args.exclude
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let mgr = Arc::new(VirtualNodeManager::new(
        args.controller,
        args.qmp_dir,
        args.base_port,
        args.audio_prefix,
        !args.no_auto_manage,
        !args.no_auto_agent,
        exclude,
    ));

    // Graceful shutdown on SIGINT / SIGTERM
    let mgr_signal = Arc::clone(&mgr);
    tokio::spawn(async move {
        #[cfg(unix)]
        {
            use tokio::signal::unix::{signal, SignalKind};
            let mut sigint = signal(SignalKind::interrupt()).expect("SIGINT handler");
            let mut sigterm = signal(SignalKind::terminate()).expect("SIGTERM handler");
            tokio::select! {
                _ = sigint.recv() => {}
                _ = sigterm.recv() => {}
            }
        }
        #[cfg(not(unix))]
        {
            tokio::signal::ctrl_c().await.ok();
        }
        info!("Shutdown signal received");
        mgr_signal.stop().await;
    });

    mgr.run().await;
    Ok(())
}
