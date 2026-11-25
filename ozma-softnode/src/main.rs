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

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context};
use audio_provisioning::AudioProvisionConfig;
use clap::Parser;
use evdev_provisioning::EvdevProvisionConfig;
use manager::{ManagerOptions, VirtualNodeManager};
use provisioning::{ProvisionConfig, DEFAULT_QMP_DIR};
use supervisor::{
    ChildOptions, GpuPassthroughChildOptions, HlsChildOptions, MeshChildOptions,
    MultiseatChildOptions, NodeFlagSupport, UnsupportedPolicy, UsbChildOptions, VncChildOptions,
    WebRtcChildOptions,
};
use tracing::{info, warn};

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

    /// Opt in to the child `--hls` pipeline (capture → ffmpeg → `/stream/*`)
    ///
    /// Off by default. Without it, supervised children advertise no stream and
    /// the controller's HLS proxy gets 503, exactly as before this flag existed.
    #[arg(long)]
    hls: bool,

    /// Root directory for per-VM HLS output (`<root>/<vm>`)
    ///
    /// Requires --hls or --multi-console-hls. When unset the child keeps its
    /// own `/tmp/ozma-stream-<name>` default.
    #[arg(long, value_name = "PATH")]
    hls_dir: Option<PathBuf>,

    /// Child HLS input frame rate
    #[arg(long, value_name = "FPS")]
    hls_fps: Option<u32>,

    /// Child HLS H.264/HEVC bitrate in kbps
    #[arg(long, value_name = "KBPS")]
    hls_bitrate_kbps: Option<u32>,

    /// Child HLS encoder (also the "codec" selector), for example `libx264`,
    /// `hevc_vaapi`, or `h264_nvenc`
    #[arg(long, value_name = "ENCODER")]
    hls_encoder: Option<String>,

    /// Child HLS pixel format (`yuv420p` or `p010`)
    #[arg(long, value_name = "FORMAT")]
    hls_pixel_format: Option<String>,

    /// Select the 10-bit `p010` HLS target
    #[arg(long)]
    hls_10bit: bool,

    /// Signal HDR color metadata (requires a 10-bit target)
    #[arg(long)]
    hls_hdr: bool,

    /// HDR color primaries (for example `bt2020`)
    #[arg(long, value_name = "NAME")]
    hls_color_primaries: Option<String>,

    /// HDR transfer characteristic (for example `smpte2084`)
    #[arg(long, value_name = "NAME")]
    hls_color_transfer: Option<String>,

    /// HDR color matrix (for example `bt2020nc`)
    #[arg(long, value_name = "NAME")]
    hls_color_matrix: Option<String>,

    /// Run one HLS pipeline per advertised VM console (implies --hls)
    ///
    /// Forwarded as `--multi-console-hls`; single-head QMP/VNC/Looking Glass
    /// children report a typed limitation rather than a fake per-console stream.
    #[arg(long)]
    multi_console_hls: bool,

    /// Maximum simultaneous per-console HLS pipelines
    #[arg(long, value_name = "N")]
    multi_console_hls_max: Option<usize>,

    /// Idle seconds before a non-active per-console HLS pipeline stops
    #[arg(long, value_name = "SECONDS")]
    multi_console_hls_idle_seconds: Option<u64>,

    /// Per-VM display output for the controller picker, repeatable
    ///
    /// Form: `VM=LABEL:WxH[:PRIMARY][:SOURCE]` or
    /// `VM=INDEX:LABEL:WxH[:PRIMARY][:SOURCE]`. Forwarded only to that VM's
    /// child as `--display-output`.
    #[arg(long, value_name = "VM=SPEC")]
    display_output: Vec<String>,

    /// Per-VM guest IP advertised by the child, repeatable (`VM=IP`)
    #[arg(long, value_name = "VM=IP")]
    vm_guest_ip: Vec<String>,

    /// Select VNC capture for VMs whose discovery backend exposed a VNC port
    ///
    /// This forwards `--capture-provider vnc --vnc-host --vnc-port`. It does not
    /// register VNC metadata with the controller; `ozma-node` has no such field
    /// yet. GPU-passthrough VMs are never switched to VNC.
    #[arg(long)]
    vnc_capture: bool,

    /// Serve each selected child's capture source as an RFB 3.8/VNC endpoint
    ///
    /// Security is RFB None (no password, TLS, or authentication). The node
    /// binds loopback by default. Without --vnc-vm every supervised child
    /// serves its own capture source. This is independent of --vnc-capture.
    #[arg(long)]
    vnc_server: bool,

    /// Bind address for --vnc-server (`127.0.0.1` by default)
    ///
    /// Supplying a non-loopback address is the explicit opt-in; the node
    /// refuses one otherwise.
    #[arg(long, value_name = "ADDR")]
    vnc_bind: Option<String>,

    /// Fixed TCP port for --vnc-server (0 or unset = OS-assigned per child)
    ///
    /// A non-zero port requires --vnc-vm: two children on one host cannot bind
    /// the same port. Leave it unset/0 for multi-VM hosts so every child gets a
    /// distinct OS-assigned port, which the node registers with the controller.
    #[arg(long, value_name = "PORT")]
    vnc_server_port: Option<u16>,

    /// Scope all --vnc-server* flags to one VM (required for a fixed port)
    #[arg(long, value_name = "VM")]
    vnc_vm: Option<String>,

    /// External WebRTC helper executable (for example `/usr/bin/python3`)
    ///
    /// WebRTC stays disabled when unset. Other --webrtc-* options require this.
    #[arg(long, value_name = "PATH")]
    webrtc_helper: Option<PathBuf>,

    /// First argument passed to the WebRTC helper (the worker script)
    #[arg(long, value_name = "PATH")]
    webrtc_helper_script: Option<PathBuf>,

    /// Directory for per-VM WebRTC signaling sockets (`<dir>/<vm>.sock`)
    #[arg(long, value_name = "PATH")]
    webrtc_socket_dir: Option<PathBuf>,

    /// Directory for per-VM WebRTC shared-memory frames (`<dir>/<vm>.shm`)
    #[arg(long, value_name = "PATH")]
    webrtc_shm_dir: Option<PathBuf>,

    /// WebRTC helper frame width
    #[arg(long, value_name = "PIXELS")]
    webrtc_width: Option<u32>,

    /// WebRTC helper frame height
    #[arg(long, value_name = "PIXELS")]
    webrtc_height: Option<u32>,

    /// Fit the WebRTC helper to each VM's runtime capture size
    #[arg(long)]
    webrtc_fit_capture: bool,

    /// WebRTC helper frame rate
    #[arg(long, value_name = "FPS")]
    webrtc_fps: Option<u32>,

    /// WebRTC H.264 bitrate in kbps
    #[arg(long, value_name = "KBPS")]
    webrtc_bitrate_kbps: Option<u32>,

    /// Derive `--webrtc-audio-source <audio-sink>.monitor` per VM
    #[arg(long)]
    webrtc_audio_from_sink: bool,

    /// WebRTC audio backend: `auto`, `pulse`, or `pipewire`
    #[arg(long, value_name = "BACKEND", value_parser = ["auto", "pulse", "pipewire"])]
    webrtc_audio_backend: Option<String>,

    /// Optional STUN server URI for WebRTC ICE
    #[arg(long, value_name = "URI")]
    webrtc_stun: Option<String>,

    /// Optional TURN server URI without credentials
    #[arg(long, value_name = "URI")]
    webrtc_turn: Option<String>,

    /// TURN username (redacted from status and logs)
    #[arg(long, value_name = "USER")]
    webrtc_turn_user: Option<String>,

    /// TURN password (redacted from status and logs)
    #[arg(long, value_name = "PASSWORD", hide = true)]
    webrtc_turn_password: Option<String>,

    /// Enable one WebRTC helper per advertised VM console
    #[arg(long)]
    multi_console_webrtc: bool,

    /// Maximum simultaneous per-console WebRTC helpers
    #[arg(long, value_name = "N")]
    multi_console_webrtc_max: Option<usize>,

    /// Idle seconds before a non-active per-console WebRTC helper stops
    #[arg(long, value_name = "SECONDS")]
    multi_console_webrtc_idle_seconds: Option<u64>,

    /// Opt in to USB gadget/media forwarding for exactly one VM child
    ///
    /// Requires --usb-vm and --usb-udc. USB is host-scoped, so the supervisor
    /// forwards it to one child only instead of racing several children for the
    /// same UDC.
    #[arg(long)]
    usb_gadget: bool,

    /// The single VM whose child receives the USB gadget flags
    #[arg(long, value_name = "VM")]
    usb_vm: Option<String>,

    /// UDC the USB gadget binds to
    #[arg(long, value_name = "UDC")]
    usb_udc: Option<String>,

    /// ConfigFS composite-gadget root
    #[arg(long, value_name = "PATH")]
    usb_configfs_root: Option<PathBuf>,

    /// Disk image or ISO exposed as the mass-storage LUN
    #[arg(long, value_name = "PATH")]
    usb_media: Option<PathBuf>,

    /// Expose the mass-storage LUN read-only
    #[arg(long)]
    usb_media_ro: bool,

    /// Present the mass-storage image as a read-only CD-ROM
    #[arg(long)]
    usb_media_cdrom: bool,

    /// Add a UAC2 audio function (descriptors only)
    #[arg(long)]
    usb_uac2: bool,

    /// Start the UAC2 isochronous ↔ host-audio bridge
    ///
    /// Requires --usb-uac2 and is forwarded only to the --usb-vm child. The
    /// node starts the bridge only when the gadget's ALSA card and the host
    /// audio devices exist; otherwise `/usb` reports a typed `unsupported`
    /// bridge rather than claiming live audio.
    #[arg(long)]
    usb_uac2_bridge: bool,

    /// Host playback device receiving host→device UAC2 PCM
    ///
    /// Omitted by default so the node uses its own `default` ALSA device.
    #[arg(long, value_name = "ALSA")]
    usb_uac2_sink_device: Option<String>,

    /// Host capture device feeding device→host UAC2 PCM
    ///
    /// Omitted by default so the node uses its own `default` ALSA device.
    #[arg(long, value_name = "ALSA")]
    usb_uac2_source_device: Option<String>,

    /// Add a FunctionFS vendor function
    #[arg(long)]
    usb_vendor: bool,

    /// Optional absolute FunctionFS vendor daemon path
    #[arg(long, value_name = "PATH")]
    usb_vendor_daemon: Option<PathBuf>,

    /// Report a persisted multiseat plan at `GET /api/v1/seats` for one node
    ///
    /// Read-only: the supervisor never starts, stops, or restarts seats. The
    /// standalone `ozma-multiseat` supervisor keeps that lifecycle. Requires
    /// --seats-file and --multiseat-vm so exactly one child owns the report.
    #[arg(long)]
    multiseat: bool,

    /// Versioned `seats.json` store shared with the multiseat supervisor
    #[arg(long, value_name = "PATH")]
    seats_file: Option<PathBuf>,

    /// The single VM whose child serves the multiseat report
    #[arg(long, value_name = "VM")]
    multiseat_vm: Option<String>,

    /// Opt in to the node-local mesh service for one VM child
    ///
    /// A mesh identity is unique per node, so --mesh-vm selects the single
    /// child that binds the WireGuard UDP port and advertises the identity.
    #[arg(long)]
    mesh: bool,

    /// IPv4 address assigned to the mesh VM
    #[arg(long, value_name = "IP")]
    mesh_ip: Option<String>,

    /// WireGuard UDP port for mesh traffic (node default 51820)
    #[arg(long, value_name = "PORT")]
    mesh_port: Option<u16>,

    /// Base64 X25519 WireGuard private-key file (path only; never read here)
    #[arg(long, value_name = "PATH")]
    mesh_key_file: Option<PathBuf>,

    /// Request kernel WireGuard (not implemented; the node reports typed
    /// unsupported and stays on userspace boringtun)
    #[arg(long)]
    mesh_kernel: bool,

    /// Repeatable mesh peer JSON object, forwarded as `--mesh-peer`
    #[arg(long = "mesh-peer", value_name = "JSON")]
    mesh_peer: Vec<String>,

    /// The single VM whose child owns this mesh identity
    #[arg(long, value_name = "VM")]
    mesh_vm: Option<String>,

    /// Per-VM Looking Glass KVMFR shared-memory path, repeatable (`VM=PATH`)
    ///
    /// Used as `--capture-provider looking-glass --looking-glass-shm` for a VM
    /// discovery marks as GPU passthrough. Without a mapping the supervisor
    /// keeps qmp capture and reports that the agent-capture path is unsupervised.
    #[arg(long, value_name = "VM=PATH")]
    looking_glass_shm: Vec<String>,

    /// Looking Glass polling rate in frames per second
    #[arg(long, value_name = "FPS")]
    looking_glass_fps: Option<u32>,

    /// Looking Glass capture source id advertised by the child
    #[arg(long, value_name = "ID")]
    looking_glass_id: Option<String>,

    /// Looking Glass capture source name advertised by the child
    #[arg(long, value_name = "NAME")]
    looking_glass_name: Option<String>,

    /// Child audio routing backend (`none`, `pipewire`, `pulseaudio`)
    #[arg(long, value_name = "BACKEND", value_parser = ["none", "pipewire", "pulseaudio"])]
    audio_router: Option<String>,

    /// Derive `--audio-capture-target monitor:<audio-sink>` per VM
    #[arg(long)]
    audio_capture_from_sink: bool,

    /// Skip `ozma-node --help` probing and assume the current build's flags
    ///
    /// Probing enables typed "unsupported option" warnings in supervisor status.
    #[arg(long)]
    no_node_help_probe: bool,

    /// Treat a configured child option the target `ozma-node` does not support
    /// as a typed error (that VM gets no child) instead of a status warning
    #[arg(long)]
    reject_unsupported_node_flags: bool,

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

/// Split a repeatable `VM=VALUE` argument.
fn split_vm_value(raw: &str, option: &str) -> anyhow::Result<(String, String)> {
    let (vm, value) = raw
        .split_once('=')
        .with_context(|| format!("{option} expects VM=VALUE, got {raw:?}"))?;
    let vm = vm.trim();
    let value = value.trim();
    if vm.is_empty() || value.is_empty() {
        bail!("{option} expects a non-empty VM and value, got {raw:?}");
    }
    Ok((vm.to_owned(), value.to_owned()))
}

/// Parse repeatable `VM=VALUE` arguments, rejecting duplicate VM keys instead
/// of silently letting the last value win.
fn parse_vm_map(values: &[String], option: &str) -> anyhow::Result<HashMap<String, String>> {
    let mut parsed = HashMap::new();
    for raw in values {
        let (vm, value) = split_vm_value(raw, option)?;
        if parsed.insert(vm.clone(), value).is_some() {
            bail!("{option} was passed more than once for VM {vm:?}");
        }
    }
    Ok(parsed)
}

/// Parse repeatable `VM=VALUE` arguments where one VM may have several values.
fn parse_vm_lists(values: &[String], option: &str) -> anyhow::Result<HashMap<String, Vec<String>>> {
    let mut parsed: HashMap<String, Vec<String>> = HashMap::new();
    for raw in values {
        let (vm, value) = split_vm_value(raw, option)?;
        parsed.entry(vm).or_default().push(value);
    }
    Ok(parsed)
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
        track_root: args.audio_track_root.clone(),
        state_path: args
            .audio_state_file
            .unwrap_or_else(audio_provisioning::default_state_path),
    };

    // Probe the target node once so configured-but-unsupported flags are
    // reported in status instead of disappearing. `--help` is cheap and never
    // starts a VM. Discovery-only mode does not spawn children, so skip it.
    let node_flag_support = if args.no_node_help_probe || args.no_auto_manage {
        NodeFlagSupport::Unknown
    } else {
        match supervisor::probe_node_flags(&args.node_bin) {
            Ok(support) => {
                info!(
                    "Probed {} long flags from {} --help",
                    support.len(),
                    args.node_bin.display()
                );
                support
            }
            Err(error) => {
                warn!(
                    "Could not probe {} --help ({}); assuming the current build's \
                     flags. Configured unsupported options will fail at spawn rather \
                     than being reported in status.",
                    args.node_bin.display(),
                    error
                );
                NodeFlagSupport::Unknown
            }
        }
    };

    let child = ChildOptions {
        hls: HlsChildOptions {
            enabled: args.hls || args.multi_console_hls,
            dir_root: args.hls_dir,
            fps: args.hls_fps,
            bitrate_kbps: args.hls_bitrate_kbps,
            encoder: args.hls_encoder,
            pixel_format: args.hls_pixel_format,
            ten_bit: args.hls_10bit,
            hdr: args.hls_hdr,
            color_primaries: args.hls_color_primaries,
            color_transfer: args.hls_color_transfer,
            color_matrix: args.hls_color_matrix,
            multi_console: args.multi_console_hls,
            multi_console_max: args.multi_console_hls_max,
            multi_console_idle_seconds: args.multi_console_hls_idle_seconds,
        },
        display_outputs: parse_vm_lists(&args.display_output, "--display-output")?,
        guest_ips: parse_vm_map(&args.vm_guest_ip, "--vm-guest-ip")?,
        webrtc: WebRtcChildOptions {
            helper: args.webrtc_helper,
            helper_script: args.webrtc_helper_script,
            socket_dir: args.webrtc_socket_dir,
            shm_dir: args.webrtc_shm_dir,
            width: args.webrtc_width,
            height: args.webrtc_height,
            fit_capture: args.webrtc_fit_capture,
            fps: args.webrtc_fps,
            bitrate_kbps: args.webrtc_bitrate_kbps,
            audio_from_sink: args.webrtc_audio_from_sink,
            audio_backend: args.webrtc_audio_backend,
            stun: args.webrtc_stun,
            turn: args.webrtc_turn,
            turn_user: args.webrtc_turn_user,
            turn_password: args.webrtc_turn_password,
            multi_console: args.multi_console_webrtc,
            multi_console_max: args.multi_console_webrtc_max,
            multi_console_idle_seconds: args.multi_console_webrtc_idle_seconds,
        },
        vnc: VncChildOptions {
            capture: args.vnc_capture,
            server: args.vnc_server,
            bind: args.vnc_bind,
            server_port: args.vnc_server_port,
            vm: args.vnc_vm,
        },
        usb: UsbChildOptions {
            enabled: args.usb_gadget,
            vm: args.usb_vm,
            udc: args.usb_udc,
            configfs_root: args.usb_configfs_root,
            media: args.usb_media,
            media_ro: args.usb_media_ro,
            media_cdrom: args.usb_media_cdrom,
            uac2: args.usb_uac2,
            uac2_bridge: args.usb_uac2_bridge,
            uac2_sink_device: args.usb_uac2_sink_device,
            uac2_source_device: args.usb_uac2_source_device,
            vendor: args.usb_vendor,
            vendor_daemon: args.usb_vendor_daemon,
        },
        gpu: GpuPassthroughChildOptions {
            looking_glass: parse_vm_map(&args.looking_glass_shm, "--looking-glass-shm")?
                .into_iter()
                .map(|(vm, path)| (vm, PathBuf::from(path)))
                .collect(),
            looking_glass_fps: args.looking_glass_fps,
            looking_glass_id: args.looking_glass_id,
            looking_glass_name: args.looking_glass_name,
        },
        multiseat: MultiseatChildOptions {
            enabled: args.multiseat,
            seats_file: args.seats_file,
            vm: args.multiseat_vm,
        },
        mesh: MeshChildOptions {
            enabled: args.mesh,
            ip: args.mesh_ip,
            port: args.mesh_port,
            key_file: args.mesh_key_file,
            kernel: args.mesh_kernel,
            peers: args.mesh_peer,
            vm: args.mesh_vm,
        },
        audio_router: args.audio_router,
        audio_capture_from_sink: args.audio_capture_from_sink,
        audio_track_root: args.audio_track_root.clone(),
    };
    let unsupported_policy = if args.reject_unsupported_node_flags {
        UnsupportedPolicy::Reject
    } else {
        UnsupportedPolicy::Warn
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
        child,
        node_flag_support,
        unsupported_policy,
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
        assert!(!args.hls);
        assert!(args.hls_dir.is_none());
        assert!(args.hls_fps.is_none());
        assert!(!args.multi_console_hls);
        assert!(args.display_output.is_empty());
        assert!(args.vm_guest_ip.is_empty());
        assert!(!args.vnc_capture);
        assert!(!args.vnc_server);
        assert!(args.vnc_bind.is_none());
        assert!(args.vnc_server_port.is_none());
        assert!(args.vnc_vm.is_none());
        assert!(args.webrtc_helper.is_none());
        assert!(args.webrtc_turn_password.is_none());
        assert!(!args.usb_gadget);
        assert!(args.usb_vm.is_none());
        assert!(!args.usb_uac2_bridge);
        assert!(args.usb_uac2_sink_device.is_none());
        assert!(args.usb_uac2_source_device.is_none());
        assert!(!args.multiseat);
        assert!(args.seats_file.is_none());
        assert!(args.multiseat_vm.is_none());
        assert!(!args.mesh);
        assert!(args.mesh_ip.is_none());
        assert!(args.mesh_port.is_none());
        assert!(args.mesh_key_file.is_none());
        assert!(!args.mesh_kernel);
        assert!(args.mesh_peer.is_empty());
        assert!(args.mesh_vm.is_none());
        assert!(args.looking_glass_shm.is_empty());
        assert!(args.audio_router.is_none());
        assert!(!args.audio_capture_from_sink);
        assert!(!args.no_node_help_probe);
        assert!(!args.reject_unsupported_node_flags);
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

    #[test]
    fn child_argv_parity_flags_parse() {
        let args = Args::try_parse_from([
            "ozma-virtual-node",
            "--hls",
            "--hls-dir",
            "/srv/hls",
            "--hls-fps",
            "15",
            "--hls-encoder",
            "hevc_vaapi",
            "--multi-console-hls",
            "--display-output",
            "vm-a=0:Console_0:1920x1080:dbus",
            "--display-output",
            "vm-a=1:Console_1:1280x720",
            "--vm-guest-ip",
            "vm-a=10.0.0.42",
            "--vnc-capture",
            "--webrtc-helper",
            "/usr/bin/python3",
            "--webrtc-socket-dir",
            "/run/ozma/webrtc",
            "--webrtc-turn-password",
            "hunter2",
            "--usb-gadget",
            "--usb-vm",
            "vm-a",
            "--usb-udc",
            "dummy_udc.0",
            "--usb-media",
            "/srv/media/installer.iso",
            "--looking-glass-shm",
            "vm-b=/dev/shm/looking-glass",
            "--audio-router",
            "pipewire",
            "--audio-capture-from-sink",
        ])
        .expect("parse child argv parity flags");

        assert!(args.hls);
        assert_eq!(args.hls_dir, Some(PathBuf::from("/srv/hls")));
        assert_eq!(args.hls_fps, Some(15));
        assert_eq!(args.hls_encoder.as_deref(), Some("hevc_vaapi"));
        assert!(args.multi_console_hls);
        assert!(args.vnc_capture);
        assert_eq!(args.webrtc_helper, Some(PathBuf::from("/usr/bin/python3")));
        assert_eq!(args.webrtc_turn_password.as_deref(), Some("hunter2"));
        assert!(args.usb_gadget);
        assert_eq!(args.usb_vm.as_deref(), Some("vm-a"));
        assert_eq!(args.usb_udc.as_deref(), Some("dummy_udc.0"));
        assert_eq!(args.audio_router.as_deref(), Some("pipewire"));
        assert!(args.audio_capture_from_sink);

        let displays =
            parse_vm_lists(&args.display_output, "--display-output").expect("parse displays");
        assert_eq!(
            displays.get("vm-a"),
            Some(&vec![
                "0:Console_0:1920x1080:dbus".to_owned(),
                "1:Console_1:1280x720".to_owned(),
            ])
        );
        let guests = parse_vm_map(&args.vm_guest_ip, "--vm-guest-ip").expect("parse guest IPs");
        assert_eq!(guests.get("vm-a").map(String::as_str), Some("10.0.0.42"));
        let looking_glass = parse_vm_map(&args.looking_glass_shm, "--looking-glass-shm")
            .expect("parse looking glass");
        assert_eq!(
            looking_glass.get("vm-b").map(String::as_str),
            Some("/dev/shm/looking-glass")
        );
    }

    #[test]
    fn new_child_feature_flags_parse() {
        let args = Args::try_parse_from([
            "ozma-virtual-node",
            "--usb-gadget",
            "--usb-vm",
            "vm-a",
            "--usb-udc",
            "dummy_udc.0",
            "--usb-uac2",
            "--usb-uac2-bridge",
            "--usb-uac2-sink-device",
            "default",
            "--usb-uac2-source-device",
            "hw:1,0",
            "--vnc-server",
            "--vnc-bind",
            "127.0.0.1",
            "--vnc-server-port",
            "5901",
            "--vnc-vm",
            "vm-a",
            "--multiseat",
            "--seats-file",
            "/etc/ozma/seats.json",
            "--multiseat-vm",
            "vm-a",
            "--mesh",
            "--mesh-ip",
            "10.255.0.2",
            "--mesh-port",
            "51820",
            "--mesh-key-file",
            "/etc/ozma/mesh.key",
            "--mesh-peer",
            "{\"id\":\"peer-b\"}",
            "--mesh-vm",
            "vm-a",
        ])
        .expect("parse new child feature flags");

        assert!(args.usb_uac2_bridge);
        assert_eq!(args.usb_uac2_sink_device.as_deref(), Some("default"));
        assert_eq!(args.usb_uac2_source_device.as_deref(), Some("hw:1,0"));
        assert!(args.vnc_server);
        assert_eq!(args.vnc_bind.as_deref(), Some("127.0.0.1"));
        assert_eq!(args.vnc_server_port, Some(5901));
        assert_eq!(args.vnc_vm.as_deref(), Some("vm-a"));
        assert!(args.multiseat);
        assert_eq!(args.seats_file, Some(PathBuf::from("/etc/ozma/seats.json")));
        assert_eq!(args.multiseat_vm.as_deref(), Some("vm-a"));
        assert!(args.mesh);
        assert_eq!(args.mesh_ip.as_deref(), Some("10.255.0.2"));
        assert_eq!(args.mesh_port, Some(51820));
        assert_eq!(
            args.mesh_key_file,
            Some(PathBuf::from("/etc/ozma/mesh.key"))
        );
        assert_eq!(args.mesh_peer, vec!["{\"id\":\"peer-b\"}".to_owned()]);
        assert_eq!(args.mesh_vm.as_deref(), Some("vm-a"));

        let child = ChildOptions {
            vnc: VncChildOptions {
                server: args.vnc_server,
                bind: args.vnc_bind.clone(),
                server_port: args.vnc_server_port,
                vm: args.vnc_vm.clone(),
                ..VncChildOptions::default()
            },
            usb: UsbChildOptions {
                enabled: args.usb_gadget,
                vm: args.usb_vm.clone(),
                udc: args.usb_udc.clone(),
                uac2: args.usb_uac2,
                uac2_bridge: args.usb_uac2_bridge,
                uac2_sink_device: args.usb_uac2_sink_device.clone(),
                uac2_source_device: args.usb_uac2_source_device.clone(),
                ..UsbChildOptions::default()
            },
            multiseat: MultiseatChildOptions {
                enabled: args.multiseat,
                seats_file: args.seats_file.clone(),
                vm: args.multiseat_vm.clone(),
            },
            mesh: MeshChildOptions {
                enabled: args.mesh,
                ip: args.mesh_ip.clone(),
                port: args.mesh_port,
                key_file: args.mesh_key_file.clone(),
                kernel: args.mesh_kernel,
                peers: args.mesh_peer.clone(),
                vm: args.mesh_vm.clone(),
            },
            ..ChildOptions::default()
        };
        child.validate().expect("new flag combination is valid");
    }

    #[test]
    fn repeatable_vm_assignments_reject_duplicates_and_missing_values() {
        assert!(parse_vm_map(&["vm-a=10.0.0.1".to_owned()], "--vm-guest-ip").is_ok());
        assert!(parse_vm_map(
            &["vm-a=10.0.0.1".to_owned(), "vm-a=10.0.0.2".to_owned()],
            "--vm-guest-ip"
        )
        .is_err());
        assert!(parse_vm_map(&["vm-a".to_owned()], "--vm-guest-ip").is_err());
        assert!(parse_vm_map(&["=10.0.0.1".to_owned()], "--vm-guest-ip").is_err());
    }
}
