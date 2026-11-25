//! Per-VM `ozma-node` subprocess supervision.
//!
//! This module is deliberately limited to the runtime half of the
//! `ozma-softnode` supervisor:
//!
//! * allocate a stable, distinct HID UDP / HTTP port pair per VM,
//! * build the exact `ozma-node --profile vm` argv for a discovered VM,
//! * spawn, poll, restart, and stop child processes.
//!
//! QMP/display socket provisioning lives in the `provisioning` module. This
//! module only consumes a QMP path that discovery or the provisioner proved
//! usable. A VM without such a socket is skipped rather than given a fabricated
//! path.

use std::collections::{HashMap, HashSet};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use thiserror::Error;
use tracing::warn;

use crate::vm_info::VMInfo;

/// Default child binary. The real per-VM runtime is `ozma-node`, not
/// `ozma-softnode`.
pub const DEFAULT_NODE_BIN: &str = "ozma-node";
/// First HID UDP port handed to a VM. 7331 is `ozma-node`'s own default, so
/// 7332 leaves room for a hardware node on the same host.
pub const DEFAULT_BASE_PORT: u16 = 7332;
/// First HTTP port handed to a VM.
pub const DEFAULT_NODE_HTTP_BASE: u16 = 7382;
/// First restart delay.
pub const DEFAULT_RESTART_BACKOFF_MS: u64 = 1_000;
/// Maximum restart delay.
pub const DEFAULT_RESTART_BACKOFF_MAX_MS: u64 = 30_000;

/// A child that stayed up this long is considered healthy; its next failure
/// starts the backoff schedule from the initial delay again.
pub(crate) const HEALTHY_UPTIME: Duration = Duration::from_secs(30);
/// How long a child gets to exit after SIGTERM before it is killed.
const TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Long flags whose following value is a secret. The argv included in
/// supervisor status and logs always redacts these values.
pub const SECRET_CHILD_FLAGS: &[&str] = &[
    "--api-token",
    "--webrtc-turn-user",
    "--webrtc-turn-password",
];

/// Whether an option unsupported by the target `ozma-node` build is a warning
/// (the option is omitted and reported in status) or a hard error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum UnsupportedPolicy {
    /// Omit the flag and record a typed warning. This is the runtime default,
    /// so an older node binary keeps starting without silently claiming the
    /// feature.
    Warn,
    /// Reject the build with [`ChildArgvError::UnsupportedFlag`]. Used by
    /// configuration validation and tests.
    Reject,
}

/// Long flags advertised by a target `ozma-node --help`.
///
/// `Unknown` is the backward-compatible default: the supervisor assumes the
/// binary understands the flags this crate emits. `Probed` is produced by
/// [`probe_node_flags`] and enables honest unsupported-option warnings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum NodeFlagSupport {
    /// Not probed; every flag is assumed supported.
    #[default]
    Unknown,
    /// Flags extracted from `ozma-node --help`.
    Probed(HashSet<String>),
}

impl NodeFlagSupport {
    /// Parse long flags from clap help output (`--hls`, `--hls-dir <PATH>`, ...).
    pub fn from_help(help: &str) -> Self {
        let mut flags = HashSet::new();
        for word in help.split_whitespace() {
            let word = word.trim_matches(|character: char| {
                matches!(
                    character,
                    '`' | '\'' | '"' | ',' | ';' | ':' | '(' | ')' | '[' | ']'
                )
            });
            let flag = word.split('=').next().unwrap_or(word);
            if let Some(name) = flag.strip_prefix("--") {
                if !name.is_empty()
                    && name
                        .chars()
                        .all(|character| character.is_ascii_alphanumeric() || character == '-')
                {
                    flags.insert(flag.to_owned());
                }
            }
        }
        Self::Probed(flags)
    }

    /// Whether `flag` may be emitted for this target.
    pub fn supports(&self, flag: &str) -> bool {
        match self {
            Self::Unknown => true,
            Self::Probed(flags) => flags.contains(flag),
        }
    }

    /// Number of probed flags (`0` for [`Self::Unknown`]).
    pub fn len(&self) -> usize {
        match self {
            Self::Unknown => 0,
            Self::Probed(flags) => flags.len(),
        }
    }

    pub fn is_empty(&self) -> bool {
        matches!(self, Self::Probed(flags) if flags.is_empty())
    }
}

/// Probe `<node_bin> --help` for the long flags a target build advertises.
///
/// This runs one short-lived process and never starts a VM or opens a socket.
/// Callers that cannot probe (missing binary, non-zero exit) should keep
/// [`NodeFlagSupport::Unknown`] and log the failure.
pub fn probe_node_flags(program: &Path) -> Result<NodeFlagSupport> {
    const PROBE_TIMEOUT: Duration = Duration::from_secs(3);
    const POLL: Duration = Duration::from_millis(20);

    let mut child = Command::new(program)
        .arg("--help")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("running {} --help", program.display()))?;
    let started = Instant::now();
    let status = loop {
        if let Some(status) = child
            .try_wait()
            .with_context(|| format!("waiting for {} --help", program.display()))?
        {
            break status;
        }
        if started.elapsed() >= PROBE_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            bail!(
                "{} --help did not exit within {} ms",
                program.display(),
                PROBE_TIMEOUT.as_millis()
            );
        }
        std::thread::sleep(POLL);
    };
    let output = child
        .wait_with_output()
        .with_context(|| format!("reading {} --help output", program.display()))?;
    if !status.success() {
        bail!("{} --help exited with {}", program.display(), status);
    }
    let mut help = String::from_utf8_lossy(&output.stdout).into_owned();
    help.push('\n');
    help.push_str(&String::from_utf8_lossy(&output.stderr));
    let support = NodeFlagSupport::from_help(&help);
    if support.is_empty() {
        bail!("{} --help listed no long flags", program.display());
    }
    Ok(support)
}

/// Opt-in HLS forwarding for every supervised VM.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct HlsChildOptions {
    /// Pass `--hls`. Sub-options are rejected unless this (or
    /// `multi_console`) is set.
    pub enabled: bool,
    /// Root for per-VM `--hls-dir <root>/<vm>` values. `None` leaves the node's
    /// own `/tmp/ozma-stream-<name>` default.
    pub dir_root: Option<PathBuf>,
    pub fps: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    /// `--hls-encoder` (the supervisor-level "codec" choice).
    pub encoder: Option<String>,
    pub pixel_format: Option<String>,
    pub ten_bit: bool,
    pub hdr: bool,
    pub color_primaries: Option<String>,
    pub color_transfer: Option<String>,
    pub color_matrix: Option<String>,
    pub multi_console: bool,
    pub multi_console_max: Option<usize>,
    pub multi_console_idle_seconds: Option<u64>,
}

impl HlsChildOptions {
    fn requested(&self) -> bool {
        self.enabled || self.multi_console
    }

    fn has_sub_options(&self) -> bool {
        self.dir_root.is_some()
            || self.fps.is_some()
            || self.bitrate_kbps.is_some()
            || self.encoder.is_some()
            || self.pixel_format.is_some()
            || self.ten_bit
            || self.hdr
            || self.color_primaries.is_some()
            || self.color_transfer.is_some()
            || self.color_matrix.is_some()
            || self.multi_console_max.is_some()
            || self.multi_console_idle_seconds.is_some()
    }
}

/// Opt-in WebRTC helper forwarding.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WebRtcChildOptions {
    /// External helper executable. WebRTC stays disabled when absent.
    pub helper: Option<PathBuf>,
    pub helper_script: Option<PathBuf>,
    /// Per-VM socket directory: `<dir>/<vm>.sock`.
    pub socket_dir: Option<PathBuf>,
    /// Per-VM shared-memory directory: `<dir>/<vm>.shm`.
    pub shm_dir: Option<PathBuf>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    /// Use the child's runtime capture geometry instead of fixed dimensions.
    pub fit_capture: bool,
    pub fps: Option<u32>,
    pub bitrate_kbps: Option<u32>,
    /// Derive `--webrtc-audio-source <audio-sink>.monitor` per VM.
    pub audio_from_sink: bool,
    pub audio_backend: Option<String>,
    pub stun: Option<String>,
    pub turn: Option<String>,
    pub turn_user: Option<String>,
    pub turn_password: Option<String>,
    pub multi_console: bool,
    pub multi_console_max: Option<usize>,
    pub multi_console_idle_seconds: Option<u64>,
}

impl WebRtcChildOptions {
    fn configured(&self) -> bool {
        self.helper.is_some()
            || self.helper_script.is_some()
            || self.socket_dir.is_some()
            || self.shm_dir.is_some()
            || self.width.is_some()
            || self.height.is_some()
            || self.fit_capture
            || self.fps.is_some()
            || self.bitrate_kbps.is_some()
            || self.audio_from_sink
            || self.audio_backend.is_some()
            || self.stun.is_some()
            || self.turn.is_some()
            || self.turn_user.is_some()
            || self.turn_password.is_some()
            || self.multi_console
            || self.multi_console_max.is_some()
            || self.multi_console_idle_seconds.is_some()
    }
}

/// Opt-in VNC capture selection and/or serving for a VM child.
///
/// `capture` selects a discovered external QEMU VNC endpoint as the input
/// capture source. `server` makes the child serve its own capture source as an
/// RFB 3.8 endpoint. They are independent: a child may capture from QMP and
/// still rebroadcast that capture through `--vnc-server`.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct VncChildOptions {
    /// Select the discovered VNC endpoint as the capture provider.
    pub capture: bool,
    /// Serve the child's capture source as an RFB endpoint (`--vnc-server`).
    pub server: bool,
    /// `--vnc-bind`. The node defaults to loopback; a non-loopback bind must be
    /// explicit here.
    pub bind: Option<String>,
    /// Fixed `--vnc-server-port`. `None`/`0` lets each child ask the OS for a
    /// free port. A non-zero port requires [`Self::vm`], because two children
    /// on one host cannot bind the same TCP port.
    pub server_port: Option<u16>,
    /// When set, all VNC *server* flags are forwarded only to this VM.
    pub vm: Option<String>,
}

/// Host-scoped USB gadget/media forwarding, limited to one VM.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UsbChildOptions {
    pub enabled: bool,
    /// The only VM whose child receives the USB flags.
    pub vm: Option<String>,
    pub udc: Option<String>,
    pub configfs_root: Option<PathBuf>,
    pub media: Option<PathBuf>,
    pub media_ro: bool,
    pub media_cdrom: bool,
    pub uac2: bool,
    /// Start the UAC2 isochronous ↔ host-audio bridge (`--usb-uac2-bridge`).
    /// Requires `uac2`; without it the UAC2 function stays descriptor-only.
    pub uac2_bridge: bool,
    /// Host playback device receiving host→device PCM.
    pub uac2_sink_device: Option<String>,
    /// Host capture device feeding device→host PCM.
    pub uac2_source_device: Option<String>,
    pub vendor: bool,
    pub vendor_daemon: Option<PathBuf>,
}

impl UsbChildOptions {
    fn configured(&self) -> bool {
        self.enabled
            || self.udc.is_some()
            || self.configfs_root.is_some()
            || self.media.is_some()
            || self.media_ro
            || self.media_cdrom
            || self.uac2
            || self.uac2_bridge
            || self.uac2_sink_device.is_some()
            || self.uac2_source_device.is_some()
            || self.vendor
            || self.vendor_daemon.is_some()
    }
}

/// Opt-in read-only multiseat report for exactly one logical node.
///
/// The node only reads the persisted seat store; the standalone
/// `ozma-multiseat` supervisor keeps seat lifecycle. Because the store is
/// shared host state, only one supervised child may own the report.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultiseatChildOptions {
    pub enabled: bool,
    /// Versioned `seats.json` store (`--seats-file`).
    pub seats_file: Option<PathBuf>,
    /// The single child that serves `GET /api/v1/seats`.
    pub vm: Option<String>,
}

/// Opt-in node-local mesh service for exactly one logical node.
///
/// A mesh identity (mesh IP, WireGuard UDP port, private key) is unique per
/// node, so forwarding the same configuration to every per-VM child would bind
/// the same port and advertise the same identity several times. The supervisor
/// therefore forwards mesh flags to one named child only.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MeshChildOptions {
    pub enabled: bool,
    pub ip: Option<String>,
    pub port: Option<u16>,
    /// Path to the base64 X25519 private-key file. The supervisor never reads
    /// its contents; only the path is forwarded.
    pub key_file: Option<PathBuf>,
    pub kernel: bool,
    /// Repeatable `MeshNode` JSON peers.
    pub peers: Vec<String>,
    /// The single child that owns this mesh identity.
    pub vm: Option<String>,
}

/// GPU-passthrough capture over Looking Glass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GpuPassthroughChildOptions {
    /// VM name -> KVMFR shared-memory path (`--looking-glass-shm`).
    pub looking_glass: HashMap<String, PathBuf>,
    pub looking_glass_fps: Option<u32>,
    pub looking_glass_id: Option<String>,
    pub looking_glass_name: Option<String>,
}

/// Everything the supervisor may forward to a child beyond the core QMP/port
/// arguments.
///
/// The default value emits exactly the argv this supervisor emitted before the
/// Phase 4 parity work: QMP capture/input/power, name, ports, optional register
/// URL, optional audio sink, and nothing else.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ChildOptions {
    pub hls: HlsChildOptions,
    /// VM name -> repeated `--display-output` specs.
    pub display_outputs: HashMap<String, Vec<String>>,
    /// VM name -> `--vm-guest-ip` value.
    pub guest_ips: HashMap<String, String>,
    pub webrtc: WebRtcChildOptions,
    pub vnc: VncChildOptions,
    pub usb: UsbChildOptions,
    pub gpu: GpuPassthroughChildOptions,
    /// Read-only multiseat report owned by one logical child.
    pub multiseat: MultiseatChildOptions,
    /// Node-local mesh identity owned by one logical child.
    pub mesh: MeshChildOptions,
    /// `--audio-router` (`none`, `pipewire`, `pulseaudio`).
    pub audio_router: Option<String>,
    /// Derive `--audio-capture-target monitor:<audio-sink>` when a sink exists.
    pub audio_capture_from_sink: bool,
    pub audio_track_root: Option<PathBuf>,
}

impl ChildOptions {
    /// Reject option combinations that cannot be represented honestly in a
    /// child argv. This never probes the target binary.
    pub fn validate(&self) -> Result<(), ChildArgvError> {
        if !self.hls.requested() && self.hls.has_sub_options() {
            return Err(ChildArgvError::InvalidOption {
                option: "hls",
                detail: "HLS sub-options require --hls or --multi-console-hls".to_owned(),
            });
        }
        if self
            .hls
            .dir_root
            .as_deref()
            .is_some_and(|path| path.as_os_str().is_empty())
        {
            return Err(ChildArgvError::InvalidOption {
                option: "hls-dir",
                detail: "--hls-dir must not be empty".to_owned(),
            });
        }

        if self.webrtc.configured() && self.webrtc.helper.is_none() {
            return Err(ChildArgvError::InvalidOption {
                option: "webrtc-helper",
                detail: "--webrtc-* options require --webrtc-helper".to_owned(),
            });
        }
        if let Some(backend) = &self.webrtc.audio_backend {
            if !matches!(backend.as_str(), "auto" | "pulse" | "pipewire") {
                return Err(ChildArgvError::InvalidOption {
                    option: "webrtc-audio-backend",
                    detail: format!("{backend:?} is not auto, pulse, or pipewire"),
                });
            }
        }
        if let Some(router) = &self.audio_router {
            if !matches!(router.as_str(), "none" | "pipewire" | "pulseaudio") {
                return Err(ChildArgvError::InvalidOption {
                    option: "audio-router",
                    detail: format!("{router:?} is not none, pipewire, or pulseaudio"),
                });
            }
        }

        if self.usb.configured() {
            if !self.usb.enabled {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-gadget",
                    detail: "USB sub-options require --usb-gadget".to_owned(),
                });
            }
            if self.usb.vm.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-vm",
                    detail: "USB gadget forwarding requires --usb-vm <VM> so only one child binds the UDC"
                        .to_owned(),
                });
            }
            if self.usb.udc.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-udc",
                    detail: "USB gadget forwarding requires --usb-udc".to_owned(),
                });
            }
            if self.usb.uac2_bridge && !self.usb.uac2 {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-uac2-bridge",
                    detail: "--usb-uac2-bridge requires --usb-uac2".to_owned(),
                });
            }
            if !self.usb.uac2_bridge && self.usb.uac2_sink_device.is_some() {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-uac2-sink-device",
                    detail: "--usb-uac2-sink-device requires --usb-uac2-bridge".to_owned(),
                });
            }
            if !self.usb.uac2_bridge && self.usb.uac2_source_device.is_some() {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-uac2-source-device",
                    detail: "--usb-uac2-source-device requires --usb-uac2-bridge".to_owned(),
                });
            }
            if self
                .usb
                .uac2_sink_device
                .as_deref()
                .is_some_and(|device| device.trim().is_empty())
            {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-uac2-sink-device",
                    detail: "ALSA sink device must not be empty".to_owned(),
                });
            }
            if self
                .usb
                .uac2_source_device
                .as_deref()
                .is_some_and(|device| device.trim().is_empty())
            {
                return Err(ChildArgvError::InvalidOption {
                    option: "usb-uac2-source-device",
                    detail: "ALSA source device must not be empty".to_owned(),
                });
            }
        }

        if self.vnc.bind.is_some() && !self.vnc.server {
            return Err(ChildArgvError::InvalidOption {
                option: "vnc-bind",
                detail: "--vnc-bind requires --vnc-server".to_owned(),
            });
        }
        if self.vnc.server_port.is_some() && !self.vnc.server {
            return Err(ChildArgvError::InvalidOption {
                option: "vnc-server-port",
                detail: "--vnc-server-port requires --vnc-server".to_owned(),
            });
        }
        if self.vnc.vm.is_some() && !self.vnc.server {
            return Err(ChildArgvError::InvalidOption {
                option: "vnc-vm",
                detail: "--vnc-vm requires --vnc-server".to_owned(),
            });
        }
        if self
            .vnc
            .server_port
            .is_some_and(|port| port != 0 && self.vnc.vm.as_deref().unwrap_or("").trim().is_empty())
        {
            return Err(ChildArgvError::InvalidOption {
                option: "vnc-server-port",
                detail: "a non-zero --vnc-server-port requires --vnc-vm <VM>: two children \
                         cannot bind the same TCP port; leave the port unset/0 so each child \
                         gets a distinct OS-assigned port"
                    .to_owned(),
            });
        }

        if self.multiseat.enabled {
            if self.multiseat.vm.as_deref().unwrap_or("").trim().is_empty() {
                return Err(ChildArgvError::InvalidOption {
                    option: "multiseat-vm",
                    detail: "--multiseat requires --multiseat-vm <VM> so exactly one logical \
                             node owns the read-only seat report"
                        .to_owned(),
                });
            }
            if self
                .multiseat
                .seats_file
                .as_deref()
                .map_or(true, |path| path.as_os_str().is_empty())
            {
                return Err(ChildArgvError::InvalidOption {
                    option: "seats-file",
                    detail: "--multiseat requires --seats-file".to_owned(),
                });
            }
        } else {
            if self.multiseat.seats_file.is_some() {
                return Err(ChildArgvError::InvalidOption {
                    option: "seats-file",
                    detail: "--seats-file requires --multiseat".to_owned(),
                });
            }
            if self.multiseat.vm.is_some() {
                return Err(ChildArgvError::InvalidOption {
                    option: "multiseat-vm",
                    detail: "--multiseat-vm requires --multiseat".to_owned(),
                });
            }
        }

        let mesh_configured = self.mesh.enabled
            || self.mesh.ip.is_some()
            || self.mesh.port.is_some()
            || self.mesh.key_file.is_some()
            || self.mesh.kernel
            || !self.mesh.peers.is_empty();
        if mesh_configured && !self.mesh.enabled {
            return Err(ChildArgvError::InvalidOption {
                option: "mesh",
                detail: "mesh sub-options require --mesh".to_owned(),
            });
        }
        if self.mesh.enabled && self.mesh.vm.as_deref().unwrap_or("").trim().is_empty() {
            return Err(ChildArgvError::InvalidOption {
                option: "mesh-vm",
                detail: "the mesh identity is unique per node; --mesh requires --mesh-vm <VM> \
                         so only one child binds the UDP port and advertises the identity"
                    .to_owned(),
            });
        }
        if self.mesh.port == Some(0) {
            return Err(ChildArgvError::InvalidOption {
                option: "mesh-port",
                detail: "--mesh-port must be non-zero".to_owned(),
            });
        }
        if self.mesh.peers.iter().any(|peer| peer.trim().is_empty()) {
            return Err(ChildArgvError::InvalidOption {
                option: "mesh-peer",
                detail: "--mesh-peer must not be empty".to_owned(),
            });
        }

        for (vm, ip) in &self.guest_ips {
            if vm.trim().is_empty() {
                return Err(ChildArgvError::InvalidOption {
                    option: "vm-guest-ip",
                    detail: "VM name must not be empty".to_owned(),
                });
            }
            if ip.parse::<IpAddr>().is_err() {
                return Err(ChildArgvError::InvalidOption {
                    option: "vm-guest-ip",
                    detail: format!("{ip:?} for VM {vm:?} is not an IP address"),
                });
            }
        }

        for (vm, specs) in &self.display_outputs {
            if vm.trim().is_empty() {
                return Err(ChildArgvError::InvalidOption {
                    option: "display-output",
                    detail: "VM name must not be empty".to_owned(),
                });
            }
            for spec in specs {
                if !valid_display_output(spec) {
                    return Err(ChildArgvError::InvalidOption {
                        option: "display-output",
                        detail: format!(
                            "{spec:?} for VM {vm:?} is not \
                             LABEL:WxH[:PRIMARY][:SOURCE]"
                        ),
                    });
                }
            }
        }

        for (vm, path) in &self.gpu.looking_glass {
            if vm.trim().is_empty() || path.as_os_str().is_empty() {
                return Err(ChildArgvError::InvalidOption {
                    option: "looking-glass-shm",
                    detail: "requires VM=<non-empty path>".to_owned(),
                });
            }
        }

        Ok(())
    }
}

/// Typed child-argv failure. `UnsupportedFlag` is returned only when
/// [`UnsupportedPolicy::Reject`] is selected; the runtime uses warnings.
#[derive(Clone, Debug, Error, PartialEq, Eq)]
pub enum ChildArgvError {
    #[error("invalid --{option}: {detail}")]
    InvalidOption {
        option: &'static str,
        detail: String,
    },
    #[error("target ozma-node build does not support {flag} (required by --{option})")]
    UnsupportedFlag { option: &'static str, flag: String },
}

/// One option that was configured but not forwarded.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeArgvWarning {
    pub option: &'static str,
    pub flag: String,
    pub detail: String,
}

impl NodeArgvWarning {
    fn new(option: &'static str, flag: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            option,
            flag: flag.into(),
            detail: detail.into(),
        }
    }

    /// Human-readable one-line status/log message.
    pub fn message(&self) -> String {
        if self.flag.is_empty() {
            format!("--{} was not forwarded: {}", self.option, self.detail)
        } else {
            format!(
                "--{} was not forwarded to the child ({}): {}",
                self.option, self.flag, self.detail
            )
        }
    }
}

/// Result of [`build_child_argv`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeArgv {
    pub args: Vec<String>,
    pub warnings: Vec<NodeArgvWarning>,
}

/// Ports allocated to one VM's `ozma-node`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodePorts {
    pub hid_udp: u16,
    pub http: u16,
}

/// Everything needed to launch one per-VM `ozma-node`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NodeSpec {
    pub vm_name: String,
    pub program: PathBuf,
    pub args: Vec<String>,
    pub hid_udp_port: u16,
    pub http_port: u16,
    pub qmp_socket: String,
    /// Sink name passed as `--audio-sink`, when per-VM audio provisioning
    /// succeeded. `None` means the node reports no sink.
    pub audio_sink: Option<String>,
    /// Options that were configured but omitted because the target build does
    /// not advertise them (or because the VM cannot support them). Exposed in
    /// supervisor status; never silently dropped.
    pub warnings: Vec<NodeArgvWarning>,
}

impl NodeSpec {
    /// Program plus arguments, for logging and tests.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.to_string_lossy().into_owned());
        argv.extend(self.args.iter().cloned());
        argv
    }

    /// Program plus arguments with secret values replaced, for status/logs.
    pub fn redacted_argv(&self) -> Vec<String> {
        redact_argv(&self.argv())
    }

    /// Warning messages in stable order, for status.
    pub fn warning_messages(&self) -> Vec<String> {
        self.warnings.iter().map(NodeArgvWarning::message).collect()
    }
}

/// Replace the value following a [`SECRET_CHILD_FLAGS`] entry with
/// `<redacted>`. Handles both `--flag value` and `--flag=value`.
pub fn redact_argv(argv: &[String]) -> Vec<String> {
    let mut redacted = Vec::with_capacity(argv.len());
    let mut hide_next = false;
    for arg in argv {
        if hide_next {
            redacted.push("<redacted>".to_owned());
            hide_next = false;
            continue;
        }
        if SECRET_CHILD_FLAGS.contains(&arg.as_str()) {
            redacted.push(arg.clone());
            hide_next = true;
            continue;
        }
        if let Some((flag, _)) = arg.split_once('=') {
            if SECRET_CHILD_FLAGS.contains(&flag) {
                redacted.push(format!("{flag}=<redacted>"));
                continue;
            }
        }
        redacted.push(arg.clone());
    }
    redacted
}

struct ArgvBuilder<'a> {
    args: Vec<String>,
    warnings: Vec<NodeArgvWarning>,
    support: &'a NodeFlagSupport,
    policy: UnsupportedPolicy,
    error: Option<ChildArgvError>,
}

impl<'a> ArgvBuilder<'a> {
    fn new(support: &'a NodeFlagSupport, policy: UnsupportedPolicy) -> Self {
        Self {
            args: Vec::new(),
            warnings: Vec::new(),
            support,
            policy,
            error: None,
        }
    }

    /// Core flag: always emitted. These define the child's identity and the
    /// QMP/port contract; a build that lacks them is not a usable target.
    fn core(&mut self, flag: &'static str, value: impl Into<String>) {
        self.args.push(flag.to_owned());
        self.args.push(value.into());
    }

    /// Optional flag/value pair. Emitted only when `value` is `Some`.
    fn push(&mut self, option: &'static str, flag: &'static str, value: Option<impl Into<String>>) {
        let Some(value) = value else {
            return;
        };
        let value = value.into();
        if value.is_empty() {
            return;
        }
        if !self.accept(option, flag) {
            return;
        }
        self.args.push(flag.to_owned());
        self.args.push(value);
    }

    /// Optional boolean flag.
    fn push_flag(&mut self, option: &'static str, flag: &'static str, enabled: bool) {
        if !enabled {
            return;
        }
        if !self.accept(option, flag) {
            return;
        }
        self.args.push(flag.to_owned());
    }

    fn accept(&mut self, option: &'static str, flag: &'static str) -> bool {
        if self.support.supports(flag) {
            return true;
        }
        let error = ChildArgvError::UnsupportedFlag {
            option,
            flag: flag.to_owned(),
        };
        match self.policy {
            UnsupportedPolicy::Warn => self.warnings.push(NodeArgvWarning::new(
                option,
                flag,
                "the target ozma-node build does not advertise this flag",
            )),
            UnsupportedPolicy::Reject => {
                if self.error.is_none() {
                    self.error = Some(error);
                }
            }
        }
        false
    }

    /// Record a non-flag limitation, such as a GPU-passthrough VM with no
    /// configured Looking Glass path.
    fn warn(&mut self, option: &'static str, detail: impl Into<String>) {
        self.warnings.push(NodeArgvWarning::new(option, "", detail));
    }

    fn finish(self) -> Result<NodeArgv, ChildArgvError> {
        match self.error {
            Some(error) => Err(error),
            None => Ok(NodeArgv {
                args: self.args,
                warnings: self.warnings,
            }),
        }
    }
}

/// Build the argv for `ozma-node --profile vm`.
///
/// This is the single source of truth for child arguments. Core QMP/port flags
/// are always emitted; every opt-in flag is emitted only when configured, and
/// only once. A configured flag the target build does not advertise is either
/// warned about and omitted ([`UnsupportedPolicy::Warn`]) or rejected
/// ([`UnsupportedPolicy::Reject`]).
pub fn build_child_argv(
    vm: &VMInfo,
    ports: NodePorts,
    register_url: &str,
    audio_sink: Option<&str>,
    options: &ChildOptions,
    support: &NodeFlagSupport,
    policy: UnsupportedPolicy,
) -> Result<NodeArgv, ChildArgvError> {
    options.validate()?;

    let mut builder = ArgvBuilder::new(support, policy);
    builder.core("--profile", "vm");
    builder.core("--name", vm.name.clone());
    builder.core("--qmp-socket", vm.qmp_path.clone());

    let qmp_input = vm
        .qmp_input_path
        .as_deref()
        .map(str::trim)
        .filter(|path| !path.is_empty());
    if let Some(input) = qmp_input {
        builder.core("--qmp-input-socket", input.to_owned());
    }

    builder.core("--hid-udp-port", ports.hid_udp.to_string());
    builder.core("--http-port", ports.http.to_string());

    let looking_glass = vm
        .has_gpu_passthrough
        .then(|| options.gpu.looking_glass.get(&vm.name))
        .flatten();
    let vnc_capture = options.vnc.capture && !vm.has_gpu_passthrough && vm.vnc_port != 0;

    if vm.has_gpu_passthrough && looking_glass.is_none() {
        builder.warn(
            "gpu-passthrough",
            "VM has GPU passthrough but no --looking-glass-shm VM=PATH mapping; \
             capture stays qmp. The in-guest agent capture path is not supervised \
             from the hypervisor.",
        );
    } else if options.vnc.capture && !vm.has_gpu_passthrough && vm.vnc_port == 0 {
        builder.warn(
            "vnc-capture",
            "VNC capture was requested but discovery exposed no VNC port for this \
             VM; capture stays qmp.",
        );
    }

    let capture_provider = if looking_glass.is_some() {
        "looking-glass"
    } else if vnc_capture {
        "vnc"
    } else {
        "qmp"
    };

    builder.core("--capture-provider", capture_provider);
    builder.core("--input-provider", "qmp");
    builder.core("--power-provider", "qmp");

    let register_url = register_url.trim();
    builder.push(
        "register-url",
        "--register-url",
        (!register_url.is_empty()).then(|| register_url.to_owned()),
    );

    let audio_sink = audio_sink
        .map(str::trim)
        .filter(|sink| !sink.is_empty())
        .map(str::to_owned);
    if let Some(sink) = audio_sink.as_deref() {
        // `ozma-node` has `--audio-sink` but no `--audio-type` flag: the node
        // derives `audio_type=pipewire` from a non-empty sink.
        builder.push("audio-sink", "--audio-sink", Some(sink.to_owned()));
    }
    builder.push(
        "audio-router",
        "--audio-router",
        options.audio_router.clone(),
    );
    let audio_capture_target = options
        .audio_capture_from_sink
        .then(|| audio_sink.as_deref().map(|sink| format!("monitor:{sink}")))
        .flatten();
    if options.audio_capture_from_sink && audio_capture_target.is_none() {
        builder.warn(
            "audio-capture-from-sink",
            "no per-VM audio sink was provisioned, so \
             --audio-capture-target monitor:<sink> cannot be derived",
        );
    }
    builder.push(
        "audio-capture-target",
        "--audio-capture-target",
        audio_capture_target,
    );
    builder.push(
        "audio-track-root",
        "--audio-track-root",
        options
            .audio_track_root
            .as_ref()
            .map(|path| path.display().to_string()),
    );

    // HLS (the supervisor-level "codec" knobs live here).
    if options.hls.requested() {
        builder.push_flag("hls", "--hls", true);
        builder.push(
            "hls-dir",
            "--hls-dir",
            options
                .hls
                .dir_root
                .as_ref()
                .map(|root| per_vm_path(root, &vm.name, "").display().to_string()),
        );
        builder.push(
            "hls-fps",
            "--hls-fps",
            options.hls.fps.map(|value| value.to_string()),
        );
        builder.push(
            "hls-bitrate-kbps",
            "--hls-bitrate-kbps",
            options.hls.bitrate_kbps.map(|value| value.to_string()),
        );
        builder.push("hls-encoder", "--hls-encoder", options.hls.encoder.clone());
        builder.push(
            "hls-pixel-format",
            "--hls-pixel-format",
            options.hls.pixel_format.clone(),
        );
        builder.push_flag("hls-10bit", "--hls-10bit", options.hls.ten_bit);
        builder.push_flag("hls-hdr", "--hls-hdr", options.hls.hdr);
        builder.push(
            "hls-color-primaries",
            "--hls-color-primaries",
            options.hls.color_primaries.clone(),
        );
        builder.push(
            "hls-color-transfer",
            "--hls-color-transfer",
            options.hls.color_transfer.clone(),
        );
        builder.push(
            "hls-color-matrix",
            "--hls-color-matrix",
            options.hls.color_matrix.clone(),
        );
        builder.push_flag(
            "multi-console-hls",
            "--multi-console-hls",
            options.hls.multi_console,
        );
        builder.push(
            "multi-console-hls-max",
            "--multi-console-hls-max",
            options.hls.multi_console_max.map(|value| value.to_string()),
        );
        builder.push(
            "multi-console-hls-idle-seconds",
            "--multi-console-hls-idle-seconds",
            options
                .hls
                .multi_console_idle_seconds
                .map(|value| value.to_string()),
        );
    }

    // Display picker: explicit per-VM specs win; otherwise derive from the
    // discovery model when a backend populated it.
    let display_specs = options
        .display_outputs
        .get(&vm.name)
        .filter(|specs| !specs.is_empty())
        .cloned()
        .unwrap_or_else(|| discovered_display_outputs(vm));
    for spec in display_specs {
        builder.push("display-output", "--display-output", Some(spec));
    }

    builder.push(
        "vm-guest-ip",
        "--vm-guest-ip",
        options.guest_ips.get(&vm.name).cloned(),
    );

    // VNC capture selection. This is capture, not `vnc_host`/`vnc_port`
    // registration, which `ozma-node` does not implement yet.
    if capture_provider == "vnc" {
        let host = vm.vnc_host.trim();
        builder.push(
            "vnc-host",
            "--vnc-host",
            Some(if host.is_empty() {
                "127.0.0.1".to_owned()
            } else {
                host.to_owned()
            }),
        );
        builder.push("vnc-port", "--vnc-port", Some(vm.vnc_port.to_string()));
    }

    // Internal RFB server. It is independent of VNC *capture* above: a child
    // can serve its QMP/VNC/Looking Glass capture source. A fixed port is
    // single-VM scoped because sibling children cannot share a TCP port;
    // without a fixed port every child asks the OS for a distinct one.
    if options.vnc.server
        && options
            .vnc
            .vm
            .as_deref()
            .map_or(true, |name| name == vm.name.as_str())
    {
        builder.push_flag("vnc-server", "--vnc-server", true);
        builder.push("vnc-bind", "--vnc-bind", options.vnc.bind.clone());
        builder.push(
            "vnc-server-port",
            "--vnc-server-port",
            options
                .vnc
                .server_port
                .filter(|port| *port != 0)
                .map(|port| port.to_string()),
        );
    }

    if let Some(shm) = looking_glass {
        builder.push(
            "looking-glass-shm",
            "--looking-glass-shm",
            Some(shm.display().to_string()),
        );
        builder.push(
            "looking-glass-fps",
            "--looking-glass-fps",
            options.gpu.looking_glass_fps.map(|value| value.to_string()),
        );
        builder.push(
            "looking-glass-id",
            "--looking-glass-id",
            options.gpu.looking_glass_id.clone(),
        );
        builder.push(
            "looking-glass-name",
            "--looking-glass-name",
            options.gpu.looking_glass_name.clone(),
        );
    }

    // WebRTC helper. Absence of `--webrtc-helper` keeps the node's honest
    // `not-configured` state; `ChildOptions::validate` rejects other WebRTC
    // options without a helper.
    if let Some(helper) = &options.webrtc.helper {
        builder.push(
            "webrtc-helper",
            "--webrtc-helper",
            Some(helper.display().to_string()),
        );
        builder.push(
            "webrtc-helper-script",
            "--webrtc-helper-script",
            options
                .webrtc
                .helper_script
                .as_ref()
                .map(|path| path.display().to_string()),
        );
        builder.push(
            "webrtc-socket",
            "--webrtc-socket",
            options
                .webrtc
                .socket_dir
                .as_ref()
                .map(|dir| per_vm_path(dir, &vm.name, ".sock").display().to_string()),
        );
        builder.push(
            "webrtc-shm",
            "--webrtc-shm",
            options
                .webrtc
                .shm_dir
                .as_ref()
                .map(|dir| per_vm_path(dir, &vm.name, ".shm").display().to_string()),
        );
        builder.push(
            "webrtc-width",
            "--webrtc-width",
            options.webrtc.width.map(|value| value.to_string()),
        );
        builder.push(
            "webrtc-height",
            "--webrtc-height",
            options.webrtc.height.map(|value| value.to_string()),
        );
        if options.webrtc.fit_capture {
            builder.push_flag("webrtc-fit-capture", "--webrtc-fit-capture", true);
        }
        builder.push(
            "webrtc-fps",
            "--webrtc-fps",
            options.webrtc.fps.map(|value| value.to_string()),
        );
        builder.push(
            "webrtc-bitrate-kbps",
            "--webrtc-bitrate-kbps",
            options.webrtc.bitrate_kbps.map(|value| value.to_string()),
        );
        let webrtc_audio_source = options
            .webrtc
            .audio_from_sink
            .then(|| audio_sink.as_deref().map(|sink| format!("{sink}.monitor")))
            .flatten();
        if options.webrtc.audio_from_sink && webrtc_audio_source.is_none() {
            builder.warn(
                "webrtc-audio-from-sink",
                "no per-VM audio sink was provisioned, so \
                 --webrtc-audio-source <sink>.monitor cannot be derived; WebRTC \
                 audio stays disabled",
            );
        }
        builder.push(
            "webrtc-audio-source",
            "--webrtc-audio-source",
            webrtc_audio_source,
        );
        builder.push(
            "webrtc-audio-backend",
            "--webrtc-audio-backend",
            options.webrtc.audio_backend.clone(),
        );
        builder.push("webrtc-stun", "--webrtc-stun", options.webrtc.stun.clone());
        builder.push("webrtc-turn", "--webrtc-turn", options.webrtc.turn.clone());
        builder.push(
            "webrtc-turn-user",
            "--webrtc-turn-user",
            options.webrtc.turn_user.clone(),
        );
        builder.push(
            "webrtc-turn-password",
            "--webrtc-turn-password",
            options.webrtc.turn_password.clone(),
        );
        builder.push_flag(
            "multi-console-webrtc",
            "--multi-console-webrtc",
            options.webrtc.multi_console,
        );
        builder.push(
            "multi-console-webrtc-max",
            "--multi-console-webrtc-max",
            options
                .webrtc
                .multi_console_max
                .map(|value| value.to_string()),
        );
        builder.push(
            "multi-console-webrtc-idle-seconds",
            "--multi-console-webrtc-idle-seconds",
            options
                .webrtc
                .multi_console_idle_seconds
                .map(|value| value.to_string()),
        );
    }

    // USB/media is host-scoped; validate() guarantees exactly one VM is
    // selected, so sibling children never race for the same UDC.
    if options.usb.enabled && options.usb.vm.as_deref() == Some(vm.name.as_str()) {
        builder.push_flag("usb-gadget", "--usb-gadget", true);
        builder.push("usb-udc", "--usb-udc", options.usb.udc.clone());
        builder.push(
            "usb-configfs-root",
            "--usb-configfs-root",
            options
                .usb
                .configfs_root
                .as_ref()
                .map(|path| path.display().to_string()),
        );
        builder.push(
            "usb-media",
            "--usb-media",
            options
                .usb
                .media
                .as_ref()
                .map(|path| path.display().to_string()),
        );
        builder.push_flag("usb-media-ro", "--usb-media-ro", options.usb.media_ro);
        builder.push_flag(
            "usb-media-cdrom",
            "--usb-media-cdrom",
            options.usb.media_cdrom,
        );
        builder.push_flag("usb-uac2", "--usb-uac2", options.usb.uac2);
        builder.push_flag(
            "usb-uac2-bridge",
            "--usb-uac2-bridge",
            options.usb.uac2_bridge,
        );
        builder.push(
            "usb-uac2-sink-device",
            "--usb-uac2-sink-device",
            options.usb.uac2_sink_device.clone(),
        );
        builder.push(
            "usb-uac2-source-device",
            "--usb-uac2-source-device",
            options.usb.uac2_source_device.clone(),
        );
        builder.push_flag("usb-vendor", "--usb-vendor", options.usb.vendor);
        builder.push(
            "usb-vendor-daemon",
            "--usb-vendor-daemon",
            options
                .usb
                .vendor_daemon
                .as_ref()
                .map(|path| path.display().to_string()),
        );
    }

    // Read-only multiseat report. The store is shared host state and the node
    // deliberately reports rather than mutates seats, so exactly one child
    // owns it.
    if options.multiseat.enabled && options.multiseat.vm.as_deref() == Some(vm.name.as_str()) {
        builder.push_flag("multiseat", "--multiseat", true);
        builder.push(
            "seats-file",
            "--seats-file",
            options
                .multiseat
                .seats_file
                .as_ref()
                .map(|path| path.display().to_string()),
        );
    }

    // Node-local mesh service. One mesh identity per node; validate() requires
    // --mesh-vm so siblings never bind the same UDP port or advertise the same
    // key. The key file's contents are never read here.
    if options.mesh.enabled && options.mesh.vm.as_deref() == Some(vm.name.as_str()) {
        builder.push_flag("mesh", "--mesh", true);
        builder.push("mesh-ip", "--mesh-ip", options.mesh.ip.clone());
        builder.push(
            "mesh-port",
            "--mesh-port",
            options.mesh.port.map(|port| port.to_string()),
        );
        builder.push(
            "mesh-key-file",
            "--mesh-key-file",
            options
                .mesh
                .key_file
                .as_ref()
                .map(|path| path.display().to_string()),
        );
        builder.push_flag("mesh-kernel", "--mesh-kernel", options.mesh.kernel);
        // `--mesh-peer` is intentionally repeatable, like `--display-output`.
        for peer in &options.mesh.peers {
            builder.push("mesh-peer", "--mesh-peer", Some(peer.clone()));
        }
    }

    builder.finish()
}

/// Convenience wrapper used by callers that do not opt into extra features.
///
/// It preserves the pre-parity argv exactly and assumes all flags are
/// supported, so existing deployments keep their current behaviour.
#[allow(dead_code)] // compatibility/test wrapper; the manager uses the options form
pub fn build_node_spec(
    vm: &VMInfo,
    ports: NodePorts,
    node_bin: &Path,
    register_url: &str,
    audio_sink: Option<&str>,
) -> NodeSpec {
    build_node_spec_with_options(
        vm,
        ports,
        node_bin,
        register_url,
        audio_sink,
        &ChildOptions::default(),
        &NodeFlagSupport::Unknown,
        UnsupportedPolicy::Warn,
    )
    .expect("default child options always build")
}

/// Build a [`NodeSpec`] with the operator's opt-in child options.
#[allow(clippy::too_many_arguments)]
pub fn build_node_spec_with_options(
    vm: &VMInfo,
    ports: NodePorts,
    node_bin: &Path,
    register_url: &str,
    audio_sink: Option<&str>,
    options: &ChildOptions,
    support: &NodeFlagSupport,
    policy: UnsupportedPolicy,
) -> Result<NodeSpec, ChildArgvError> {
    let built = build_child_argv(
        vm,
        ports,
        register_url,
        audio_sink,
        options,
        support,
        policy,
    )?;
    let audio_sink = audio_sink
        .map(str::trim)
        .filter(|sink| !sink.is_empty())
        .map(str::to_owned);
    Ok(NodeSpec {
        vm_name: vm.name.clone(),
        program: node_bin.to_path_buf(),
        args: built.args,
        hid_udp_port: ports.hid_udp,
        http_port: ports.http,
        qmp_socket: vm.qmp_path.clone(),
        audio_sink,
        warnings: built.warnings,
    })
}

/// `LABEL:WxH[:PRIMARY][:SOURCE]` or
/// `INDEX:LABEL:WxH[:PRIMARY][:SOURCE]`, matching `ozma-node`'s
/// `--display-output` contract closely enough to catch typos before spawn.
fn valid_display_output(spec: &str) -> bool {
    let parts: Vec<&str> = spec.split(':').collect();
    if parts.len() < 2 || parts[0].trim().is_empty() {
        return false;
    }
    parts.iter().any(|part| {
        part.split_once('x').is_some_and(|(width, height)| {
            let height = height.trim_end_matches(|character: char| {
                matches!(character, 'p' | 'r' | 'i' | 'm' | 'a' | 'y')
            });
            width
                .trim()
                .parse::<u32>()
                .ok()
                .zip(height.trim().parse::<u32>().ok())
                .is_some_and(|(width, height)| width > 0 && height > 0)
        })
    })
}

/// Build a safe per-VM file component. VM names come from the hypervisor, not
/// the operator, so path separators and whitespace are replaced rather than
/// trusted.
fn per_vm_path(dir: &Path, vm: &str, suffix: &str) -> PathBuf {
    let mut component: String = vm
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.') {
                character
            } else {
                '_'
            }
        })
        .collect();
    if component.is_empty() || component == "." || component == ".." {
        component = "_".to_owned();
    }
    dir.join(format!("{component}{suffix}"))
}

/// Derive `--display-output` specs from discovery when it populated consoles.
fn discovered_display_outputs(vm: &VMInfo) -> Vec<String> {
    vm.displays
        .iter()
        .map(|display| {
            let label = if display.capture_source_id.trim().is_empty() {
                format!("Console_{}", display.index)
            } else {
                display.capture_source_id.replace(':', "_")
            };
            let width = display.resolution.0.max(1);
            let height = display.resolution.1.max(1);
            let mut spec = format!("{}:{}:{}x{}", display.index, label, width, height);
            match display.source_type.as_str() {
                "qmp" => spec.push_str(":qmp"),
                "dbus" => spec.push_str(":dbus"),
                "vnc" => spec.push_str(":vnc"),
                _ => {}
            }
            spec
        })
        .collect()
}

/// Deterministic per-VM port allocator.
///
/// Ports are held for as long as the VM is tracked, including while its child
/// is restarting, and released when discovery reports the VM gone. Allocation
/// scans a shared used-set so overlapping HID/HTTP base ranges still produce
/// distinct ports.
pub struct PortAllocator {
    hid_base: u16,
    http_base: u16,
    next_offset: u32,
    assigned: HashMap<String, NodePorts>,
    used: HashSet<u16>,
}

impl PortAllocator {
    pub fn new(hid_base: u16, http_base: u16) -> Self {
        Self {
            hid_base,
            http_base,
            next_offset: 0,
            assigned: HashMap::new(),
            used: HashSet::new(),
        }
    }

    /// Allocate or return the existing port pair for `vm_name`.
    pub fn allocate(&mut self, vm_name: &str) -> Result<NodePorts> {
        if let Some(ports) = self.assigned.get(vm_name) {
            return Ok(*ports);
        }

        loop {
            let hid = self.hid_base as u32 + self.next_offset;
            let http = self.http_base as u32 + self.next_offset;
            self.next_offset += 1;

            if hid > u16::MAX as u32 || http > u16::MAX as u32 {
                anyhow::bail!(
                    "no free per-VM port pair left (hid base {}, http base {})",
                    self.hid_base,
                    self.http_base
                );
            }

            let hid = hid as u16;
            let http = http as u16;
            if hid == http || self.used.contains(&hid) || self.used.contains(&http) {
                continue;
            }

            let ports = NodePorts { hid_udp: hid, http };
            self.used.insert(hid);
            self.used.insert(http);
            self.assigned.insert(vm_name.to_owned(), ports);
            return Ok(ports);
        }
    }

    /// Release a removed VM's ports.
    pub fn release(&mut self, vm_name: &str) {
        if let Some(ports) = self.assigned.remove(vm_name) {
            self.used.remove(&ports.hid_udp);
            self.used.remove(&ports.http);
        }
    }

    pub fn clear(&mut self) {
        self.assigned.clear();
        self.used.clear();
        self.next_offset = 0;
    }

    /// The port pair the next new VM would receive, when one is still free.
    pub fn next_ports(&self) -> Option<NodePorts> {
        let hid = u32::from(self.hid_base).checked_add(self.next_offset)?;
        let http = u32::from(self.http_base).checked_add(self.next_offset)?;
        if hid > u32::from(u16::MAX) || http > u32::from(u16::MAX) {
            return None;
        }
        Some(NodePorts {
            hid_udp: hid as u16,
            http: http as u16,
        })
    }
}

/// Bounded exponential restart delay: `initial * 2^attempt`, capped at `max`.
pub(crate) fn restart_delay(initial: Duration, max: Duration, attempt: u32) -> Duration {
    if initial.is_zero() || max.is_zero() {
        return Duration::ZERO;
    }
    let initial_ms = initial.as_millis().max(1);
    let max_ms = max.as_millis().max(initial_ms);
    let factor = 1u128 << attempt.min(31);
    let delay_ms = initial_ms.saturating_mul(factor).min(max_ms);
    Duration::from_millis(u64::try_from(delay_ms).unwrap_or(u64::MAX))
}

/// Exit information for a supervised child.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NodeExitStatus {
    pub success: bool,
    pub code: Option<i32>,
}

impl std::fmt::Display for NodeExitStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "exit code {code}"),
            None => formatter.write_str(if self.success { "success" } else { "signal" }),
        }
    }
}

/// Opaque handle to one spawned child, owned by the supervisor per VM.
pub struct NodeHandle {
    vm_name: String,
    pid: Option<u32>,
    process: Box<dyn NodeProcess>,
}

impl NodeHandle {
    fn new(vm_name: String, pid: Option<u32>, process: Box<dyn NodeProcess>) -> Self {
        Self {
            vm_name,
            pid,
            process,
        }
    }

    pub fn vm_name(&self) -> &str {
        &self.vm_name
    }

    pub fn pid(&self) -> Option<u32> {
        self.pid
    }

    /// Non-blocking child check.
    pub fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>> {
        self.process.try_wait()
    }

    async fn terminate(&mut self) -> Result<()> {
        self.process.terminate().await
    }
}

/// Test seam over `tokio::process::Child` and fake children.
#[async_trait]
trait NodeProcess: Send {
    fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>>;
    async fn terminate(&mut self) -> Result<()>;
}

/// Spawn/stop seam. Tests inject a fake implementation; production spawns
/// real `ozma-node` children.
#[async_trait]
pub trait NodeLauncher: Send + Sync {
    async fn spawn(&self, spec: &NodeSpec) -> Result<NodeHandle>;

    async fn stop(&self, mut handle: NodeHandle) -> Result<()> {
        handle.terminate().await
    }
}

/// Production launcher backed by `tokio::process::Command`.
#[derive(Default)]
pub struct ProcessNodeLauncher;

#[async_trait]
impl NodeLauncher for ProcessNodeLauncher {
    async fn spawn(&self, spec: &NodeSpec) -> Result<NodeHandle> {
        let mut command = tokio::process::Command::new(&spec.program);
        command
            .args(&spec.args)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::inherit())
            .stderr(std::process::Stdio::inherit())
            .kill_on_drop(true);

        let child = command.spawn().with_context(|| {
            format!(
                "spawning {} for VM {}",
                spec.program.display(),
                spec.vm_name
            )
        })?;
        let pid = child.id();
        Ok(NodeHandle::new(
            spec.vm_name.clone(),
            pid,
            Box::new(ProcessChild(child)),
        ))
    }
}

struct ProcessChild(tokio::process::Child);

#[async_trait]
impl NodeProcess for ProcessChild {
    fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>> {
        self.0.try_wait().map(|status| {
            status.map(|status| NodeExitStatus {
                success: status.success(),
                code: status.code(),
            })
        })
    }

    async fn terminate(&mut self) -> Result<()> {
        // Already exited: `wait` below still reaps and returns the cached
        // status on tokio's Child.
        if self.0.try_wait().ok().flatten().is_some() {
            return Ok(());
        }

        #[cfg(unix)]
        {
            if let Some(pid) = self.0.id() {
                // SIGTERM first so the node can deregister and close sockets.
                // A dead pid is harmless.
                unsafe {
                    libc::kill(pid as i32, libc::SIGTERM);
                }
            }
        }
        #[cfg(not(unix))]
        {
            let _ = self.0.start_kill();
        }

        match tokio::time::timeout(TERMINATE_GRACE, self.0.wait()).await {
            Ok(result) => {
                result.context("waiting for ozma-node child")?;
                Ok(())
            }
            Err(_) => {
                warn!(
                    "ozma-node child did not exit within {:?}; killing it",
                    TERMINATE_GRACE
                );
                let _ = self.0.start_kill();
                let _ = self.0.wait().await;
                Ok(())
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod testing {
    //! In-memory launcher and child used by unit tests. It never executes a
    //! process and lets tests simulate both spawn failures and child exits.

    use std::collections::{HashMap, HashSet};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;

    use super::*;

    #[derive(Default)]
    pub struct FakeNodeLauncher {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        next_id: u64,
        spawns: Vec<NodeSpec>,
        stops: Vec<String>,
        running: HashMap<u64, bool>,
        latest: HashMap<String, u64>,
        failing: HashSet<String>,
    }

    impl FakeNodeLauncher {
        pub fn new() -> Self {
            Self::default()
        }

        pub fn spawns(&self) -> Vec<NodeSpec> {
            self.state.lock().unwrap().spawns.clone()
        }

        pub fn stopped(&self) -> Vec<String> {
            self.state.lock().unwrap().stops.clone()
        }

        /// Make the next (and all subsequent) spawn for `vm_name` fail.
        pub fn fail_spawn(&self, vm_name: &str) {
            self.state
                .lock()
                .unwrap()
                .failing
                .insert(vm_name.to_owned());
        }

        /// Simulate the latest child for `vm_name` exiting.
        pub fn exit(&self, vm_name: &str) {
            let mut state = self.state.lock().unwrap();
            if let Some(id) = state.latest.get(vm_name).copied() {
                state.running.insert(id, false);
            }
        }

        pub fn active(&self, vm_name: &str) -> bool {
            let state = self.state.lock().unwrap();
            state
                .latest
                .get(vm_name)
                .and_then(|id| state.running.get(id))
                .copied()
                .unwrap_or(false)
        }
    }

    #[async_trait]
    impl NodeLauncher for FakeNodeLauncher {
        async fn spawn(&self, spec: &NodeSpec) -> Result<NodeHandle> {
            let mut state = self.state.lock().unwrap();
            if state.failing.contains(&spec.vm_name) {
                anyhow::bail!("injected spawn failure for {}", spec.vm_name);
            }
            state.next_id += 1;
            let id = state.next_id;
            state.spawns.push(spec.clone());
            state.running.insert(id, true);
            state.latest.insert(spec.vm_name.clone(), id);
            drop(state);

            Ok(NodeHandle::new(
                spec.vm_name.clone(),
                Some(id as u32),
                Box::new(FakeProcess {
                    id,
                    state: Arc::clone(&self.state),
                }),
            ))
        }

        async fn stop(&self, mut handle: NodeHandle) -> Result<()> {
            self.state
                .lock()
                .unwrap()
                .stops
                .push(handle.vm_name().to_owned());
            handle.terminate().await
        }
    }

    struct FakeProcess {
        id: u64,
        state: Arc<Mutex<FakeState>>,
    }

    #[async_trait]
    impl NodeProcess for FakeProcess {
        fn try_wait(&mut self) -> std::io::Result<Option<NodeExitStatus>> {
            let running = self
                .state
                .lock()
                .unwrap()
                .running
                .get(&self.id)
                .copied()
                .unwrap_or(false);
            Ok(if running {
                None
            } else {
                Some(NodeExitStatus {
                    success: false,
                    code: Some(1),
                })
            })
        }

        async fn terminate(&mut self) -> Result<()> {
            self.state.lock().unwrap().running.insert(self.id, false);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vm_info::VMInfo;

    fn vm(name: &str, qmp: &str) -> VMInfo {
        let mut vm = VMInfo::new(name);
        vm.qmp_path = qmp.to_owned();
        vm
    }

    #[test]
    fn port_allocator_is_deterministic_and_distinct() {
        let mut allocator = PortAllocator::new(7400, 7500);
        let a = allocator.allocate("vm-a").unwrap();
        let b = allocator.allocate("vm-b").unwrap();
        let a_again = allocator.allocate("vm-a").unwrap();

        assert_eq!(
            a,
            NodePorts {
                hid_udp: 7400,
                http: 7500
            }
        );
        assert_eq!(
            b,
            NodePorts {
                hid_udp: 7401,
                http: 7501
            }
        );
        assert_eq!(a, a_again);
        assert_ne!(a.hid_udp, b.hid_udp);
        assert_ne!(a.http, b.http);
    }

    #[test]
    fn port_allocator_handles_overlapping_ranges() {
        let mut allocator = PortAllocator::new(7400, 7401);
        let a = allocator.allocate("vm-a").unwrap();
        let b = allocator.allocate("vm-b").unwrap();

        // Offset 0 would give a HTTP=7401 and b HID=7401, so b advances.
        assert_eq!(
            a,
            NodePorts {
                hid_udp: 7400,
                http: 7401
            }
        );
        assert_eq!(
            b,
            NodePorts {
                hid_udp: 7402,
                http: 7403
            }
        );
    }

    #[test]
    fn restart_delay_doubles_and_caps() {
        let initial = Duration::from_millis(100);
        let max = Duration::from_millis(250);

        assert_eq!(restart_delay(initial, max, 0), Duration::from_millis(100));
        assert_eq!(restart_delay(initial, max, 1), Duration::from_millis(200));
        assert_eq!(restart_delay(initial, max, 2), Duration::from_millis(250));
        assert_eq!(restart_delay(initial, max, 10), max);
    }

    #[test]
    fn argv_includes_qmp_input_only_when_the_model_exposes_it() {
        let mut with_input = vm("vm-a", "/run/ozma/a.qmp");
        with_input.qmp_input_path = Some("/run/ozma/a-input.qmp".to_owned());

        let ports = NodePorts {
            hid_udp: 7332,
            http: 7382,
        };
        let spec = build_node_spec(
            &with_input,
            ports,
            Path::new("/usr/bin/ozma-node"),
            "http://10.0.0.1:7380",
            None,
        );

        assert_eq!(
            spec.argv(),
            vec![
                "/usr/bin/ozma-node",
                "--profile",
                "vm",
                "--name",
                "vm-a",
                "--qmp-socket",
                "/run/ozma/a.qmp",
                "--qmp-input-socket",
                "/run/ozma/a-input.qmp",
                "--hid-udp-port",
                "7332",
                "--http-port",
                "7382",
                "--capture-provider",
                "qmp",
                "--input-provider",
                "qmp",
                "--power-provider",
                "qmp",
                "--register-url",
                "http://10.0.0.1:7380",
            ]
        );

        let without_input = vm("vm-b", "/run/ozma/b.qmp");
        let spec = build_node_spec(&without_input, ports, Path::new("ozma-node"), "", None);
        let argv = spec.argv();
        assert!(!argv.iter().any(|arg| arg == "--qmp-input-socket"));
        let input = argv
            .iter()
            .position(|arg| arg == "--input-provider")
            .expect("input provider flag");
        // No dedicated input socket: `ozma-qmp` uses the control socket.
        assert_eq!(argv[input + 1], "qmp");
        assert!(!argv.iter().any(|arg| arg == "--register-url"));
    }

    #[test]
    fn argv_passes_audio_sink_only_when_provisioned() {
        let ports = NodePorts {
            hid_udp: 7332,
            http: 7382,
        };
        let spec = build_node_spec(
            &vm("vm-a", "/run/ozma/a.qmp"),
            ports,
            Path::new("ozma-node"),
            "",
            Some("ozma-vm-a"),
        );
        let argv = spec.argv();
        let sink = argv
            .iter()
            .position(|arg| arg == "--audio-sink")
            .expect("audio sink flag");
        assert_eq!(argv[sink + 1], "ozma-vm-a");
        assert_eq!(spec.audio_sink.as_deref(), Some("ozma-vm-a"));

        let none = build_node_spec(
            &vm("vm-a", "/run/ozma/a.qmp"),
            ports,
            Path::new("ozma-node"),
            "",
            None,
        );
        assert!(!none.argv().iter().any(|arg| arg == "--audio-sink"));
        assert!(none.audio_sink.is_none());
    }

    fn flag_count(argv: &[String], flag: &str) -> usize {
        argv.iter().filter(|arg| arg.as_str() == flag).count()
    }

    fn value_after<'a>(argv: &'a [String], flag: &str) -> Option<&'a str> {
        argv.iter()
            .position(|arg| arg == flag)
            .and_then(|index| argv.get(index + 1))
            .map(String::as_str)
    }

    fn all_option_features() -> ChildOptions {
        ChildOptions {
            hls: HlsChildOptions {
                enabled: true,
                dir_root: Some(PathBuf::from("/srv/hls")),
                fps: Some(15),
                bitrate_kbps: Some(6_000),
                encoder: Some("hevc_vaapi".to_owned()),
                pixel_format: Some("p010".to_owned()),
                ten_bit: true,
                hdr: true,
                color_primaries: Some("bt2020".to_owned()),
                color_transfer: Some("smpte2084".to_owned()),
                color_matrix: Some("bt2020nc".to_owned()),
                multi_console: true,
                multi_console_max: Some(3),
                multi_console_idle_seconds: Some(45),
            },
            display_outputs: HashMap::from([(
                "vm-a".to_owned(),
                vec!["0:Console_0:1920x1080:primary:dbus".to_owned()],
            )]),
            guest_ips: HashMap::from([("vm-a".to_owned(), "10.0.0.42".to_owned())]),
            webrtc: WebRtcChildOptions {
                helper: Some(PathBuf::from("/usr/bin/python3")),
                helper_script: Some(PathBuf::from("/opt/ozma/gst_webrtc_worker.py")),
                socket_dir: Some(PathBuf::from("/run/ozma/webrtc")),
                shm_dir: Some(PathBuf::from("/dev/shm/ozma")),
                width: Some(1280),
                height: Some(720),
                fit_capture: true,
                fps: Some(30),
                bitrate_kbps: Some(4_500),
                audio_from_sink: true,
                audio_backend: Some("pipewire".to_owned()),
                stun: Some("stun:stun.example.org:3478".to_owned()),
                turn: Some("turn:turn.example.org:3478".to_owned()),
                turn_user: Some("turn-user".to_owned()),
                turn_password: Some("turn-secret".to_owned()),
                multi_console: true,
                multi_console_max: Some(2),
                multi_console_idle_seconds: Some(30),
            },
            vnc: VncChildOptions {
                capture: false,
                server: true,
                bind: Some("127.0.0.1".to_owned()),
                server_port: Some(5901),
                vm: Some("vm-a".to_owned()),
            },
            usb: UsbChildOptions {
                enabled: true,
                vm: Some("vm-a".to_owned()),
                udc: Some("dummy_udc.0".to_owned()),
                configfs_root: Some(PathBuf::from("/sys/kernel/config/usb_gadget")),
                media: Some(PathBuf::from("/srv/media/installer.iso")),
                media_ro: true,
                media_cdrom: true,
                uac2: true,
                uac2_bridge: true,
                uac2_sink_device: Some("default".to_owned()),
                uac2_source_device: Some("hw:1,0".to_owned()),
                vendor: true,
                vendor_daemon: Some(PathBuf::from("/usr/libexec/ozma-usb-vendor")),
            },
            gpu: GpuPassthroughChildOptions {
                looking_glass: HashMap::from([(
                    "vm-a".to_owned(),
                    PathBuf::from("/dev/shm/looking-glass"),
                )]),
                looking_glass_fps: Some(60),
                looking_glass_id: Some("lg-vm-a".to_owned()),
                looking_glass_name: Some("VM A display".to_owned()),
            },
            multiseat: MultiseatChildOptions {
                enabled: true,
                seats_file: Some(PathBuf::from("/etc/ozma/seats.json")),
                vm: Some("vm-a".to_owned()),
            },
            mesh: MeshChildOptions {
                enabled: true,
                ip: Some("10.255.0.2".to_owned()),
                port: Some(51820),
                key_file: Some(PathBuf::from("/etc/ozma/mesh.key")),
                kernel: false,
                peers: vec![
                    r#"{"id":"peer-b","public_key":"cGVlci1i","endpoint":"192.0.2.9:51820"}"#
                        .to_owned(),
                ],
                vm: Some("vm-a".to_owned()),
            },
            audio_router: Some("pipewire".to_owned()),
            audio_capture_from_sink: true,
            audio_track_root: Some(PathBuf::from("/srv/tracks")),
        }
    }

    #[test]
    fn argv_parity_table_emits_each_configured_option_exactly_once() {
        let mut vm = vm("vm-a", "/run/ozma/a.qmp");
        vm.qmp_input_path = Some("/run/ozma/a-input.qmp".to_owned());
        vm.has_gpu_passthrough = true;
        vm.vnc_port = 5901;

        let ports = NodePorts {
            hid_udp: 7400,
            http: 7500,
        };
        let argv = build_child_argv(
            &vm,
            ports,
            "http://10.0.0.1:7380",
            Some("ozma-vm-a"),
            &all_option_features(),
            &NodeFlagSupport::Unknown,
            UnsupportedPolicy::Warn,
        )
        .expect("all options build")
        .args;

        // One entry per supported flag group. Repeating any of these would mean
        // the supervisor emitted a duplicate flag.
        for flag in [
            "--profile",
            "--name",
            "--qmp-socket",
            "--qmp-input-socket",
            "--hid-udp-port",
            "--http-port",
            "--capture-provider",
            "--input-provider",
            "--power-provider",
            "--register-url",
            "--audio-sink",
            "--audio-router",
            "--audio-capture-target",
            "--audio-track-root",
            "--hls",
            "--hls-dir",
            "--hls-fps",
            "--hls-bitrate-kbps",
            "--hls-encoder",
            "--hls-pixel-format",
            "--hls-10bit",
            "--hls-hdr",
            "--hls-color-primaries",
            "--hls-color-transfer",
            "--hls-color-matrix",
            "--multi-console-hls",
            "--multi-console-hls-max",
            "--multi-console-hls-idle-seconds",
            "--display-output",
            "--vm-guest-ip",
            "--looking-glass-shm",
            "--looking-glass-fps",
            "--looking-glass-id",
            "--looking-glass-name",
            "--webrtc-helper",
            "--webrtc-helper-script",
            "--webrtc-socket",
            "--webrtc-shm",
            "--webrtc-width",
            "--webrtc-height",
            "--webrtc-fps",
            "--webrtc-bitrate-kbps",
            "--webrtc-audio-source",
            "--webrtc-audio-backend",
            "--webrtc-stun",
            "--webrtc-turn",
            "--webrtc-turn-user",
            "--webrtc-turn-password",
            "--multi-console-webrtc",
            "--multi-console-webrtc-max",
            "--multi-console-webrtc-idle-seconds",
            "--vnc-server",
            "--vnc-bind",
            "--vnc-server-port",
            "--usb-gadget",
            "--usb-udc",
            "--usb-configfs-root",
            "--usb-media",
            "--usb-media-ro",
            "--usb-media-cdrom",
            "--usb-uac2",
            "--usb-uac2-bridge",
            "--usb-uac2-sink-device",
            "--usb-uac2-source-device",
            "--usb-vendor",
            "--usb-vendor-daemon",
            "--multiseat",
            "--seats-file",
            "--mesh",
            "--mesh-ip",
            "--mesh-port",
            "--mesh-key-file",
            "--mesh-peer",
        ] {
            assert_eq!(flag_count(&argv, flag), 1, "{flag} must appear once");
        }

        assert_eq!(
            value_after(&argv, "--capture-provider"),
            Some("looking-glass")
        );
        assert_eq!(value_after(&argv, "--hls-dir"), Some("/srv/hls/vm-a"));
        assert_eq!(value_after(&argv, "--hls-encoder"), Some("hevc_vaapi"));
        assert_eq!(
            value_after(&argv, "--display-output"),
            Some("0:Console_0:1920x1080:primary:dbus")
        );
        assert_eq!(value_after(&argv, "--vm-guest-ip"), Some("10.0.0.42"));
        assert_eq!(
            value_after(&argv, "--audio-capture-target"),
            Some("monitor:ozma-vm-a")
        );
        assert_eq!(
            value_after(&argv, "--webrtc-socket"),
            Some("/run/ozma/webrtc/vm-a.sock")
        );
        assert_eq!(
            value_after(&argv, "--webrtc-shm"),
            Some("/dev/shm/ozma/vm-a.shm")
        );
        assert_eq!(
            value_after(&argv, "--webrtc-audio-source"),
            Some("ozma-vm-a.monitor")
        );
        assert_eq!(
            value_after(&argv, "--looking-glass-shm"),
            Some("/dev/shm/looking-glass")
        );
        assert_eq!(value_after(&argv, "--usb-udc"), Some("dummy_udc.0"));
        assert_eq!(
            value_after(&argv, "--usb-media"),
            Some("/srv/media/installer.iso")
        );
        assert_eq!(value_after(&argv, "--vnc-bind"), Some("127.0.0.1"));
        assert_eq!(value_after(&argv, "--vnc-server-port"), Some("5901"));
        assert_eq!(
            value_after(&argv, "--usb-uac2-sink-device"),
            Some("default")
        );
        assert_eq!(
            value_after(&argv, "--usb-uac2-source-device"),
            Some("hw:1,0")
        );
        assert_eq!(
            value_after(&argv, "--seats-file"),
            Some("/etc/ozma/seats.json")
        );
        assert_eq!(value_after(&argv, "--mesh-ip"), Some("10.255.0.2"));
        assert_eq!(value_after(&argv, "--mesh-port"), Some("51820"));
        assert_eq!(
            value_after(&argv, "--mesh-key-file"),
            Some("/etc/ozma/mesh.key")
        );
        assert!(value_after(&argv, "--mesh-peer")
            .is_some_and(|peer| peer.contains("\"id\":\"peer-b\"")));
    }

    #[test]
    fn argv_omits_every_absent_option() {
        let vm = vm("vm-a", "/run/ozma/a.qmp");
        let argv = build_child_argv(
            &vm,
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            "",
            None,
            &ChildOptions::default(),
            &NodeFlagSupport::Unknown,
            UnsupportedPolicy::Warn,
        )
        .expect("default options build")
        .args;

        for flag in [
            "--hls",
            "--hls-dir",
            "--display-output",
            "--vm-guest-ip",
            "--webrtc-helper",
            "--webrtc-socket",
            "--webrtc-shm",
            "--vnc-host",
            "--vnc-port",
            "--vnc-server",
            "--vnc-bind",
            "--vnc-server-port",
            "--usb-gadget",
            "--usb-uac2-bridge",
            "--usb-uac2-sink-device",
            "--usb-uac2-source-device",
            "--looking-glass-shm",
            "--multiseat",
            "--seats-file",
            "--mesh",
            "--mesh-ip",
            "--mesh-port",
            "--mesh-key-file",
            "--mesh-peer",
            "--audio-router",
            "--audio-capture-target",
            "--audio-track-root",
        ] {
            assert_eq!(flag_count(&argv, flag), 0, "{flag} must be absent");
        }

        // Core flags remain exactly as before the parity work.
        assert_eq!(
            argv,
            vec![
                "--profile",
                "vm",
                "--name",
                "vm-a",
                "--qmp-socket",
                "/run/ozma/a.qmp",
                "--hid-udp-port",
                "7400",
                "--http-port",
                "7500",
                "--capture-provider",
                "qmp",
                "--input-provider",
                "qmp",
                "--power-provider",
                "qmp",
            ]
        );
    }

    #[test]
    fn vnc_capture_selects_vnc_only_for_non_passthrough_vms() {
        let mut vm = vm("vm-a", "/run/ozma/a.qmp");
        vm.vnc_port = 5901;
        vm.vnc_host = "192.0.2.10".to_owned();

        let options = ChildOptions {
            vnc: VncChildOptions {
                capture: true,
                ..VncChildOptions::default()
            },
            ..ChildOptions::default()
        };
        let argv = build_child_argv(
            &vm,
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            "",
            None,
            &options,
            &NodeFlagSupport::Unknown,
            UnsupportedPolicy::Warn,
        )
        .expect("vnc options build")
        .args;

        assert_eq!(value_after(&argv, "--capture-provider"), Some("vnc"));
        assert_eq!(value_after(&argv, "--vnc-host"), Some("192.0.2.10"));
        assert_eq!(value_after(&argv, "--vnc-port"), Some("5901"));

        let mut passthrough = vm.clone();
        passthrough.has_gpu_passthrough = true;
        let built = build_child_argv(
            &passthrough,
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            "",
            None,
            &options,
            &NodeFlagSupport::Unknown,
            UnsupportedPolicy::Warn,
        )
        .expect("gpu passthrough options build");
        assert_eq!(value_after(&built.args, "--capture-provider"), Some("qmp"));
        assert_eq!(flag_count(&built.args, "--vnc-port"), 0);
        assert!(built
            .warnings
            .iter()
            .any(|warning| warning.option == "gpu-passthrough"));
    }

    #[test]
    fn per_vm_options_only_target_the_named_vm() {
        let options = ChildOptions {
            display_outputs: HashMap::from([(
                "vm-a".to_owned(),
                vec![
                    "0:Console_0:1920x1080:primary:dbus".to_owned(),
                    "1:Console_1:1280x720:vnc".to_owned(),
                ],
            )]),
            guest_ips: HashMap::from([("vm-b".to_owned(), "10.0.0.43".to_owned())]),
            usb: UsbChildOptions {
                enabled: true,
                vm: Some("vm-a".to_owned()),
                udc: Some("dummy_udc.0".to_owned()),
                ..UsbChildOptions::default()
            },
            vnc: VncChildOptions {
                server: true,
                bind: Some("127.0.0.1".to_owned()),
                vm: Some("vm-a".to_owned()),
                ..VncChildOptions::default()
            },
            multiseat: MultiseatChildOptions {
                enabled: true,
                seats_file: Some(PathBuf::from("/etc/ozma/seats.json")),
                vm: Some("vm-a".to_owned()),
            },
            mesh: MeshChildOptions {
                enabled: true,
                ip: Some("10.255.0.2".to_owned()),
                port: Some(51820),
                key_file: Some(PathBuf::from("/etc/ozma/mesh.key")),
                vm: Some("vm-a".to_owned()),
                ..MeshChildOptions::default()
            },
            ..ChildOptions::default()
        };

        let ports = NodePorts {
            hid_udp: 7400,
            http: 7500,
        };
        let build = |name: &str| {
            build_child_argv(
                &vm(name, "/run/ozma/a.qmp"),
                ports,
                "",
                None,
                &options,
                &NodeFlagSupport::Unknown,
                UnsupportedPolicy::Warn,
            )
            .expect("per-VM options build")
            .args
        };

        let vm_a = build("vm-a");
        assert_eq!(flag_count(&vm_a, "--display-output"), 2);
        assert_eq!(flag_count(&vm_a, "--usb-gadget"), 1);
        assert_eq!(flag_count(&vm_a, "--vm-guest-ip"), 0);
        assert_eq!(flag_count(&vm_a, "--vnc-server"), 1);
        assert_eq!(flag_count(&vm_a, "--multiseat"), 1);
        assert_eq!(flag_count(&vm_a, "--mesh"), 1);

        let vm_b = build("vm-b");
        assert_eq!(flag_count(&vm_b, "--display-output"), 0);
        assert_eq!(flag_count(&vm_b, "--usb-gadget"), 0);
        assert_eq!(flag_count(&vm_b, "--vnc-server"), 0);
        assert_eq!(flag_count(&vm_b, "--multiseat"), 0);
        assert_eq!(flag_count(&vm_b, "--seats-file"), 0);
        assert_eq!(flag_count(&vm_b, "--mesh"), 0);
        assert_eq!(flag_count(&vm_b, "--mesh-peer"), 0);
        assert_eq!(value_after(&vm_b, "--vm-guest-ip"), Some("10.0.0.43"));
    }

    #[test]
    fn flag_support_parses_help_and_unknown_assumes_all() {
        let support = NodeFlagSupport::from_help(
            "Usage: ozma-node [OPTIONS]\n\
             \x20 --hls\n\
             \x20 --hls-dir <PATH>\n\
             \x20 --display-output <LABEL:WxH[:PRIMARY][:SOURCE]>\n\
             \x20 --webrtc-socket <PATH>\n",
        );
        assert!(support.supports("--hls"));
        assert!(support.supports("--hls-dir"));
        assert!(support.supports("--display-output"));
        assert!(!support.supports("--usb-gadget"));
        assert_eq!(support.len(), 4);

        let unknown = NodeFlagSupport::Unknown;
        assert!(unknown.supports("--anything"));
    }

    #[cfg(unix)]
    #[test]
    fn probe_reads_long_flags_from_a_target_binary() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let script = dir.path().join("fake-ozma-node");
        std::fs::write(
            &script,
            "#!/bin/sh\necho 'Usage: fake' >&1\necho '  --hls' >&1\necho '  --webrtc-helper <PATH>' >&1\n",
        )
        .expect("write fake help");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");

        let support = probe_node_flags(&script).expect("probe fake help");
        assert!(support.supports("--hls"));
        assert!(support.supports("--webrtc-helper"));
        assert!(!support.supports("--usb-gadget"));
    }

    #[test]
    fn unsupported_option_is_a_typed_error_when_rejecting() {
        let options = ChildOptions {
            hls: HlsChildOptions {
                enabled: true,
                ..HlsChildOptions::default()
            },
            ..ChildOptions::default()
        };
        let support = NodeFlagSupport::from_help(
            "--profile --name --qmp-socket --hid-udp-port --http-port \
             --capture-provider --input-provider --power-provider",
        );

        let error = build_child_argv(
            &vm("vm-a", "/run/ozma/a.qmp"),
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            "",
            None,
            &options,
            &support,
            UnsupportedPolicy::Reject,
        )
        .expect_err("unsupported --hls is rejected");
        assert_eq!(
            error,
            ChildArgvError::UnsupportedFlag {
                option: "hls",
                flag: "--hls".to_owned(),
            }
        );

        let warned = build_child_argv(
            &vm("vm-a", "/run/ozma/a.qmp"),
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            "",
            None,
            &options,
            &support,
            UnsupportedPolicy::Warn,
        )
        .expect("warning policy still builds");
        assert_eq!(flag_count(&warned.args, "--hls"), 0);
        assert!(warned
            .warnings
            .iter()
            .any(|warning| warning.flag == "--hls"));
    }

    #[test]
    fn new_server_and_mesh_flags_follow_warn_and_reject_policy() {
        let options = ChildOptions {
            vnc: VncChildOptions {
                server: true,
                bind: Some("127.0.0.1".to_owned()),
                ..VncChildOptions::default()
            },
            mesh: MeshChildOptions {
                enabled: true,
                ip: Some("10.255.0.2".to_owned()),
                port: Some(51820),
                vm: Some("vm-a".to_owned()),
                ..MeshChildOptions::default()
            },
            ..ChildOptions::default()
        };
        let support = NodeFlagSupport::from_help(
            "--profile --name --qmp-socket --hid-udp-port --http-port \
             --capture-provider --input-provider --power-provider",
        );

        let error = build_child_argv(
            &vm("vm-a", "/run/ozma/a.qmp"),
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            "",
            None,
            &options,
            &support,
            UnsupportedPolicy::Reject,
        )
        .expect_err("unsupported --vnc-server is rejected");
        assert_eq!(
            error,
            ChildArgvError::UnsupportedFlag {
                option: "vnc-server",
                flag: "--vnc-server".to_owned(),
            }
        );

        let warned = build_child_argv(
            &vm("vm-a", "/run/ozma/a.qmp"),
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            "",
            None,
            &options,
            &support,
            UnsupportedPolicy::Warn,
        )
        .expect("warning policy still builds");
        assert_eq!(flag_count(&warned.args, "--vnc-server"), 0);
        assert_eq!(flag_count(&warned.args, "--mesh"), 0);
        assert!(warned
            .warnings
            .iter()
            .any(|warning| warning.flag == "--vnc-server"));
        assert!(warned
            .warnings
            .iter()
            .any(|warning| warning.flag == "--mesh"));
    }

    #[test]
    fn new_flag_combinations_are_typed_errors() {
        let cases = [
            (
                ChildOptions {
                    vnc: VncChildOptions {
                        bind: Some("0.0.0.0".to_owned()),
                        ..VncChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "vnc-bind",
            ),
            (
                ChildOptions {
                    vnc: VncChildOptions {
                        server: true,
                        server_port: Some(5901),
                        ..VncChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "vnc-server-port",
            ),
            (
                ChildOptions {
                    usb: UsbChildOptions {
                        enabled: true,
                        vm: Some("vm-a".to_owned()),
                        udc: Some("dummy_udc.0".to_owned()),
                        uac2_bridge: true,
                        ..UsbChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "usb-uac2-bridge",
            ),
            (
                ChildOptions {
                    usb: UsbChildOptions {
                        enabled: true,
                        vm: Some("vm-a".to_owned()),
                        udc: Some("dummy_udc.0".to_owned()),
                        uac2: true,
                        uac2_sink_device: Some("default".to_owned()),
                        ..UsbChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "usb-uac2-sink-device",
            ),
            (
                ChildOptions {
                    multiseat: MultiseatChildOptions {
                        enabled: true,
                        seats_file: Some(PathBuf::from("/etc/ozma/seats.json")),
                        ..MultiseatChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "multiseat-vm",
            ),
            (
                ChildOptions {
                    multiseat: MultiseatChildOptions {
                        enabled: true,
                        vm: Some("vm-a".to_owned()),
                        ..MultiseatChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "seats-file",
            ),
            (
                ChildOptions {
                    multiseat: MultiseatChildOptions {
                        seats_file: Some(PathBuf::from("/etc/ozma/seats.json")),
                        ..MultiseatChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "seats-file",
            ),
            (
                ChildOptions {
                    mesh: MeshChildOptions {
                        ip: Some("10.255.0.2".to_owned()),
                        ..MeshChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "mesh",
            ),
            (
                ChildOptions {
                    mesh: MeshChildOptions {
                        enabled: true,
                        ..MeshChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "mesh-vm",
            ),
            (
                ChildOptions {
                    mesh: MeshChildOptions {
                        enabled: true,
                        port: Some(0),
                        vm: Some("vm-a".to_owned()),
                        ..MeshChildOptions::default()
                    },
                    ..ChildOptions::default()
                },
                "mesh-port",
            ),
        ];

        for (options, option) in cases {
            match options.validate() {
                Err(ChildArgvError::InvalidOption { option: actual, .. }) => {
                    assert_eq!(actual, option)
                }
                other => panic!("expected typed --{option} error, got {other:?}"),
            }
        }
    }

    #[test]
    fn invalid_option_combinations_are_typed_errors() {
        let usb_without_vm = ChildOptions {
            usb: UsbChildOptions {
                enabled: true,
                udc: Some("dummy_udc.0".to_owned()),
                ..UsbChildOptions::default()
            },
            ..ChildOptions::default()
        };
        assert_eq!(
            usb_without_vm.validate().unwrap_err(),
            ChildArgvError::InvalidOption {
                option: "usb-vm",
                detail: "USB gadget forwarding requires --usb-vm <VM> so only one \
                         child binds the UDC"
                    .to_owned(),
            }
        );

        let webrtc_without_helper = ChildOptions {
            webrtc: WebRtcChildOptions {
                fps: Some(30),
                ..WebRtcChildOptions::default()
            },
            ..ChildOptions::default()
        };
        assert!(matches!(
            webrtc_without_helper.validate(),
            Err(ChildArgvError::InvalidOption {
                option: "webrtc-helper",
                ..
            })
        ));
    }

    #[test]
    fn redacted_argv_hides_turn_credentials_and_supports_flag_value_form() {
        let mut vm = vm("vm-a", "/run/ozma/a.qmp");
        vm.has_gpu_passthrough = false;
        let options = ChildOptions {
            webrtc: WebRtcChildOptions {
                helper: Some(PathBuf::from("/usr/bin/python3")),
                turn_user: Some("alice".to_owned()),
                turn_password: Some("hunter2".to_owned()),
                ..WebRtcChildOptions::default()
            },
            ..ChildOptions::default()
        };
        let spec = build_node_spec_with_options(
            &vm,
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            Path::new("ozma-node"),
            "",
            None,
            &options,
            &NodeFlagSupport::Unknown,
            UnsupportedPolicy::Warn,
        )
        .expect("webrtc options build");

        let raw = spec.argv();
        assert!(raw.iter().any(|arg| arg == "alice"));
        assert!(raw.iter().any(|arg| arg == "hunter2"));

        let redacted = spec.redacted_argv();
        assert!(!redacted.iter().any(|arg| arg == "alice"));
        assert!(!redacted.iter().any(|arg| arg == "hunter2"));
        assert_eq!(
            redacted
                .iter()
                .filter(|arg| arg.as_str() == "<redacted>")
                .count(),
            2
        );

        let inline = redact_argv(&[
            "--webrtc-turn-password=hunter2".to_owned(),
            "--name".to_owned(),
            "vm-a".to_owned(),
        ]);
        assert_eq!(inline[0], "--webrtc-turn-password=<redacted>");
        assert_eq!(inline[2], "vm-a");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn process_launcher_spawns_exactly_the_built_argv() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempfile::tempdir().expect("temp dir");
        let script = dir.path().join("fake-ozma-node");
        std::fs::write(
            &script,
            "#!/bin/sh\n: > \"$0.argv\"\nfor arg in \"$@\"; do printf '%s\\n' \"$arg\" >> \"$0.argv\"; done\n",
        )
        .expect("write fake child");
        let mut permissions = std::fs::metadata(&script).expect("metadata").permissions();
        permissions.set_mode(0o755);
        std::fs::set_permissions(&script, permissions).expect("chmod");

        let mut vm = vm("vm-a", "/run/ozma/a.qmp");
        vm.qmp_input_path = Some("/run/ozma/a-input.qmp".to_owned());
        let options = ChildOptions {
            hls: HlsChildOptions {
                enabled: true,
                dir_root: Some(dir.path().join("hls")),
                fps: Some(15),
                ..HlsChildOptions::default()
            },
            display_outputs: HashMap::from([(
                "vm-a".to_owned(),
                vec!["0:Console_0:1920x1080".to_owned()],
            )]),
            guest_ips: HashMap::from([("vm-a".to_owned(), "10.0.0.42".to_owned())]),
            vnc: VncChildOptions {
                server: true,
                bind: Some("127.0.0.1".to_owned()),
                server_port: Some(5901),
                vm: Some("vm-a".to_owned()),
                ..VncChildOptions::default()
            },
            mesh: MeshChildOptions {
                enabled: true,
                ip: Some("10.255.0.2".to_owned()),
                port: Some(51820),
                vm: Some("vm-a".to_owned()),
                ..MeshChildOptions::default()
            },
            ..ChildOptions::default()
        };
        let spec = build_node_spec_with_options(
            &vm,
            NodePorts {
                hid_udp: 7400,
                http: 7500,
            },
            &script,
            "http://10.0.0.1:7380",
            Some("ozma-vm-a"),
            &options,
            &NodeFlagSupport::Unknown,
            UnsupportedPolicy::Warn,
        )
        .expect("spec builds");

        let launcher = ProcessNodeLauncher;
        let handle = launcher.spawn(&spec).await.expect("spawn fake child");

        let output_path = PathBuf::from(format!("{}.argv", script.display()));
        let mut content = None;
        for _ in 0..200 {
            if let Ok(text) = std::fs::read_to_string(&output_path) {
                if !text.is_empty() {
                    content = Some(text);
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        let content = content.expect("fake child wrote its argv");
        let actual: Vec<String> = content.lines().map(str::to_owned).collect();
        assert_eq!(actual, spec.args);

        launcher.stop(handle).await.expect("stop fake child");
    }
}
