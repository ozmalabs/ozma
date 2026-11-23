//! ozma-virtual-node — hypervisor-side supervisor for per-VM `ozma-node`
//! processes.
//!
//! Discovers running VMs on this host (Proxmox, libvirt, or QMP sockets),
//! reports their status, provisions the in-guest agent, and — by default —
//! supervises one real `ozma-node --profile vm` subprocess per discovered VM
//! that has a QMP socket. Each child gets a stable HID UDP port and HTTP port;
//! failed children restart with bounded exponential backoff and all children
//! are stopped on shutdown.
//!
//! This is the subprocess transition, not the full VM provisioning story. QMP
//! and display socket creation, libvirt XML edits, D-Bus p2p display, evdev
//! `input-linux`, PulseAudio routing, and libvirt power control remain separate
//! work. Use `--no-auto-manage` for discovery/agent-only operation.

mod agent;
mod discovery;
mod manager;
mod supervisor;
mod vm_info;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use manager::{ManagerOptions, VirtualNodeManager};
use tracing::info;

/// Ozma Virtual Node — discover VMs and supervise one `ozma-node` per VM.
///
/// Discovery, status, agent provisioning, per-VM port allocation, subprocess
/// start/stop/restart, and shutdown are implemented here. QMP/display socket
/// provisioning, libvirt XML edits, D-Bus p2p display, evdev `input-linux`,
/// PulseAudio routing, and libvirt power remain separate work.
#[derive(Parser, Debug)]
#[command(
    name = "ozma-virtual-node",
    about = "Discover VMs on this hypervisor and supervise one ozma-node --profile vm per VM",
    long_about = "Discover running VMs on this hypervisor, report their status, provision \
                  the in-guest agent, and supervise one `ozma-node --profile vm` subprocess \
                  per discovered VM that has a QMP socket.\n\n\
                  Each child gets a stable HID UDP port and HTTP port. New VMs start, \
                  removed VMs stop, unchanged VMs are left alone, failed children restart \
                  with bounded exponential backoff, and shutdown stops every child.\n\n\
                  QMP/display socket provisioning, libvirt XML edits, D-Bus p2p display, \
                  evdev input-linux, PulseAudio routing, and libvirt power are not part of \
                  this supervisor yet. VMs without a discovered QMP socket are skipped with \
                  a warning; no socket path is fabricated. Use --no-auto-manage for \
                  discovery/agent-only operation."
)]
struct Args {
    /// Controller URL (e.g. http://10.0.0.1:7380)
    ///
    /// Passed to each per-VM node as `--register-url`.
    #[arg(long, default_value = "")]
    controller: String,

    /// Directory to scan for QMP sockets (fallback discovery)
    #[arg(long, default_value = "")]
    qmp_dir: String,

    /// Comma-separated VM name patterns to exclude (e.g. 'template-*,infra-*')
    #[arg(long, default_value = "")]
    exclude: String,

    /// Path to the per-VM node binary
    ///
    /// The target runtime is `ozma-node`; this supervisor never spawns the
    /// `ozma-virtual-node` binary itself.
    #[arg(long, default_value = "ozma-node", value_name = "PATH")]
    node_bin: PathBuf,

    /// First HID UDP port; each VM gets base + its allocation offset
    #[arg(long, default_value_t = 7332)]
    base_port: u16,

    /// First HTTP port; each VM gets node-http-base + its allocation offset
    #[arg(long, default_value_t = 7382)]
    node_http_base: u16,

    /// Initial restart delay after a per-VM node failure, in milliseconds
    #[arg(long, default_value_t = 1000)]
    restart_backoff_ms: u64,

    /// Maximum restart delay after repeated per-VM node failures, in milliseconds
    #[arg(long, default_value_t = 30000)]
    restart_backoff_max_ms: u64,

    /// Discovery-only mode: report VMs and provision agents without starting nodes
    #[arg(long)]
    no_auto_manage: bool,

    /// Don't attempt agent provisioning inside VMs
    #[arg(long)]
    no_auto_agent: bool,

    /// Enable debug logging
    #[arg(long)]
    debug: bool,
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

    let exclude: Vec<String> = if args.exclude.is_empty() {
        Vec::new()
    } else {
        args.exclude
            .split(',')
            .map(|s| s.trim().to_owned())
            .filter(|s| !s.is_empty())
            .collect()
    };

    let options = ManagerOptions {
        controller_url: args.controller,
        qmp_dir: args.qmp_dir,
        auto_manage: !args.no_auto_manage,
        auto_agent: !args.no_auto_agent,
        exclude_patterns: exclude,
        node_bin: args.node_bin,
        base_port: args.base_port,
        node_http_base: args.node_http_base,
        restart_backoff: Duration::from_millis(args.restart_backoff_ms),
        restart_backoff_max: Duration::from_millis(args.restart_backoff_max_ms),
        ..ManagerOptions::default()
    };

    let mgr = Arc::new(VirtualNodeManager::new(options));

    // Reject bad supervisor settings before installing signal handlers or
    // spawning any work.
    mgr.validate()?;

    // Graceful shutdown on SIGINT / SIGTERM. `run` stops every child and waits
    // before returning.
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

    mgr.run().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cli_supervisor_defaults() {
        let args = Args::try_parse_from(["ozma-virtual-node"]).expect("parse defaults");

        assert_eq!(args.node_bin, PathBuf::from("ozma-node"));
        assert_eq!(args.base_port, 7332);
        assert_eq!(args.node_http_base, 7382);
        assert_eq!(args.restart_backoff_ms, 1000);
        assert_eq!(args.restart_backoff_max_ms, 30000);
        assert!(!args.no_auto_manage);
        assert!(!args.no_auto_agent);
    }

    #[test]
    fn no_auto_manage_flag_parses_without_affecting_validation() {
        let args =
            Args::try_parse_from(["ozma-virtual-node", "--no-auto-manage"]).expect("parse flag");
        let options = ManagerOptions {
            controller_url: args.controller,
            qmp_dir: args.qmp_dir,
            auto_manage: !args.no_auto_manage,
            auto_agent: !args.no_auto_agent,
            exclude_patterns: Vec::new(),
            node_bin: args.node_bin,
            base_port: args.base_port,
            node_http_base: args.node_http_base,
            restart_backoff: Duration::from_millis(args.restart_backoff_ms),
            restart_backoff_max: Duration::from_millis(args.restart_backoff_max_ms),
            ..ManagerOptions::default()
        };

        assert!(!options.auto_manage);
        assert!(VirtualNodeManager::new(options).validate().is_ok());
    }
}
