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
//! This is the subprocess transition plus opt-in libvirt provisioning. With
//! `--provision`, a libvirt domain without a QMP socket gets an injected
//! qemu command line for QMP/display sockets; `--provision-evdev` may create
//! uinput devices and `--provision-audio` may create per-VM PipeWire/PulseAudio
//! null sinks. XML is defined through `virsh define /dev/stdin`. A restart is
//! never performed unless `--provision-restart` is also passed. Use
//! `--no-auto-manage` for discovery/agent-only operation.

mod agent;
mod audio_provisioning;
mod discovery;
mod evdev_provisioning;
mod manager;
mod provisioning;
mod status;
mod supervisor;
mod vm_info;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use audio_provisioning::AudioProvisionConfig;
use clap::Parser;
use evdev_provisioning::EvdevProvisionConfig;
use manager::{ManagerOptions, VirtualNodeManager};
use provisioning::{ProvisionConfig, DEFAULT_QMP_DIR};
use tracing::info;

/// Ozma Virtual Node — discover VMs and supervise one `ozma-node` per VM.
///
/// Discovery, status, agent provisioning, per-VM port allocation, subprocess
/// start/stop/restart, shutdown, and opt-in libvirt QMP/display provisioning
/// are implemented here. Editing libvirt XML is off by default; restarting a VM
/// is a second explicit opt-in.
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
                  With --provision, a libvirt domain that has no QMP socket gets an ozma \
                  qemu command line (QMP control/input/display sockets, D-Bus p2p display, \
                  optional evdev and PulseAudio environment) defined through \
                  `virsh define /dev/stdin`. Editing libvirt XML is opt-in. --provision-evdev \
                  may create virtual keyboard/pointer devices with uinput; --provision-audio \
                  may create one null sink per VM through PipeWire/PulseAudio's pactl. Both \
                  device paths are non-fatal and are only passed on when creation succeeded. \
                  The VM must usually be restarted for the new sockets to exist; that only \
                  happens with the second opt-in, --provision-restart. Proxmox configuration is \
                  never edited. VMs without a discovered or provisioned QMP socket are skipped \
                  with a warning; no socket path is fabricated. Use --no-auto-manage for \
                  discovery/agent-only operation."
)]
struct Args {
    /// Controller URL (e.g. http://10.0.0.1:7380)
    ///
    /// Passed to each per-VM node as `--register-url`.
    #[arg(long, default_value = "")]
    controller: String,

    /// Directory to scan for QMP sockets, and for provisioned sockets when --provision is on
    ///
    /// Discovery falls back to `/tmp` when this is empty; provisioning uses
    /// `/run/ozma/qmp` when this is empty.
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

    /// Read-only supervisor status/metrics HTTP port (0 disables it)
    ///
    /// Serves `/health`, `/status`, `/metrics`, and `/api/v1/connection` for
    /// the whole hypervisor. The document aggregates every supervised VM,
    /// including skipped and excluded ones. No secrets are exposed.
    #[arg(long, default_value_t = 0)]
    status_port: u16,

    /// Poll each running child's `/api/v1/connection` every N milliseconds
    ///
    /// `0` (the default) disables child aggregation. When enabled, the
    /// supervisor caches a bounded per-VM summary and reports `last_seen`/`stale`
    /// honestly. Polls run independently and never block `/status`.
    #[arg(long, default_value_t = 0)]
    child_poll_interval_ms: u64,

    /// Per-child HTTP timeout for status polling, in milliseconds
    ///
    /// Only used when `--child-poll-interval-ms` is greater than zero.
    #[arg(long, default_value_t = 2000)]
    child_poll_timeout_ms: u64,

    /// Optional bearer token sent to child APIs while polling status
    ///
    /// Read from `OZMA_NODE_API_TOKEN` when not passed. The token is never
    /// included in `/status`, `/metrics`, `/health`, or logs.
    #[arg(long, default_value = "", env = "OZMA_NODE_API_TOKEN", hide = true)]
    child_api_token: String,

    /// Opt in to defining ozma QMP/display XML on libvirt domains
    ///
    /// Proxmox configuration is never edited.
    #[arg(long)]
    provision: bool,

    /// Allow the provisioner to run `virsh destroy` and `virsh start`
    ///
    /// Requires --provision. Off by default; without it the new XML is defined
    /// and the VM must be restarted by the operator.
    #[arg(long)]
    provision_restart: bool,

    /// PULSE_SERVER value injected into QEMU's environment when --provision is on
    #[arg(long, value_name = "VALUE")]
    pulse_server: Option<String>,

    /// XDG_RUNTIME_DIR value injected into QEMU's environment when --provision is on
    #[arg(long, value_name = "PATH")]
    xdg_runtime_dir: Option<String>,

    /// evdev keyboard path injected as input-linux when --provision is on
    #[arg(long, value_name = "PATH")]
    evdev_kbd: Option<PathBuf>,

    /// evdev mouse path injected as input-linux when --provision is on
    #[arg(long, value_name = "PATH")]
    evdev_mouse: Option<PathBuf>,

    /// Create per-VM evdev keyboard/pointer devices with uinput when --provision is on
    ///
    /// Requires --provision. Conflicts with --evdev-kbd/--evdev-mouse.
    /// Requires access to /dev/uinput on Linux; creation is skipped with a
    /// warning otherwise.
    #[arg(long)]
    provision_evdev: bool,

    /// Directory for per-VM evdev marker files
    #[arg(long, default_value = evdev_provisioning::DEFAULT_EVDEV_MARKER_DIR, value_name = "PATH")]
    evdev_marker_dir: PathBuf,

    /// Opt in to creating one audio sink per VM
    #[arg(long)]
    provision_audio: bool,

    /// Audio backend for per-VM sinks
    #[arg(long, value_enum, default_value_t = audio_provisioning::AudioBackend::None)]
    audio_backend: audio_provisioning::AudioBackend,

    /// Prefix for per-VM audio sink names (`<prefix><vm>`)
    #[arg(long, default_value = audio_provisioning::DEFAULT_AUDIO_SINK_PREFIX)]
    audio_sink_prefix: String,

    /// Directory containing reference tracks for the per-VM audio router
    #[arg(long, value_name = "PATH")]
    audio_track_root: Option<PathBuf>,

    /// JSON file recording per-VM audio sink ownership across supervisor restarts
    ///
    /// Defaults to `$XDG_RUNTIME_DIR/ozma/audio-provision.json`, falling back
    /// to `/run/ozma/audio-provision.json`. Only sinks recorded here are
    /// unloaded; an existing sink this supervisor did not create is left alone.
    #[arg(long, value_name = "PATH")]
    audio_state_file: Option<PathBuf>,

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

    let provision_qmp_dir = if args.qmp_dir.trim().is_empty() {
        PathBuf::from(DEFAULT_QMP_DIR)
    } else {
        PathBuf::from(args.qmp_dir.trim())
    };
    let provision_config = ProvisionConfig {
        qmp_dir: provision_qmp_dir,
        restart_vm: args.provision_restart,
        pulse_server: args.pulse_server,
        xdg_runtime_dir: args.xdg_runtime_dir,
        evdev_keyboard: args.evdev_kbd,
        evdev_mouse: args.evdev_mouse,
        evdev: EvdevProvisionConfig {
            marker_dir: args.evdev_marker_dir,
            enabled: args.provision_evdev,
            ..EvdevProvisionConfig::default()
        },
        ..ProvisionConfig::default()
    };

    let audio = AudioProvisionConfig {
        enabled: args.provision_audio,
        backend: args.audio_backend,
        sink_prefix: args.audio_sink_prefix,
        track_root: args.audio_track_root,
        state_path: args
            .audio_state_file
            .unwrap_or_else(audio_provisioning::default_state_path),
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
        child_poll_interval: Duration::from_millis(args.child_poll_interval_ms),
        child_poll_timeout: Duration::from_millis(args.child_poll_timeout_ms),
        child_api_token: args.child_api_token,
        provision: args.provision,
        provision_config,
        audio,
        ..ManagerOptions::default()
    };

    let mgr = Arc::new(VirtualNodeManager::new(options));

    // Reject bad supervisor settings before installing signal handlers or
    // spawning any work.
    mgr.validate()?;

    // Optional read-only status/metrics listener for the whole supervisor.
    if args.status_port != 0 {
        let listener = status::bind(args.status_port).await?;
        let bound_port = listener.local_addr()?.port();
        info!(
            "Supervisor status API listening on http://0.0.0.0:{}/ (health, status, metrics)",
            bound_port
        );
        let status_mgr = Arc::clone(&mgr);
        let shutdown = mgr.subscribe_shutdown();
        tokio::spawn(async move {
            if let Err(error) = status::serve(status_mgr, listener, bound_port, shutdown).await {
                tracing::warn!("Supervisor status API stopped: {}", error);
            }
        });
    }

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
        assert_eq!(args.status_port, 0);
        assert_eq!(args.child_poll_interval_ms, 0);
        assert_eq!(args.child_poll_timeout_ms, 2000);
        assert!(!args.no_auto_manage);
        assert!(!args.no_auto_agent);
        assert!(!args.provision);
        assert!(!args.provision_restart);
        assert!(args.pulse_server.is_none());
        assert!(args.xdg_runtime_dir.is_none());
        assert!(args.evdev_kbd.is_none());
        assert!(args.evdev_mouse.is_none());
        assert!(!args.provision_evdev);
        assert_eq!(
            args.evdev_marker_dir,
            PathBuf::from(evdev_provisioning::DEFAULT_EVDEV_MARKER_DIR)
        );
        assert!(!args.provision_audio);
        assert_eq!(args.audio_backend, audio_provisioning::AudioBackend::None);
        assert_eq!(
            args.audio_sink_prefix,
            audio_provisioning::DEFAULT_AUDIO_SINK_PREFIX
        );
        assert!(args.audio_track_root.is_none());
        assert!(args.audio_state_file.is_none());
    }

    #[test]
    fn provisioning_flags_parse() {
        let args = Args::try_parse_from([
            "ozma-virtual-node",
            "--provision",
            "--provision-restart",
            "--qmp-dir",
            "/run/ozma/qmp",
            "--pulse-server",
            "unix:/run/user/1000/pulse/native",
            "--xdg-runtime-dir",
            "/run/user/1000",
            "--evdev-kbd",
            "/dev/input/event5",
            "--evdev-mouse",
            "/dev/input/event6",
        ])
        .expect("parse provisioning flags");

        assert!(args.provision);
        assert!(args.provision_restart);
        assert_eq!(args.qmp_dir, "/run/ozma/qmp");
        assert_eq!(
            args.pulse_server.as_deref(),
            Some("unix:/run/user/1000/pulse/native")
        );
        assert_eq!(args.xdg_runtime_dir.as_deref(), Some("/run/user/1000"));
        assert_eq!(args.evdev_kbd, Some(PathBuf::from("/dev/input/event5")));
        assert_eq!(args.evdev_mouse, Some(PathBuf::from("/dev/input/event6")));
    }

    #[test]
    fn status_port_flag_parses_and_zero_disables_it() {
        let args = Args::try_parse_from(["ozma-virtual-node", "--status-port", "9555"])
            .expect("parse status port");
        assert_eq!(args.status_port, 9555);

        let defaults = Args::try_parse_from(["ozma-virtual-node"]).expect("parse defaults");
        assert_eq!(defaults.status_port, 0);
    }

    #[test]
    fn child_poll_flags_parse_and_interval_zero_disables_polling() {
        let args = Args::try_parse_from([
            "ozma-virtual-node",
            "--child-poll-interval-ms",
            "5000",
            "--child-poll-timeout-ms",
            "750",
        ])
        .expect("parse child poll flags");
        assert_eq!(args.child_poll_interval_ms, 5000);
        assert_eq!(args.child_poll_timeout_ms, 750);

        let defaults = Args::try_parse_from(["ozma-virtual-node"]).expect("parse defaults");
        assert_eq!(defaults.child_poll_interval_ms, 0);
        assert_eq!(defaults.child_poll_timeout_ms, 2000);
    }

    #[test]
    fn device_and_audio_provisioning_flags_parse() {
        let args = Args::try_parse_from([
            "ozma-virtual-node",
            "--provision",
            "--provision-evdev",
            "--evdev-marker-dir",
            "/run/ozma/markers",
            "--provision-audio",
            "--audio-backend",
            "pulseaudio",
            "--audio-sink-prefix",
            "ozma-vm-",
            "--audio-track-root",
            "/srv/tracks",
            "--audio-state-file",
            "/run/ozma/custom-audio-provision.json",
        ])
        .expect("parse device/audio provisioning flags");

        assert!(args.provision_evdev);
        assert_eq!(args.evdev_marker_dir, PathBuf::from("/run/ozma/markers"));
        assert!(args.provision_audio);
        assert_eq!(
            args.audio_backend,
            audio_provisioning::AudioBackend::PulseAudio
        );
        assert_eq!(args.audio_sink_prefix, "ozma-vm-");
        assert_eq!(args.audio_track_root, Some(PathBuf::from("/srv/tracks")));
        assert_eq!(
            args.audio_state_file,
            Some(PathBuf::from("/run/ozma/custom-audio-provision.json"))
        );
    }

    #[test]
    fn audio_backend_accepts_only_documented_names() {
        for (value, expected) in [
            ("none", audio_provisioning::AudioBackend::None),
            ("pipewire", audio_provisioning::AudioBackend::PipeWire),
            ("pulseaudio", audio_provisioning::AudioBackend::PulseAudio),
        ] {
            let args = Args::try_parse_from(["ozma-virtual-node", "--audio-backend", value])
                .expect("documented backend");
            assert_eq!(args.audio_backend, expected);
        }
        assert!(Args::try_parse_from(["ozma-virtual-node", "--audio-backend", "alsa"]).is_err());
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
