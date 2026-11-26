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

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use clap::Parser;
use ozma_audio::{
    AudioRouter, PipeWireAudioRouter, PulseAudioAudioRouter, RoomProfileStore, RouterConfig,
    TokioCommandRunner,
};
use ozma_input::SinkKind;
use ozma_power::{LibvirtPower, PowerBackend, QmpPower, UnavailablePower};
use ozma_protocol::NodeId;
use ozma_qmp::SharedQmpClient;
use ozma_thermal::ThermalStatus;
use ozma_usb::{
    CommandFunctionFsMounter, ConfigFsUsbController, FunctionFsConfig, FunctionFsDaemonSpec,
    FunctionFsHandoff, LinuxUsbIpHostBackend, LinuxUsbIpKernelDataEndpoint, MassStorageImage,
    MediaFormat, MediaRestartPolicy, NoUsbIpKernelDataEndpoint, SystemCommandRunner, UsbError,
    UsbFunction, UsbGadgetConfig, UsbGadgetController, UsbIpDaemon, UsbIpDaemonConfig,
    UsbIpKernelDataEndpoint, UsbIpKernelSysfs, UsbIpServer, UsbIpServerConfig,
};
use tracing::{error, info, warn};

use ozma_node::{
    api, audio_runtime,
    config::{
        AudioRouterKind, DesktopCaptureSelection, DesktopCaptureTarget, NodeConfig, NodeProfile,
        PortalCursorKind, PortalSourceKind, PowerProvider, ProviderOverrides, ProviderSelection,
        VmProviderConfig, DEFAULT_FAKE_FPS, DEFAULT_FAKE_HEIGHT, DEFAULT_FAKE_WIDTH,
        DEFAULT_QMP_COMMAND_TIMEOUT_MS, DEFAULT_QMP_CONNECT_TIMEOUT_MS, DEFAULT_USB_CONFIGFS_ROOT,
        DEFAULT_USB_GADGET_NAME, DEFAULT_USB_MASS_STORAGE_INSTANCE,
    },
    controller_compat::{self, AudioControl, CodecControl},
    display_broker::{ConsoleBroker, ConsoleBrokerConfig},
    display_compat::SnapshotControl,
    display_control::{
        ConfigConsoleFactory, ConsoleSourceFactory, DisplayControl, HlsPipelineSwitch,
        SwitchablePipeline, WebRtcPipelineSwitch,
    },
    hls::{
        HlsAudioConfig, HlsConfig, HlsController, HlsEncoder, HlsError, HlsPixelFormat,
        HlsSourceFactory, HlsStatus,
    },
    input_select,
    keyboard_layout::KeyboardLayout,
    mdns, mesh_service,
    multi_hls::{MultiConsoleHls, MultiConsoleHlsConfig},
    multi_webrtc::{MultiConsoleWebrtc, MultiConsoleWebrtcConfig},
    platform, registration, serial, shutdown,
    software::report::{InventoryReporter, DEFAULT_REPORT_INTERVAL},
    usb_audio, usb_uac2,
    webrtc::{
        probe_settled_geometry, select_helper, FitProbe, HelperSelection, WebRtcAudioBackend,
        WebRtcBackend, WebRtcConfig, WebRtcControl, WebRtcGeometryStatus, WebRtcHelper,
        MAX_FIT_PROBE, MAX_FIT_SETTLE_MS,
    },
};

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

    /// WebTransport (HTTP/3) listener port for the browser client.
    /// 0 disables the browser listener.
    #[arg(long, env = "OZMA_WEBTRANSPORT_PORT", default_value_t = 0)]
    pub webtransport_port: u16,

    /// Bind address for the WebTransport listener. The default is loopback;
    /// use `0.0.0.0` to accept browsers on the LAN.
    #[arg(long, env = "OZMA_WEBTRANSPORT_BIND", default_value = "127.0.0.1")]
    pub webtransport_bind: String,

    /// Comma-separated capability list advertised in mDNS TXT and registration.
    /// Example: `hid,video,audio`
    #[arg(long, default_value = "hid,video,audio")]
    pub cap: String,

    /// Node profile: `hardware`, `vm`, or `desktop`.
    ///
    /// The vm profile wires QMP capture/input/power when the corresponding
    /// sockets/domain are configured. The desktop profile selects PipeWire
    /// capture by default and supports X11 (`--capture-provider x11
    /// --x11-display`) and the XDG Portal ScreenCast handshake
    /// (`--capture-provider portal`); a denied portal or missing X display is a
    /// warned no-capture state, never a fake source. Local HLS is opt-in via
    /// `--hls`; WebRTC runs only through a configured helper (external
    /// `--webrtc-helper` or the opt-in native `--webrtc-native`). The per-VM
    /// supervisor remains a later phase.
    #[arg(long, env = "OZMA_PROFILE", value_enum, default_value_t = NodeProfile::Hardware)]
    pub profile: NodeProfile,

    /// Override the profile's video capture provider.
    ///
    /// Desktop: `pipewire` (default), `x11`, `portal`, or `fake` (dev/test).
    /// VM: `qmp`, `dbus`, `vnc`, `looking-glass`, or `fake`.
    #[arg(long, default_value = "")]
    pub capture_provider: String,

    /// Override the profile's input provider (e.g. `hid-gadget`, `uinput`).
    #[arg(long, default_value = "")]
    pub input_provider: String,

    /// Guest keyboard layout used by `POST /input/type` (`us`, `uk`, `de`, `fr`).
    ///
    /// This must match the guest's console keymap; it is not detected. Dead
    /// keys and IME/compose are not supported and are rejected before typing.
    #[arg(
        long,
        env = "OZMA_KEYBOARD_LAYOUT",
        value_enum,
        default_value_t = KeyboardLayout::Us
    )]
    pub keyboard_layout: KeyboardLayout,

    /// Override the profile's audio provider (e.g. `pipewire`, `alsa`).
    #[arg(long, default_value = "")]
    pub audio_provider: String,

    /// Override the profile's virtual display provider (e.g. `fake`, `evdi`).
    #[arg(long, default_value = "")]
    pub display_provider: String,

    /// Override the profile's power provider (e.g. `none`, `qmp`, `libvirt`).
    #[arg(long, default_value = "")]
    pub power_provider: String,

    /// QEMU control QMP Unix socket (vm capture and power).
    #[arg(long, env = "OZMA_QMP_SOCKET", value_name = "PATH")]
    pub qmp_socket: Option<std::path::PathBuf>,

    /// Dedicated QEMU QMP Unix socket for input injection.
    #[arg(long, env = "OZMA_QMP_INPUT_SOCKET", value_name = "PATH")]
    pub qmp_input_socket: Option<std::path::PathBuf>,

    /// libvirt domain name used for VM power control.
    #[arg(long, env = "OZMA_LIBVIRT_DOMAIN")]
    pub libvirt_domain: Option<String>,

    /// libvirt connection URI passed to `virsh --connect` (for example
    /// `qemu:///system`). Omit to use virsh's own default connection.
    #[arg(long, env = "OZMA_LIBVIRT_URI", value_name = "URI")]
    pub libvirt_uri: Option<String>,

    /// QMP connect/greeting timeout in milliseconds (must be non-zero).
    #[arg(
        long,
        env = "OZMA_QMP_CONNECT_TIMEOUT_MS",
        default_value_t = DEFAULT_QMP_CONNECT_TIMEOUT_MS
    )]
    pub qmp_connect_timeout_ms: u64,

    /// QMP control-command timeout in milliseconds (must be non-zero).
    #[arg(
        long,
        env = "OZMA_QMP_COMMAND_TIMEOUT_MS",
        default_value_t = DEFAULT_QMP_COMMAND_TIMEOUT_MS
    )]
    pub qmp_command_timeout_ms: u64,

    /// QEMU VNC host for VM display capture. Setting this (or `--vnc-port`)
    /// selects the ffmpeg-backed VNC provider when no capture provider is
    /// named explicitly.
    #[arg(long, env = "OZMA_VNC_HOST")]
    pub vnc_host: Option<String>,

    /// QEMU VNC port for VM display capture (must be non-zero).
    #[arg(long, env = "OZMA_VNC_PORT")]
    pub vnc_port: Option<u16>,

    /// Scaled VNC output width in pixels (must be non-zero).
    #[arg(long, env = "OZMA_VNC_WIDTH", default_value_t = ozma_capture::VNC_DEFAULT_WIDTH)]
    pub vnc_width: u32,

    /// Scaled VNC output height in pixels (must be non-zero).
    #[arg(long, env = "OZMA_VNC_HEIGHT", default_value_t = ozma_capture::VNC_DEFAULT_HEIGHT)]
    pub vnc_height: u32,

    /// VNC output frame rate (must be non-zero).
    #[arg(long, env = "OZMA_VNC_FPS", default_value_t = ozma_capture::VNC_DEFAULT_FPS)]
    pub vnc_fps: u32,

    /// Serve this node's first capture source as an RFB 3.8/VNC endpoint.
    ///
    /// Security type None is used on loopback unless `--vnc-password-file`
    /// enables VNC authentication. The transport is plain TCP: a password is
    /// not encryption and there is no TLS. A non-loopback bind requires either
    /// a password or `--vnc-allow-unauthenticated`. Enabling the server on a
    /// node with no capture source is a warned no-listener state, not a fake
    /// endpoint.
    #[arg(long, env = "OZMA_VNC_SERVER")]
    pub vnc_server: bool,

    /// Bind address for `--vnc-server`. Supplying this flag is the explicit
    /// opt-in that permits a non-loopback address (`0.0.0.0`, a LAN IP, ...).
    /// A non-loopback address also requires `--vnc-password-file` (preferred)
    /// or `--vnc-allow-unauthenticated`.
    #[arg(long, env = "OZMA_VNC_BIND")]
    pub vnc_bind: Option<String>,

    /// TCP port for `--vnc-server` (0 asks the OS for a free port).
    #[arg(long, env = "OZMA_VNC_SERVER_PORT", default_value_t = 0)]
    pub vnc_server_port: u16,

    /// File containing the RFB VNC authentication password (read once at
    /// startup). Enables security type 2, not encryption. The file must be
    /// 0600 or stricter and contain 1-8 bytes after one trailing newline.
    #[arg(long, env = "OZMA_VNC_PASSWORD_FILE", value_name = "PATH")]
    pub vnc_password_file: Option<PathBuf>,

    /// Permit a non-loopback `--vnc-server` bind without a password. Without
    /// this flag, a non-loopback bind requires `--vnc-password-file`. The
    /// transport remains plaintext either way.
    #[arg(long, env = "OZMA_VNC_ALLOW_UNAUTHENTICATED")]
    pub vnc_allow_unauthenticated: bool,

    /// Maximum concurrent RFB viewers (1-32).
    #[arg(
        long,
        env = "OZMA_VNC_MAX_CLIENTS",
        default_value_t = ozma_node::vnc_server::DEFAULT_MAX_CLIENTS
    )]
    pub vnc_max_clients: usize,

    /// Close an RFB viewer that sends no client message for this many seconds
    /// (0 disables the timeout; maximum 86400).
    #[arg(long, env = "OZMA_VNC_IDLE_TIMEOUT_SECONDS", default_value_t = 0)]
    pub vnc_idle_timeout_seconds: u64,

    /// Bridge the RFB viewer clipboard to this node's desktop clipboard.
    ///
    /// Off by default. Only meaningful with `--vnc-server`. The node must run
    /// `--profile desktop` with an X11 display and `xclip` or `xsel`; otherwise
    /// cut text is ignored and status reports the clipboard as unavailable.
    #[arg(long, env = "OZMA_VNC_CLIPBOARD")]
    pub vnc_clipboard: bool,

    /// Maximum clipboard text bytes accepted from a viewer or sent to one.
    #[arg(
        long,
        env = "OZMA_VNC_CLIPBOARD_MAX_BYTES",
        default_value_t = ozma_node::vnc_server::DEFAULT_VNC_CLIPBOARD_BYTES
    )]
    pub vnc_clipboard_max_bytes: usize,

    /// QEMU D-Bus console index (`Console_N`). Supplying any D-Bus display
    /// flag selects D-Bus capture on a VM node; the effective default is 0.
    #[arg(long, env = "OZMA_DBUS_CONSOLE", value_name = "INDEX")]
    pub dbus_console: Option<u32>,

    /// Largest D-Bus scanout width accepted (effective default 1920). Supplying
    /// this flag selects D-Bus capture on a VM node.
    #[arg(long, env = "OZMA_DBUS_WIDTH", value_name = "PIXELS")]
    pub dbus_width: Option<u32>,

    /// Largest D-Bus scanout height accepted (effective default 1080). Supplying
    /// this flag selects D-Bus capture on a VM node.
    #[arg(long, env = "OZMA_DBUS_HEIGHT", value_name = "PIXELS")]
    pub dbus_height: Option<u32>,

    /// D-Bus frame-rate ceiling (effective default 30). Supplying this flag
    /// selects D-Bus capture on a VM node.
    #[arg(long, env = "OZMA_DBUS_FPS", value_name = "FPS")]
    pub dbus_fps: Option<u32>,

    /// Console index (`Console_N`) the one live capture source targets.
    ///
    /// D-Bus honours any index; QMP, VNC, and Looking Glass capture one default
    /// head, so a different index is reported as a limitation instead of being
    /// silently ignored. Defaults to `--dbus-console` for D-Bus and 0 otherwise.
    #[arg(long, env = "OZMA_CAPTURE_CONSOLE", value_name = "INDEX")]
    pub capture_console: Option<u32>,

    /// Looking Glass KVMFR shared-memory path (`/dev/shm/looking-glass`).
    ///
    /// Supplying this selects Looking Glass capture on a VM/GPU-passthrough
    /// node when no capture provider is named explicitly. Discovery reads only
    /// the KVMFR header; a missing or foreign region advertises no source.
    #[arg(long, env = "OZMA_LOOKING_GLASS_SHM", value_name = "PATH")]
    pub looking_glass_shm: Option<std::path::PathBuf>,

    /// Looking Glass polling rate in frames per second (must be non-zero).
    #[arg(
        long,
        env = "OZMA_LOOKING_GLASS_FPS",
        default_value_t = ozma_capture::LOOKING_GLASS_DEFAULT_FPS,
        value_name = "FPS"
    )]
    pub looking_glass_fps: u32,

    /// Looking Glass capture source id advertised to the controller.
    #[arg(
        long,
        env = "OZMA_LOOKING_GLASS_ID",
        default_value = ozma_capture::LOOKING_GLASS_DEFAULT_ID
    )]
    pub looking_glass_id: String,

    /// Looking Glass capture source name advertised to the controller.
    #[arg(
        long,
        env = "OZMA_LOOKING_GLASS_NAME",
        default_value = ozma_capture::LOOKING_GLASS_DEFAULT_NAME
    )]
    pub looking_glass_name: String,

    /// DEV/TEST ONLY: synthetic capture width for `--capture-provider fake`.
    ///
    /// The fake source is a deterministic RGB24 generator for validating the
    /// HLS/hardware-encode path on a host with no real capture device. It is
    /// rejected for the hardware profile, is never a profile default, and the
    /// node logs a warning when it is used.
    #[arg(long, env = "OZMA_FAKE_WIDTH", value_name = "PIXELS")]
    pub fake_width: Option<u32>,

    /// DEV/TEST ONLY: synthetic capture height for `--capture-provider fake`.
    #[arg(long, env = "OZMA_FAKE_HEIGHT", value_name = "PIXELS")]
    pub fake_height: Option<u32>,

    /// DEV/TEST ONLY: synthetic capture frame rate for `--capture-provider fake`.
    #[arg(long, env = "OZMA_FAKE_FPS", value_name = "FPS")]
    pub fake_fps: Option<u32>,

    /// Desktop PipeWire capture target: `default`, `name:<node.name>`,
    /// `object:<node.name>`, or `node:<id>`.
    ///
    /// Use this for an existing `Video/Source` node. For `--capture-provider
    /// portal`, the handshake selects the node and this flag is not used.
    #[arg(
        long,
        env = "OZMA_DESKTOP_CAPTURE_TARGET",
        default_value = "default",
        value_name = "TARGET"
    )]
    pub desktop_capture_target: String,

    /// Explicit X11 display for `--capture-provider x11` (`:0`, `:0.0`, or
    /// `host:10.0`). Ambient `DISPLAY` is never inherited.
    #[arg(long, env = "OZMA_X11_DISPLAY", value_name = "DISPLAY")]
    pub x11_display: Option<String>,

    /// X11 capture region offset, `X,Y` (default `0,0`).
    #[arg(long, env = "OZMA_X11_OFFSET", value_name = "X,Y")]
    pub x11_offset: Option<String>,

    /// X11 capture output width in pixels (default 1920).
    #[arg(long, env = "OZMA_X11_WIDTH", value_name = "PIXELS")]
    pub x11_width: Option<u32>,

    /// X11 capture output height in pixels (default 1080).
    #[arg(long, env = "OZMA_X11_HEIGHT", value_name = "PIXELS")]
    pub x11_height: Option<u32>,

    /// X11 capture frame rate (default 30, maximum 240).
    #[arg(long, env = "OZMA_X11_FPS", value_name = "FPS")]
    pub x11_fps: Option<u32>,

    /// Portal ScreenCast source type: `monitor`, `window`, or `virtual`.
    #[arg(long, env = "OZMA_PORTAL_SOURCE_TYPE", value_enum, value_name = "TYPE")]
    pub portal_source_type: Option<PortalSourceKind>,

    /// Portal cursor mode: `hidden`, `embedded`, or `metadata`.
    #[arg(long, env = "OZMA_PORTAL_CURSOR", value_enum, value_name = "MODE")]
    pub portal_cursor: Option<PortalCursorKind>,

    /// Restore token from a previous portal session.
    #[arg(long, env = "OZMA_PORTAL_RESTORE_TOKEN", value_name = "TOKEN")]
    pub portal_restore_token: Option<String>,

    /// Bound on the portal consent/selection handshake in milliseconds
    /// (default 180000).
    #[arg(long, env = "OZMA_PORTAL_TIMEOUT_MS", value_name = "MS")]
    pub portal_timeout_ms: Option<u64>,

    /// Optional PipeWire remote instance name for desktop capture.
    #[arg(long, env = "OZMA_DESKTOP_CAPTURE_REMOTE", value_name = "NAME")]
    pub desktop_capture_remote: Option<String>,

    /// Requested desktop capture width in pixels (must be non-zero).
    #[arg(
        long,
        env = "OZMA_DESKTOP_CAPTURE_WIDTH",
        default_value_t = 1920,
        value_name = "PIXELS"
    )]
    pub desktop_capture_width: u32,

    /// Requested desktop capture height in pixels (must be non-zero).
    #[arg(
        long,
        env = "OZMA_DESKTOP_CAPTURE_HEIGHT",
        default_value_t = 1080,
        value_name = "PIXELS"
    )]
    pub desktop_capture_height: u32,

    /// Requested desktop capture frame rate (must be non-zero).
    #[arg(
        long,
        env = "OZMA_DESKTOP_CAPTURE_FPS",
        default_value_t = 30,
        value_name = "FPS"
    )]
    pub desktop_capture_fps: u32,

    /// PipeWire audio capture target: `default`, `object:<name>`, `node:<id>`,
    /// or `monitor:<sink>` for a named sink's monitor.
    #[arg(
        long,
        env = "OZMA_AUDIO_CAPTURE_TARGET",
        default_value = "default",
        value_name = "TARGET"
    )]
    pub audio_capture_target: String,

    /// Audio routing backend for `/audio/*`: `none`, `pipewire`, or
    /// `pulseaudio`. It controls an existing audio server; it never starts one.
    #[arg(
        long,
        env = "OZMA_AUDIO_ROUTER",
        value_enum,
        default_value_t = AudioRouterKind::None
    )]
    pub audio_router: AudioRouterKind,

    /// Directory containing reference tracks for `POST /audio/play`.
    #[arg(long, env = "OZMA_AUDIO_TRACK_ROOT", value_name = "PATH")]
    pub audio_track_root: Option<std::path::PathBuf>,

    /// Opt-in directory for the last measured room-correction profile.
    ///
    /// When set (and an audio router is configured), `GET`, `POST`, and
    /// `DELETE /audio/room-correction/profile` read, validate, atomically
    /// store, or clear one bounded JSON profile. Without it the routes answer a
    /// typed 501 and nothing is written.
    #[arg(long, env = "OZMA_AUDIO_PROFILE_DIR", value_name = "PATH")]
    pub audio_profile_dir: Option<std::path::PathBuf>,

    /// Node role advertised to the controller.
    #[arg(long, default_value = "compute")]
    pub role: String,

    /// Machine class: `workstation`, `server`, or `kiosk`.
    #[arg(long, default_value = "workstation")]
    pub machine_class: String,

    /// Capture device path advertised to the controller (e.g. `/dev/video0`).
    #[arg(long, env = "OZMA_CAPTURE_DEVICE")]
    pub capture_device: Option<String>,

    /// Audio sink name advertised to the controller.
    #[arg(long, env = "OZMA_AUDIO_SINK")]
    pub audio_sink: Option<String>,

    /// Guest network IP advertised by VM soft nodes.
    #[arg(long, env = "OZMA_VM_GUEST_IP")]
    pub vm_guest_ip: Option<String>,

    /// VM console head for the controller's display picker (repeatable).
    ///
    /// Form: `<label>:<width>x<height>[:primary][:<source>]` or
    /// `<index>:<label>:<width>x<height>[:primary][:<source>]`. `source` is
    /// `qmp`, `dbus`, `vnc`, or `looking-glass`; when omitted the selected
    /// capture provider is used. The index defaults to the argument position
    /// (`Console_N` numbering). Requires `--profile vm`.
    #[arg(long = "display-output", value_name = "LABEL:WxH[:PRIMARY][:SOURCE]")]
    pub display_output: Vec<String>,

    /// JSON-encoded display output list sent to the controller.
    ///
    /// This is the raw escape hatch for hardware/host outputs. It conflicts
    /// with repeated `--display-output`; VM consoles should use that instead so
    /// they are validated by `VmDisplayTopology`.
    #[arg(long, env = "OZMA_DISPLAY_OUTPUTS_JSON", default_value = "[]")]
    pub display_outputs_json: String,

    /// JSON-encoded PCI passthrough device list sent to the controller.
    #[arg(long, default_value = "[]")]
    pub pci_devices_json: String,

    /// Opt in to creating and binding a composite USB gadget.
    ///
    /// Requires `--usb-udc` and a mounted ConfigFS root. Without this flag the
    /// node performs no ConfigFS writes and advertises no USB capability.
    #[arg(long, env = "OZMA_USB_GADGET")]
    pub usb_gadget: bool,

    /// ConfigFS composite-gadget root (default `/sys/kernel/config/usb_gadget`).
    #[arg(long, env = "OZMA_USB_CONFIGFS_ROOT", value_name = "PATH")]
    pub usb_configfs_root: Option<std::path::PathBuf>,

    /// UDC the gadget binds to (for example `dummy_udc.0` or `fe980000.usb`).
    #[arg(long, env = "OZMA_USB_UDC", value_name = "UDC")]
    pub usb_udc: Option<String>,

    /// Existing disk image or ISO exposed as the mass-storage LUN.
    #[arg(long, env = "OZMA_USB_MEDIA", value_name = "PATH")]
    pub usb_media: Option<std::path::PathBuf>,

    /// Expose the mass-storage LUN read-only.
    #[arg(long, env = "OZMA_USB_MEDIA_RO")]
    pub usb_media_ro: bool,

    /// Present the mass-storage image as a read-only CD-ROM.
    #[arg(long, env = "OZMA_USB_MEDIA_CDROM")]
    pub usb_media_cdrom: bool,

    /// Auto-start the userspace SCSI export from an existing image.
    ///
    /// Separate from `--usb-media`, which creates the ConfigFS kernel LUN.
    /// Auto-start builds a `configured` export and never claims `running`
    /// until a real bulk command cycle completes.
    #[arg(long, env = "OZMA_USB_MEDIA_EXPORT", value_name = "PATH")]
    pub usb_media_export: Option<std::path::PathBuf>,

    /// Auto-start export format: `raw`, `fat32`, or `iso9660`.
    #[arg(
        long = "usb-media-export-format",
        env = "OZMA_USB_MEDIA_EXPORT_FORMAT",
        default_value = "raw",
        value_name = "FORMAT"
    )]
    pub usb_media_export_format: String,

    /// Serve the auto-start raw export read-write through the bounded overlay.
    #[arg(
        long = "usb-media-export-read-write",
        env = "OZMA_USB_MEDIA_EXPORT_READ_WRITE"
    )]
    pub usb_media_export_read_write: bool,

    /// Absolute overlay persistence file for the auto-start export.
    #[arg(
        long = "usb-media-export-persist",
        env = "OZMA_USB_MEDIA_EXPORT_PERSIST",
        value_name = "PATH"
    )]
    pub usb_media_export_persist: Option<std::path::PathBuf>,

    /// Do not commit the auto-start overlay on SCSI flush.
    #[arg(
        long = "usb-media-export-no-persist-on-flush",
        env = "OZMA_USB_MEDIA_EXPORT_NO_PERSIST_ON_FLUSH"
    )]
    pub usb_media_export_no_persist_on_flush: bool,

    /// Do not commit the auto-start overlay on stop/shutdown.
    #[arg(
        long = "usb-media-export-no-persist-on-stop",
        env = "OZMA_USB_MEDIA_EXPORT_NO_PERSIST_ON_STOP"
    )]
    pub usb_media_export_no_persist_on_stop: bool,

    /// Load the existing auto-start overlay before serving.
    #[arg(long = "usb-media-export-reload", env = "OZMA_USB_MEDIA_EXPORT_RELOAD")]
    pub usb_media_export_reload: bool,

    /// Per-transfer deadline for the auto-start export.
    #[arg(
        long = "usb-media-export-timeout-ms",
        env = "OZMA_USB_MEDIA_EXPORT_TIMEOUT_MS",
        value_name = "MS"
    )]
    pub usb_media_export_timeout_ms: Option<u64>,

    /// Data-phase byte bound for the auto-start export.
    #[arg(
        long = "usb-media-export-max-transfer-bytes",
        env = "OZMA_USB_MEDIA_EXPORT_MAX_TRANSFER_BYTES",
        value_name = "BYTES"
    )]
    pub usb_media_export_max_transfer_bytes: Option<usize>,

    /// Maximum bounded restarts after a media transport/rebind failure.
    #[arg(
        long = "usb-media-restart-max-attempts",
        env = "OZMA_USB_MEDIA_RESTART_MAX_ATTEMPTS",
        default_value_t = ozma_usb::USB_MEDIA_RESTART_DEFAULT_MAX_ATTEMPTS
    )]
    pub usb_media_restart_max_attempts: u32,

    /// First restart delay for a failed media export.
    #[arg(
        long = "usb-media-restart-backoff-ms",
        env = "OZMA_USB_MEDIA_RESTART_BACKOFF_MS",
        default_value_t = ozma_usb::USB_MEDIA_RESTART_DEFAULT_BACKOFF_MS
    )]
    pub usb_media_restart_backoff_ms: u64,

    /// Restart backoff ceiling for a failed media export.
    #[arg(
        long = "usb-media-restart-max-backoff-ms",
        env = "OZMA_USB_MEDIA_RESTART_MAX_BACKOFF_MS",
        default_value_t = ozma_usb::USB_MEDIA_RESTART_DEFAULT_MAX_BACKOFF_MS
    )]
    pub usb_media_restart_max_backoff_ms: u64,

    /// Rebuild from the base image instead of reloading the persisted overlay.
    #[arg(
        long = "usb-media-restart-no-overlay-reload",
        env = "OZMA_USB_MEDIA_RESTART_NO_OVERLAY_RELOAD"
    )]
    pub usb_media_restart_no_overlay_reload: bool,

    /// Add a UAC2 audio function (descriptors only; no ALSA bridge).
    #[arg(long, env = "OZMA_USB_UAC2")]
    pub usb_uac2: bool,

    /// Add a FunctionFS vendor function (requires a userspace daemon before it
    /// can enumerate).
    #[arg(long, env = "OZMA_USB_VENDOR")]
    pub usb_vendor: bool,

    /// Optional absolute daemon path reported as FunctionFS readiness metadata.
    #[arg(long, env = "OZMA_USB_VENDOR_DAEMON", value_name = "PATH")]
    pub usb_vendor_daemon: Option<std::path::PathBuf>,

    /// Absolute FunctionFS mountpoint the node creates after the `ffs.vendor0`
    /// function exists and before it binds the UDC.
    ///
    /// In the supported auto-start path the node creates the ConfigFS function,
    /// mounts FunctionFS at this path (requires root), starts the daemon, waits
    /// for its `ep0` descriptor writes, then binds. An operator does not need to
    /// pre-mount it. Requires `--usb-gadget`, `--usb-vendor`, and
    /// `--usb-media-export`; the daemon waits for the kernel `ENABLE` event and
    /// services the SCSI loop over `PollingFileBulkTransport`.
    #[arg(long, env = "OZMA_USB_FUNCTIONFS_MOUNT", value_name = "PATH")]
    pub usb_functionfs_mount: Option<std::path::PathBuf>,

    /// Start the bounded USB/IP control/data listener.
    #[arg(long, env = "OZMA_USBIP")]
    pub usbip: bool,

    /// USB/IP listener host.
    #[arg(
        long = "usbip-host",
        env = "OZMA_USBIP_HOST",
        default_value = "127.0.0.1",
        value_name = "HOST"
    )]
    pub usbip_host: String,

    /// USB/IP listener port.
    #[arg(
        long = "usbip-port",
        env = "OZMA_USBIP_PORT",
        default_value_t = ozma_usb::DEFAULT_USBIP_PORT,
        value_name = "PORT"
    )]
    pub usbip_port: u16,

    /// Select the Linux `usbip` host adapter and TCP data adapter.
    ///
    /// Without this flag the USB/IP daemon keeps the typed
    /// `usbip-kernel-required` defaults and never invents an attach or data
    /// session.
    #[arg(long = "usbip-linux-adapters", env = "OZMA_USBIP_LINUX_ADAPTERS")]
    pub usbip_linux_adapters: bool,

    /// Sysfs root inspected by the Linux USB/IP host adapter.
    #[arg(
        long = "usbip-sysfs-root",
        env = "OZMA_USBIP_SYSFS_ROOT",
        value_name = "PATH"
    )]
    pub usbip_sysfs_root: Option<std::path::PathBuf>,

    /// `usbip` executable used by the Linux host adapter.
    #[arg(
        long = "usbip-program",
        env = "OZMA_USBIP_PROGRAM",
        value_name = "PROGRAM"
    )]
    pub usbip_program: Option<String>,

    /// Start the UAC2 isochronous ↔ host-audio bridge.
    ///
    /// Requires `--usb-uac2`, a bound UAC2 gadget whose ALSA card is present,
    /// and usable host audio devices. Without this flag the UAC2 function is
    /// descriptor-only and `/usb` reports a typed `unsupported` bridge.
    #[arg(long, env = "OZMA_USB_UAC2_BRIDGE")]
    pub usb_uac2_bridge: bool,

    /// Host playback device receiving host→device UAC2 PCM.
    #[arg(
        long,
        env = "OZMA_USB_UAC2_SINK_DEVICE",
        default_value = "default",
        value_name = "ALSA"
    )]
    pub usb_uac2_sink_device: String,

    /// Host capture device feeding device→host UAC2 PCM.
    #[arg(
        long,
        env = "OZMA_USB_UAC2_SOURCE_DEVICE",
        default_value = "default",
        value_name = "ALSA"
    )]
    pub usb_uac2_source_device: String,

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

    /// AIDA64 RemoteSensor-compatible listener port (0 disables it).
    ///
    /// Serves `/`, `/sse`, `/api`, `/sensors`, and an Ozma widget panel at
    /// `/panel`. The listener is unauthenticated; enable it only on trusted
    /// networks.
    #[arg(long, env = "OZMA_AIDA64_PORT", default_value_t = 0)]
    pub aida64_port: u16,

    /// Bind address for the AIDA64 RemoteSensor listener.
    #[arg(long, env = "OZMA_AIDA64_BIND", default_value = "0.0.0.0")]
    pub aida64_bind: String,

    /// Thermal policy JSON (fan curves, zones, fan specs, safe limits).
    ///
    /// Without a policy the node exposes read-only temperatures and fan speeds.
    /// Fan control also requires `--thermal-control`.
    #[arg(long, env = "OZMA_THERMAL_CONFIG", value_name = "PATH")]
    pub thermal_config: Option<std::path::PathBuf>,

    /// Allow PWM fan writes. Off by default; control also requires a policy.
    #[arg(long, env = "OZMA_THERMAL_CONTROL")]
    pub thermal_control: bool,

    /// Thermal sensor/control poll interval in seconds.
    #[arg(long, env = "OZMA_THERMAL_INTERVAL", default_value_t = 5)]
    pub thermal_interval: u64,

    /// sysfs hwmon root (default `/sys/class/hwmon`; overridable for tests).
    #[arg(
        long,
        env = "OZMA_THERMAL_HWMON_ROOT",
        default_value = ozma_thermal::DEFAULT_HWMON_ROOT
    )]
    pub thermal_hwmon_root: String,

    /// Replay a trusted local browser scene fixture in scene-capable sessions.
    #[arg(long)]
    pub scene_replay: Option<std::path::PathBuf>,

    /// Require this bearer token for the node's HTTP API.
    #[arg(long, env = "OZMA_API_TOKEN")]
    pub api_token: Option<String>,

    /// Opt in to the userspace WireGuard mesh service (`/api/v1/connect/*`).
    ///
    /// Off by default: without this flag the node binds no mesh socket and the
    /// Connect routes report `state:"disabled"` without peers or tunnels.
    #[arg(long, env = "OZMA_MESH")]
    pub mesh: bool,

    /// IPv4 address assigned to this node on the mesh (required by --mesh).
    #[arg(long, env = "OZMA_MESH_IP")]
    pub mesh_ip: Option<String>,

    /// WireGuard UDP port for mesh traffic.
    #[arg(long, env = "OZMA_MESH_PORT", default_value_t = 51820)]
    pub mesh_port: u16,

    /// Path to a base64-encoded WireGuard private key file (required by --mesh).
    #[arg(long, env = "OZMA_MESH_KEY_FILE", value_name = "PATH")]
    pub mesh_key_file: Option<std::path::PathBuf>,

    /// Use Linux kernel WireGuard instead of userspace boringtun. Requires
    /// root, the `wireguard` kernel module, `ip`, and `wg` (wireguard-tools).
    /// The interface from `--mesh-tun` is created as a kernel `wireguard` link;
    /// no TUN device or userspace packet pump is used.
    #[arg(long, env = "OZMA_MESH_KERNEL")]
    pub mesh_kernel: bool,

    /// Additional mesh peer as a JSON `MeshNode` object. Repeatable.
    #[arg(long = "mesh-peer", env = "OZMA_MESH_PEER", value_name = "JSON")]
    pub mesh_peer: Vec<String>,

    /// Underlay UDP endpoint for a static peer: `ID=ADDRESS:PORT`. Repeatable.
    ///
    /// The peer's mesh IP stays its overlay identity and `/32` route. Set this
    /// in kernel WireGuard mode when the peer's mesh IP is itself routed via
    /// the WireGuard link, so encrypted UDP does not recurse into `wg`.
    #[arg(
        long = "mesh-peer-endpoint",
        env = "OZMA_MESH_PEER_ENDPOINT",
        value_name = "ID=ADDR:PORT"
    )]
    pub mesh_peer_endpoint: Vec<String>,

    /// Opt in to mDNS peer discovery on `_ozma-mesh._udp.local`.
    ///
    /// Discovered peers are only accepted when their node id and WireGuard
    /// public key match a `--mesh-peer` entry or a `--mesh-trust` entry. Static
    /// peers always win conflicts. Requires `--mesh`.
    #[arg(long, env = "OZMA_MESH_DISCOVER", requires = "mesh")]
    pub mesh_discover: bool,

    /// Seconds a discovered peer may be absent before its route and tunnel are
    /// removed (default 60).
    #[arg(
        long,
        env = "OZMA_MESH_DISCOVER_TTL",
        default_value_t = 60,
        value_name = "SECS"
    )]
    pub mesh_discover_ttl: u64,

    /// Maximum number of discovered peers (default 32, hard limit 256).
    #[arg(
        long,
        env = "OZMA_MESH_DISCOVER_MAX_PEERS",
        default_value_t = 32,
        value_name = "N"
    )]
    pub mesh_discover_max_peers: usize,

    /// Authorize a discovered peer: JSON `{"id":...,"wg_pubkey":...}` or
    /// `ID=BASE64_KEY`. Repeatable. Static `--mesh-peer` entries are already
    /// authorized.
    #[arg(
        long = "mesh-trust",
        env = "OZMA_MESH_TRUST",
        value_name = "JSON|ID=KEY"
    )]
    pub mesh_trust: Vec<String>,

    /// Shared Ed25519 cluster signing key file (mode 0600). When set, mesh
    /// advertisements are signed and discovered peers must verify against the
    /// derived public key. The private key is never logged or advertised.
    #[arg(
        long = "mesh-cluster-key-file",
        env = "OZMA_MESH_CLUSTER_KEY_FILE",
        value_name = "PATH",
        requires = "mesh"
    )]
    pub mesh_cluster_key_file: Option<std::path::PathBuf>,

    /// Program a kernel mesh dataplane (TUN link, peer routes, IPv4 forwarding).
    ///
    /// Off by default. Requires root, `/dev/net/tun`, and `ip`; failure is a
    /// typed dataplane error and never a fabricated interface.
    #[arg(long, env = "OZMA_MESH_DATAPLANE")]
    pub mesh_dataplane: bool,

    /// TUN interface name used by `--mesh-dataplane`.
    #[arg(long, env = "OZMA_MESH_TUN", default_value = "ozma0")]
    pub mesh_tun: String,

    /// TUN MTU used by `--mesh-dataplane`.
    #[arg(long, env = "OZMA_MESH_MTU", default_value_t = 1420)]
    pub mesh_mtu: u16,

    /// Mesh prefix length used by `--mesh-dataplane`.
    #[arg(long, env = "OZMA_MESH_PREFIX_LEN", default_value_t = 24)]
    pub mesh_prefix_len: u8,

    /// Opt in to NAT/masquerade for mesh traffic (`nft` must be available).
    #[arg(long, env = "OZMA_MESH_DATAPLANE_NAT")]
    pub mesh_dataplane_nat: bool,

    /// NAT egress interface. Without it, traffic leaving any interface other
    /// than the mesh TUN is masqueraded.
    #[arg(long, env = "OZMA_MESH_NAT_OUT", value_name = "IFACE")]
    pub mesh_nat_out: Option<String>,

    /// Automatic WireGuard key rotation interval in hours (`0` disables it).
    ///
    /// Off by default. A non-zero value must be `1..=720`; each rotation uses
    /// `--mesh-rotate-grace-secs` and adds up to
    /// `--mesh-rotate-jitter-secs` of random jitter.
    #[arg(
        long,
        env = "OZMA_MESH_ROTATE_HOURS",
        requires = "mesh",
        default_value_t = 0
    )]
    pub mesh_rotate_hours: u64,

    /// Random jitter added to each scheduled rotation.
    #[arg(
        long,
        env = "OZMA_MESH_ROTATE_JITTER_SECS",
        requires = "mesh",
        default_value_t = 300
    )]
    pub mesh_rotate_jitter_secs: u64,

    /// Grace window during which the previous key stays usable after rotation.
    #[arg(
        long,
        env = "OZMA_MESH_ROTATE_GRACE_SECS",
        requires = "mesh",
        default_value_t = 90
    )]
    pub mesh_rotate_grace_secs: u64,

    /// Report the persisted multiseat plan/status at `GET /api/v1/seats`.
    ///
    /// Opt-in and read-only. `--multiseat` (alias `--multiseat-plan`) loads the
    /// shared `--seats-file` store once and reports persisted seat identities.
    /// It never starts, stops, or restarts a seat process: lifecycle stays with
    /// the standalone `ozma-multiseat` supervisor. Requires `--seats-file`.
    #[arg(
        long,
        env = "OZMA_MULTISEAT",
        visible_alias = "multiseat-plan",
        requires = "seats_file"
    )]
    pub multiseat: bool,

    /// Persisted seat store loaded by `--multiseat` (for example
    /// `/etc/ozma/seats.json`). Required when `--multiseat` is set.
    #[arg(long, env = "OZMA_SEATS_FILE", value_name = "PATH")]
    pub seats_file: Option<std::path::PathBuf>,

    /// Opt in to the local Restic backup provider (`/backup/*`).
    ///
    /// Must be an absolute local repository path. Remote restic backends
    /// (`s3:`, `sftp:`, `rest:`, …) are rejected. Requires
    /// `--backup-password-file`; there are no default sources.
    #[arg(
        long,
        env = "OZMA_BACKUP_REPO",
        value_name = "PATH",
        requires = "backup_password_file"
    )]
    pub backup_repo: Option<std::path::PathBuf>,

    /// Absolute path to the restic repository password file (mode 0600).
    ///
    /// The contents are passed to restic through `RESTIC_PASSWORD_FILE`; the
    /// password is never placed in argv, logs, or status JSON.
    #[arg(
        long,
        env = "OZMA_BACKUP_PASSWORD_FILE",
        value_name = "PATH",
        requires = "backup_repo"
    )]
    pub backup_password_file: Option<std::path::PathBuf>,

    /// Absolute directory allow-listed as a backup source. Repeatable.
    ///
    /// There are no default sources, and whole-system roots such as `/`, `/etc`,
    /// `/home`, and `/var` are rejected. `POST /backup/run` can only select
    /// paths equal to or below these roots.
    #[arg(
        long = "backup-source",
        env = "OZMA_BACKUP_SOURCE",
        value_name = "PATH",
        requires = "backup_repo"
    )]
    pub backup_source: Vec<std::path::PathBuf>,

    /// Absolute directory allow-listed as a restore target root. Repeatable.
    ///
    /// Restores require a target below one of these roots; symlinks and `..`
    /// escapes are rejected.
    #[arg(
        long = "backup-restore-dir",
        env = "OZMA_BACKUP_RESTORE_DIR",
        value_name = "PATH",
        requires = "backup_repo"
    )]
    pub backup_restore_dir: Vec<std::path::PathBuf>,

    /// restic executable used by the local backup provider (default `restic`).
    #[arg(
        long,
        env = "OZMA_BACKUP_RESTIC",
        value_name = "PATH",
        requires = "backup_repo"
    )]
    pub backup_restic: Option<std::path::PathBuf>,

    /// Opt in to the bounded BlueZ/`bluetoothctl` surface (`/bluetooth/*`).
    ///
    /// The routes stay behind the node bearer guard. Without this flag they
    /// answer a typed 503 `bluetooth.not-configured`. Scanning is bounded to
    /// 15 seconds and pairing never trusts a device.
    #[arg(long, env = "OZMA_BLUETOOTH")]
    pub bluetooth: bool,

    /// Explicit Bluetooth adapter MAC (`--bluetooth` only).
    ///
    /// Omit it to use the BlueZ `[default]` controller or the first adapter.
    #[arg(
        long,
        env = "OZMA_BLUETOOTH_ADAPTER",
        value_name = "MAC",
        requires = "bluetooth"
    )]
    pub bluetooth_adapter: Option<String>,

    /// `bluetoothctl` executable used by the Bluetooth provider (default
    /// `bluetoothctl`).
    #[arg(
        long,
        env = "OZMA_BLUETOOTH_PROGRAM",
        value_name = "PATH",
        requires = "bluetooth"
    )]
    pub bluetooth_program: Option<std::path::PathBuf>,

    /// Opt in to the bounded serial/UART console surface (`/serial/*`).
    ///
    /// Must be an explicit absolute device path (for example `/dev/ttyUSB0` or
    /// `/dev/ttyS1`). There is no auto-detection. Without this flag the
    /// implemented routes answer a typed 503 `serial.not-configured`; the
    /// historical buffer/send/state/text/setup sub-surfaces stay typed 501s.
    #[arg(long, env = "OZMA_SERIAL_DEVICE", value_name = "PATH")]
    pub serial_device: Option<std::path::PathBuf>,

    /// Serial baud rate (`--serial-device` only).
    #[arg(
        long,
        env = "OZMA_SERIAL_BAUD",
        value_name = "BAUD",
        requires = "serial_device",
        default_value_t = crate::serial::DEFAULT_BAUD_RATE
    )]
    pub serial_baud: u32,

    /// Serial data bits: 5, 6, 7, or 8 (`--serial-device` only).
    #[arg(
        long,
        env = "OZMA_SERIAL_DATA_BITS",
        value_name = "BITS",
        requires = "serial_device",
        default_value_t = 8,
        value_parser = clap::value_parser!(u8).range(5..=8)
    )]
    pub serial_data_bits: u8,

    /// Serial parity: `none`, `odd`, or `even` (`--serial-device` only).
    #[arg(
        long,
        env = "OZMA_SERIAL_PARITY",
        value_name = "PARITY",
        requires = "serial_device",
        default_value = "none"
    )]
    pub serial_parity: String,

    /// Serial stop bits: 1 or 2 (`--serial-device` only).
    #[arg(
        long,
        env = "OZMA_SERIAL_STOP_BITS",
        value_name = "BITS",
        requires = "serial_device",
        default_value_t = 1,
        value_parser = clap::value_parser!(u8).range(1..=2)
    )]
    pub serial_stop_bits: u8,

    /// Serial read timeout in milliseconds (`--serial-device` only).
    #[arg(
        long,
        env = "OZMA_SERIAL_TIMEOUT_MS",
        value_name = "MS",
        requires = "serial_device",
        default_value_t = 100
    )]
    pub serial_timeout_ms: u64,

    /// Received-byte buffer retained for `/serial/read` (`--serial-device` only).
    ///
    /// Clamped/validated to 1024..4194304 bytes by the serial provider.
    #[arg(
        long,
        env = "OZMA_SERIAL_BUFFER_BYTES",
        value_name = "BYTES",
        requires = "serial_device",
        default_value_t = crate::serial::DEFAULT_BUFFER_BYTES
    )]
    pub serial_buffer_bytes: usize,

    /// Serial flow control: `none`, `rtscts`, or `xonxoff`
    /// (`--serial-device` only).
    #[arg(
        long,
        env = "OZMA_SERIAL_FLOW_CONTROL",
        value_name = "MODE",
        requires = "serial_device",
        default_value = "none"
    )]
    pub serial_flow_control: String,

    /// Assert DTR when the serial device is opened (`--serial-device` only).
    ///
    /// Omit to leave DTR untouched. `--serial-dtr` asserts it;
    /// `--serial-dtr=false` deasserts it. Applied again on every reconnect.
    #[arg(
        long,
        env = "OZMA_SERIAL_DTR",
        value_name = "BOOL",
        requires = "serial_device",
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    pub serial_dtr: Option<bool>,

    /// Assert RTS when the serial device is opened (`--serial-device` only).
    ///
    /// Omit to leave RTS untouched. `--serial-rts` asserts it;
    /// `--serial-rts=false` deasserts it. Applied again on every reconnect.
    #[arg(
        long,
        env = "OZMA_SERIAL_RTS",
        value_name = "BOOL",
        requires = "serial_device",
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    pub serial_rts: Option<bool>,

    /// Enable `POST /serial/control` to change DTR/RTS (`--serial-device`
    /// only).
    ///
    /// Off by default: without this flag the control route returns a typed
    /// 403 `serial.control-disabled`. The endpoint accepts only booleans for
    /// `dtr` and `rts`.
    #[arg(long, env = "OZMA_SERIAL_CONTROL", requires = "serial_device")]
    pub serial_control: bool,

    /// Reconnect attempts after a serial removal/read error
    /// (`--serial-device` only).
    ///
    /// Zero disables reconnect. The same explicit device path is retried with
    /// exponential backoff; another device is never opened.
    #[arg(
        long,
        env = "OZMA_SERIAL_RECONNECT_ATTEMPTS",
        value_name = "N",
        requires = "serial_device",
        default_value_t = crate::serial::DEFAULT_RECONNECT_ATTEMPTS
    )]
    pub serial_reconnect_attempts: u32,

    /// First serial reconnect delay in milliseconds (`--serial-device` only).
    #[arg(
        long,
        env = "OZMA_SERIAL_RECONNECT_MIN_MS",
        value_name = "MS",
        requires = "serial_device",
        default_value_t = crate::serial::DEFAULT_RECONNECT_MIN_MS
    )]
    pub serial_reconnect_min_ms: u64,

    /// Maximum serial reconnect delay in milliseconds (`--serial-device`
    /// only).
    #[arg(
        long,
        env = "OZMA_SERIAL_RECONNECT_MAX_MS",
        value_name = "MS",
        requires = "serial_device",
        default_value_t = crate::serial::DEFAULT_RECONNECT_MAX_MS
    )]
    pub serial_reconnect_max_ms: u64,

    /// Enable the read-only 1-Wire sysfs surface. With no value, uses the
    /// kernel default root; an explicit path is allowed for staging/tests.
    #[arg(
        long,
        env = "OZMA_ONEWIRE",
        value_name = "ROOT",
        num_args = 0..=1,
        default_missing_value = "true"
    )]
    pub onewire: Option<String>,

    /// Exact I²C `BUS:ADDR` allowlist (repeatable or comma-separated).
    ///
    /// Nothing is probed or opened without an entry. Reserved/broadcast
    /// addresses are always rejected.
    #[arg(
        long,
        env = "OZMA_I2C_ALLOW",
        value_name = "BUS:ADDR",
        value_delimiter = ','
    )]
    pub i2c_allow: Vec<String>,

    /// Exact SPI `/dev/spidevX.Y` allowlist (repeatable or comma-separated).
    #[arg(
        long,
        env = "OZMA_SPI_ALLOW",
        value_name = "/dev/spidevX.Y",
        value_delimiter = ','
    )]
    pub spi_allow: Vec<String>,

    /// Exact GPIO `CHIP:LINE[:input|output]` allowlist (repeatable or
    /// comma-separated). The default direction is input; `set`/`pulse`
    /// require an explicit `output` entry. Nothing is probed without an entry.
    #[arg(
        long,
        env = "OZMA_GPIO_ALLOW",
        value_name = "CHIP:LINE[:input|output]",
        value_delimiter = ','
    )]
    pub gpio_allow: Vec<String>,

    /// Exact CAN interface allowlist (repeatable or comma-separated).
    ///
    /// Capture/status are read-only by default. Add `:send` to one exact
    /// interface (or use --can-send-allow) to opt it into transmission.
    #[arg(
        long,
        env = "OZMA_CAN_ALLOW",
        value_name = "IFACE[:send]",
        value_delimiter = ','
    )]
    pub can_allow: Vec<String>,

    /// Exact CAN interfaces allowed to transmit (repeatable or
    /// comma-separated). Every name must also appear in --can-allow.
    #[arg(
        long,
        env = "OZMA_CAN_SEND_ALLOW",
        value_name = "IFACE",
        value_delimiter = ','
    )]
    pub can_send_allow: Vec<String>,

    /// Exact read-only IIO `DEVICE:CHANNEL` allowlist (repeatable or
    /// comma-separated), e.g. `iio:device0:voltage0`.
    #[arg(
        long,
        env = "OZMA_IIO_ALLOW",
        value_name = "DEVICE:CHANNEL",
        value_delimiter = ','
    )]
    pub iio_allow: Vec<String>,

    /// Optional explicit IIO sysfs root for staging/tests (default
    /// `/sys/bus/iio/devices`).
    #[arg(long, env = "OZMA_IIO_ROOT", value_name = "PATH")]
    pub iio_root: Option<String>,

    /// Exact PWM `CHIP:CHANNEL[:export]` allowlist (repeatable or
    /// comma-separated). `:export` permits creating a missing channel; there is
    /// no unexport operation.
    #[arg(
        long,
        env = "OZMA_PWM_ALLOW",
        value_name = "CHIP:CHANNEL[:export]",
        value_delimiter = ','
    )]
    pub pwm_allow: Vec<String>,

    /// Optional explicit PWM sysfs root for staging/tests (default
    /// `/sys/class/pwm`).
    #[arg(long, env = "OZMA_PWM_ROOT", value_name = "PATH")]
    pub pwm_root: Option<String>,

    /// Exact DDC/CI `BUS:ADDR[:write]` allowlist (repeatable or
    /// comma-separated). Every pair must also be in --i2c-allow; `:write` opts
    /// that exact display into bounded VCP writes.
    #[arg(
        long,
        env = "OZMA_DDC_ALLOW",
        value_name = "BUS:ADDR[:write]",
        value_delimiter = ','
    )]
    pub ddc_allow: Vec<String>,

    /// Opt in to SMBus Packet Error Checking on allowlisted I²C transactions.
    #[arg(long, env = "OZMA_I2C_PEC")]
    pub i2c_pec: bool,

    /// Maximum recent CAN frames buffered per allowlisted interface.
    #[arg(
        long,
        env = "OZMA_CAN_MAX_FRAMES",
        value_name = "COUNT",
        default_value_t = ozma_node::buses::DEFAULT_MAX_CAN_FRAMES
    )]
    pub can_max_frames: usize,

    /// Maximum bytes in one SPI bus transfer.
    #[arg(
        long,
        env = "OZMA_BUS_MAX_TRANSFER_BYTES",
        value_name = "BYTES",
        default_value_t = ozma_node::buses::DEFAULT_MAX_TRANSFER_BYTES
    )]
    pub bus_max_transfer_bytes: usize,

    /// Maximum SMBus block bytes for I²C read/write.
    #[arg(
        long,
        env = "OZMA_BUS_MAX_BLOCK_BYTES",
        value_name = "BYTES",
        default_value_t = ozma_node::buses::DEFAULT_MAX_BLOCK_BYTES
    )]
    pub bus_max_block_bytes: usize,

    /// Maximum allowlisted/reported bus devices.
    #[arg(
        long,
        env = "OZMA_BUS_MAX_DEVICES",
        value_name = "COUNT",
        default_value_t = ozma_node::buses::DEFAULT_MAX_DEVICES
    )]
    pub bus_max_devices: usize,

    /// Opt in to bounded application control for pids listed by `/apps`
    /// (`/apps/{pid}/terminate`, `/signal`, `/suspend`, `/resume`).
    ///
    /// Off by default: without this flag every control route returns a typed
    /// `503 apps.control-disabled` and the node never signals another process.
    /// When enabled, pid 1, this node and its process tree, kernel threads,
    /// zombies, and (by default) processes owned by another user are refused.
    /// `SIGKILL` requires explicit confirmation and every signal is preceded by
    /// an exact pid/start-time re-check.
    #[arg(long, env = "OZMA_APP_CONTROL")]
    pub app_control: bool,

    /// Seconds to wait after `SIGTERM` before the bounded `SIGKILL` escalation
    /// used by `POST /apps/{pid}/terminate` (clamped to 1–30 seconds).
    #[arg(
        long,
        env = "OZMA_APP_CONTROL_KILL_AFTER_SECONDS",
        value_name = "SECONDS",
        default_value_t = 2
    )]
    pub app_control_kill_after_seconds: u64,

    /// Allow application control of processes owned by another user.
    ///
    /// Off by default. This is a deliberate administrative widening; exact pid,
    /// start-time, process-tree, kernel-thread, and confirmation checks still
    /// apply.
    #[arg(
        long,
        env = "OZMA_APP_CONTROL_ALLOW_OTHER_USERS",
        requires = "app_control"
    )]
    pub app_control_allow_other_users: bool,

    /// Enable the local HLS pipeline: capture → ffmpeg → `/stream/*`.
    ///
    /// Requires a video capture source and an ffmpeg binary. WebRTC is not
    /// part of this flag.
    #[arg(long)]
    pub hls: bool,

    /// HLS output directory (default `/tmp/ozma-stream-<name>`).
    #[arg(long, env = "OZMA_HLS_DIR", value_name = "PATH")]
    pub hls_dir: Option<std::path::PathBuf>,

    /// HLS input frame rate (must be greater than zero).
    #[arg(long, env = "OZMA_HLS_FPS", default_value_t = 30)]
    pub hls_fps: u32,

    /// HLS H.264 bitrate in kbps (must be greater than zero).
    #[arg(long, env = "OZMA_HLS_BITRATE_KBPS", default_value_t = 4000)]
    pub hls_bitrate_kbps: u32,

    /// HLS video encoder: libx264, libx265, libsvtav1, libaom-av1, mjpeg, or a
    /// hardware encoder such as h264_vaapi, hevc_vaapi, h264_nvenc, hevc_nvenc,
    /// h264_qsv, hevc_qsv, h264_v4l2m2m, or hevc_v4l2m2m.
    ///
    /// A hardware encoder is only started after `ffmpeg -encoders` confirms it
    /// is available; naming one never silently falls back to software. For
    /// NVENC, `--nvenc-gpu <n>` selects the CUDA ordinal (`-gpu <n>`).
    #[arg(
        long,
        env = "OZMA_HLS_ENCODER",
        default_value = "libx264",
        value_name = "ENCODER"
    )]
    pub hls_encoder: String,

    /// HLS encoded pixel format: `yuv420p` (default SDR 8-bit) or `p010`
    /// (10-bit; HEVC/AV1 Main10 only).
    #[arg(long, env = "OZMA_HLS_PIXEL_FORMAT", value_name = "FORMAT")]
    pub hls_pixel_format: Option<String>,

    /// Select the 10-bit `p010` HLS target (equivalent to
    /// `--hls-pixel-format p010`; HEVC/AV1 Main10 only).
    #[arg(long, env = "OZMA_HLS_10BIT")]
    pub hls_10bit: bool,

    /// Signal HDR color metadata and default it to BT.2020/PQ.
    ///
    /// Requires a 10-bit target. `--hls-color-primaries`,
    /// `--hls-color-transfer`, and `--hls-color-matrix` override the preset
    /// fields individually.
    #[arg(long, env = "OZMA_HLS_HDR")]
    pub hls_hdr: bool,

    /// HDR color primaries (`-color_primaries`, for example `bt2020`).
    #[arg(long, env = "OZMA_HLS_COLOR_PRIMARIES", value_name = "NAME")]
    pub hls_color_primaries: Option<String>,

    /// HDR transfer characteristic (`-color_trc`, for example `smpte2084`).
    #[arg(long, env = "OZMA_HLS_COLOR_TRANSFER", value_name = "NAME")]
    pub hls_color_transfer: Option<String>,

    /// HDR color matrix (`-colorspace`, for example `bt2020nc`).
    #[arg(long, env = "OZMA_HLS_COLOR_MATRIX", value_name = "NAME")]
    pub hls_color_matrix: Option<String>,

    /// Enable opt-in AAC audio in the local HLS stream.
    ///
    /// Without `--hls-audio-source` the monitor is derived from `--audio-sink`
    /// (`<sink>.monitor`) or a `monitor:<sink>` `--audio-capture-target`. When
    /// neither names a sink, startup fails with a typed error instead of
    /// synthesizing silence. A configured monitor that does not exist fails the
    /// generation; the node never claims audio it did not mux.
    #[arg(long, env = "OZMA_HLS_AUDIO")]
    pub hls_audio: bool,

    /// PulseAudio/PipeWire monitor source for HLS audio, for example
    /// `ozma-vm1.monitor` (the `<sink>.monitor` convention). Implies
    /// `--hls-audio`.
    #[arg(long, env = "OZMA_HLS_AUDIO_SOURCE", value_name = "MONITOR")]
    pub hls_audio_source: Option<String>,

    /// AAC bitrate for HLS audio in kbps (bounded to 8–512; default 128).
    #[arg(
        long,
        env = "OZMA_HLS_AUDIO_BITRATE_KBPS",
        default_value_t = 128,
        value_name = "KBPS"
    )]
    pub hls_audio_bitrate_kbps: u32,

    /// Optional per-console HLS audio monitor template.
    ///
    /// `{console}` and `{index}` expand to the advertised console's zero-based
    /// index, for example `ozma-vm{console}.monitor` gives
    /// `ozma-vm0.monitor` and `ozma-vm1.monitor`. Unset keeps the base HLS
    /// monitor for every per-console pipeline (or video-only when that is also
    /// unset). Only applies with `--multi-console-hls`.
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_HLS_AUDIO_SOURCE",
        value_name = "TEMPLATE"
    )]
    pub multi_console_hls_audio_source: Option<String>,

    /// Enable one independent HLS pipeline per advertised VM console.
    ///
    /// Opt-in and VM/D-Bus only: each advertised `Console_N` gets its own
    /// `<hls-dir>/console-N/stream.m3u8` and ffmpeg process, served at
    /// `/stream/console-N/...`. `/stream/...` keeps following the active
    /// console. Single-head providers (QMP/VNC/Looking Glass) start no
    /// per-console pipelines and report a typed limitation. Implies `--hls`
    /// for the active-console path and capability advertisement.
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_HLS",
        visible_alias = "hls-per-console"
    )]
    pub multi_console_hls: bool,

    /// Maximum simultaneous per-console HLS pipelines/encoders (including the
    /// active console). Bounded to at least one; default 2.
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_HLS_MAX",
        default_value_t = 2,
        value_name = "N"
    )]
    pub multi_console_hls_max: usize,

    /// Stop a non-active per-console HLS pipeline after this many seconds
    /// without a `/stream/console-N/...` request (0 disables idle eviction).
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_HLS_IDLE_SECONDS",
        default_value_t = 60,
        value_name = "SECONDS"
    )]
    pub multi_console_hls_idle_seconds: u64,

    /// Enable one independent WebRTC helper per advertised VM console.
    ///
    /// Opt-in and VM/D-Bus only: each advertised `Console_N` gets its own
    /// helper process, signaling socket, and shared-memory framebuffer.
    /// `POST /webrtc/offer?console=N` (or a `console`/`id` body field) selects
    /// one; an offer without a selector targets the active console, preserving
    /// the historical behavior. Single-head providers (QMP/VNC/Looking Glass)
    /// keep the active helper and report a typed `unsupported` limitation.
    /// Requires `--webrtc-helper` or `--webrtc-native`.
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_WEBRTC",
        visible_alias = "webrtc-per-console"
    )]
    pub multi_console_webrtc: bool,

    /// Maximum simultaneous per-console WebRTC helpers (including the active
    /// console). Bounded to at least one; default 2.
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_WEBRTC_MAX",
        default_value_t = 2,
        value_name = "N"
    )]
    pub multi_console_webrtc_max: usize,

    /// Stop a non-active per-console WebRTC helper after this many seconds
    /// without an offer/candidate for that console (0 disables idle eviction).
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_WEBRTC_IDLE_SECONDS",
        default_value_t = 60,
        value_name = "SECONDS"
    )]
    pub multi_console_webrtc_idle_seconds: u64,

    /// Optional per-console WebRTC audio monitor template.
    ///
    /// `{console}` and `{index}` are replaced with the advertised console's
    /// zero-based index, for example `ozma-vm{console}.monitor` gives
    /// `ozma-vm0.monitor` and `ozma-vm1.monitor`. Unset keeps the shared
    /// `--webrtc-audio-source` for every helper (or video-only when that is
    /// unset). Only applies with `--multi-console-webrtc`.
    #[arg(
        long,
        env = "OZMA_MULTI_CONSOLE_WEBRTC_AUDIO_SOURCE",
        value_name = "TEMPLATE"
    )]
    pub multi_console_webrtc_audio_source: Option<String>,

    /// VAAPI render node used by `h264_vaapi`/`hevc_vaapi` HLS encoders.
    ///
    /// The path is validated when a hardware pipeline starts or restarts, not
    /// while building argv.
    #[arg(
        long,
        env = "OZMA_VAAPI_DEVICE",
        default_value = "/dev/dri/renderD128",
        value_name = "PATH"
    )]
    pub vaapi_device: std::path::PathBuf,

    /// NVIDIA CUDA ordinal for NVENC H.264/HEVC (ffmpeg `-gpu <n>`).
    ///
    /// Only emitted for `h264_nvenc`/`hevc_nvenc`; a non-NVENC encoder is
    /// rejected at startup rather than passed an unsupported flag. `None`
    /// leaves device selection to ffmpeg/the driver.
    #[arg(
        long,
        env = "OZMA_NVENC_GPU",
        value_name = "INDEX",
        visible_alias = "cuda-device"
    )]
    pub nvenc_gpu: Option<u32>,

    /// ffmpeg executable used by the HLS pipeline.
    #[arg(
        long,
        env = "OZMA_FFMPEG_BIN",
        default_value = "ffmpeg",
        value_name = "PATH"
    )]
    pub ffmpeg_bin: std::path::PathBuf,

    /// External WebRTC helper executable (for example `/usr/bin/python3`).
    ///
    /// WebRTC stays disabled (`not-configured`) unless this or
    /// `--webrtc-native` is set. `--webrtc-helper ozma-webrtc-native` selects
    /// the native Rust helper explicitly.
    #[arg(long, env = "OZMA_WEBRTC_HELPER", value_name = "PATH")]
    pub webrtc_helper: Option<std::path::PathBuf>,

    /// Use the native Rust `ozma-webrtc-native` helper (video-only).
    ///
    /// Without `--webrtc-helper`, the binary is looked up next to `ozma-node`
    /// and then on `PATH`; it is never substituted for a configured Python
    /// helper. Any `--webrtc-helper-script` is ignored for this backend.
    /// Native audio is not implemented, so `/api/v1/connection` reports
    /// `webrtc_audio_configured=false` even if an audio source is supplied.
    #[arg(long, env = "OZMA_WEBRTC_NATIVE")]
    pub webrtc_native: bool,

    /// Optional first argument passed to `--webrtc-helper` (for example the
    /// Python worker script; when absent the program is spawned directly).
    #[arg(long, env = "OZMA_WEBRTC_HELPER_SCRIPT", value_name = "PATH")]
    pub webrtc_helper_script: Option<std::path::PathBuf>,

    /// WebRTC helper frame width (must be greater than zero).
    #[arg(long, env = "OZMA_WEBRTC_WIDTH", default_value_t = 640)]
    pub webrtc_width: u32,

    /// WebRTC helper frame height (must be greater than zero).
    #[arg(long, env = "OZMA_WEBRTC_HEIGHT", default_value_t = 480)]
    pub webrtc_height: u32,

    /// Fit the WebRTC helper to the capture source's frame size once it is
    /// opened, instead of rejecting a mismatch with `--webrtc-width/height`.
    ///
    /// Providers whose geometry is only known at runtime (QMP, VNC, X11) need
    /// this; explicit dimensions still apply when capture declares no size.
    #[arg(long, env = "OZMA_WEBRTC_FIT_CAPTURE")]
    pub webrtc_fit_capture: bool,

    /// How long the fit probe waits for the capture size to stay unchanged
    /// before accepting it (milliseconds, clamped to 0-5000; default 4000).
    ///
    /// Only used with `--webrtc-fit-capture` when the discovery geometry is
    /// unknown. The window must outlast a guest's early boot text mode
    /// (typically ~3 s) or the probe latches it; the whole probe is still
    /// bounded (about 8 s), so a source that never settles falls back to its
    /// last observed size.
    #[arg(
        long,
        env = "OZMA_WEBRTC_SETTLE_MS",
        default_value_t = 4_000,
        value_name = "MS"
    )]
    pub webrtc_settle_ms: u64,

    /// WebRTC helper frame rate (must be greater than zero).
    #[arg(long, env = "OZMA_WEBRTC_FPS", default_value_t = 30)]
    pub webrtc_fps: u32,

    /// WebRTC H.264 bitrate in kbps (must be greater than zero).
    #[arg(long, env = "OZMA_WEBRTC_BITRATE_KBPS", default_value_t = 4_000)]
    pub webrtc_bitrate_kbps: u32,

    /// WebRTC helper signaling Unix socket (default: temp dir, per process).
    #[arg(long, env = "OZMA_WEBRTC_SOCKET", value_name = "PATH")]
    pub webrtc_socket: Option<std::path::PathBuf>,

    /// WebRTC helper shared-memory frame path (default: `/dev/shm`, per
    /// process).
    #[arg(long, env = "OZMA_WEBRTC_SHM", value_name = "PATH")]
    pub webrtc_shm: Option<std::path::PathBuf>,

    /// Optional PulseAudio/PipeWire monitor source for WebRTC audio, for
    /// example `ozma-vm1.monitor`. Audio stays disabled unless this is set.
    #[arg(long, env = "OZMA_WEBRTC_AUDIO_SOURCE", value_name = "SOURCE")]
    pub webrtc_audio_source: Option<String>,

    /// WebRTC audio capture backend: `auto`, `pulse`, or `pipewire`.
    ///
    /// `auto` prefers `pulsesrc` and falls back to `pipewiresrc`. It only
    /// applies when `--webrtc-audio-source` is set.
    #[arg(
        long,
        env = "OZMA_WEBRTC_AUDIO_BACKEND",
        value_enum,
        default_value_t = WebRtcAudioBackend::Auto
    )]
    pub webrtc_audio_backend: WebRtcAudioBackend,

    /// Optional STUN server for WebRTC ICE, for example
    /// `stun:stun.example.org:3478` or `stun://stun.example.org:3478`.
    /// Omit to keep host-candidate-only gathering.
    #[arg(long, env = "OZMA_WEBRTC_STUN", value_name = "URI")]
    pub webrtc_stun: Option<String>,

    /// Optional TURN server address without credentials, for example
    /// `turn:turn.example.org:3478`. Requires `--webrtc-turn-user` and
    /// `--webrtc-turn-password`.
    #[arg(long, env = "OZMA_WEBRTC_TURN", value_name = "URI")]
    pub webrtc_turn: Option<String>,

    /// TURN username; never logged or returned by the status API.
    #[arg(long, env = "OZMA_WEBRTC_TURN_USER", value_name = "USER")]
    pub webrtc_turn_user: Option<String>,

    /// TURN password; redacted from logs and status.
    #[arg(
        long,
        env = "OZMA_WEBRTC_TURN_PASSWORD",
        value_name = "PASSWORD",
        hide_env_values = true
    )]
    pub webrtc_turn_password: Option<String>,

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

/// Resolve the HLS pixel-format flags into one target.
///
/// `--hls-hdr` implies 10-bit when no explicit format was given, but an
/// explicit `yuv420p` is a contradiction rather than a silent override.
fn resolve_hls_pixel_format(
    explicit: Option<&str>,
    ten_bit: bool,
    hdr: bool,
) -> Result<HlsPixelFormat, String> {
    let parsed = explicit
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(HlsPixelFormat::parse)
        .transpose()
        .map_err(|error| error.to_string())?;
    if ten_bit && parsed == Some(HlsPixelFormat::Yuv420p) {
        return Err("--hls-10bit conflicts with an explicit --hls-pixel-format yuv420p".to_owned());
    }
    let resolved = if ten_bit {
        HlsPixelFormat::P010
    } else if let Some(parsed) = parsed {
        parsed
    } else if hdr {
        HlsPixelFormat::P010
    } else {
        HlsPixelFormat::Yuv420p
    };
    if hdr && !resolved.is_10bit() {
        return Err("--hls-hdr requires a 10-bit target; pass --hls-10bit or \
             --hls-pixel-format p010"
            .to_owned());
    }
    Ok(resolved)
}

/// Resolve the opt-in HLS audio monitor source.
///
/// An explicit `--hls-audio-source` wins. Otherwise `--hls-audio` derives
/// `<sink>.monitor` from `--audio-sink` or a `monitor:<sink>`
/// `--audio-capture-target`. A request with no named sink is a typed startup
/// error: the node must not invent a default source or synthesize silence.
fn resolve_hls_audio_source(
    requested: bool,
    explicit: Option<&str>,
    audio_sink: Option<&str>,
    capture_target: &ozma_audio::AudioTarget,
) -> Result<Option<String>, String> {
    if let Some(source) = explicit {
        let source = source.trim();
        if source.is_empty() {
            return Err("--hls-audio-source must not be empty or whitespace".to_owned());
        }
        return Ok(Some(source.to_owned()));
    }
    if !requested {
        return Ok(None);
    }
    if let Some(sink) = audio_sink.map(str::trim).filter(|sink| !sink.is_empty()) {
        return Ok(Some(monitor_for_sink(sink)));
    }
    if let ozma_audio::AudioTarget::SinkMonitor(sink) = capture_target {
        return Ok(Some(monitor_for_sink(sink)));
    }
    Err(
        "--hls-audio requires --hls-audio-source, a named --audio-sink, or a \
         monitor:<sink> --audio-capture-target"
            .to_owned(),
    )
}

fn monitor_for_sink(sink: &str) -> String {
    let sink = sink.trim();
    if sink.ends_with(".monitor") {
        sink.to_owned()
    } else {
        ozma_audio::AudioTarget::monitor_name(sink)
    }
}

/// The dev/test synthetic capture flags only make sense with the fake provider.
///
/// Keeping this check pure makes the CLI contract testable without starting a
/// node or opening a capture device. It deliberately does not accept the flags
/// on their own: silently switching a hardware-profile node to a synthetic
/// source would hide a missing real capture device.
fn validate_fake_flags(
    capture_provider: &str,
    fake_width: Option<u32>,
    fake_height: Option<u32>,
    fake_fps: Option<u32>,
) -> Result<(), String> {
    let fake = capture_provider.trim().eq_ignore_ascii_case("fake");
    let supplied = [fake_width, fake_height, fake_fps]
        .into_iter()
        .any(|value| value.is_some());
    if supplied && !fake {
        return Err(
            "--fake-width/--fake-height/--fake-fps are dev/test settings and require \
             --capture-provider fake"
                .to_owned(),
        );
    }
    Ok(())
}

/// Parse `--x11-offset X,Y` (a leading `+` is accepted for symmetry with the
/// ffmpeg `x11grab` region syntax).
fn parse_x11_offset(raw: Option<&str>) -> Result<(u32, u32), String> {
    let Some(raw) = raw else {
        return Ok((0, 0));
    };
    let raw = raw.trim();
    let raw = raw.strip_prefix('+').unwrap_or(raw);
    let (x, y) = raw.split_once(',').ok_or_else(|| {
        "--x11-offset must be two comma-separated non-negative integers (`X,Y`)".to_owned()
    })?;
    let x = x
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("--x11-offset X `{x}` is not a non-negative integer"))?;
    let y = y
        .trim()
        .parse::<u32>()
        .map_err(|_| format!("--x11-offset Y `{y}` is not a non-negative integer"))?;
    Ok((x, y))
}

fn main() -> std::process::ExitCode {
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("ozma-node: could not start the tokio runtime: {error}");
            return std::process::ExitCode::FAILURE;
        }
    };
    let code = runtime.block_on(run());
    // Cooperative stops are bounded inside `run`. This final bound covers a
    // task that never yields (a driver busy-loop): the process still exits
    // instead of requiring SIGKILL.
    runtime.shutdown_timeout(shutdown::RUNTIME_SHUTDOWN_TIMEOUT);
    code
}

async fn run() -> std::process::ExitCode {
    let mut args = Args::parse();

    let level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .init();

    // One root token stops every node-owned task. It is cancelled after the
    // first SIGINT/SIGTERM, then each owner gets a bounded chance to release
    // its sockets, capture sources, and children.
    let shutdown = shutdown::token();
    let mut background: Vec<(&'static str, tokio::task::JoinHandle<()>)> = Vec::new();

    let node_id = NodeConfig::node_id_from_name(&args.name);
    let provider_overrides = ProviderOverrides::from_cli(
        &args.capture_provider,
        &args.input_provider,
        &args.audio_provider,
        &args.display_provider,
        &args.power_provider,
    );
    let providers =
        ProviderSelection::for_profile(args.profile).with_overrides(&provider_overrides);
    if let Err(error) = validate_fake_flags(
        &args.capture_provider,
        args.fake_width,
        args.fake_height,
        args.fake_fps,
    ) {
        error!("{error}");
        std::process::exit(2);
    }
    let desktop_capture_target = match args.desktop_capture_target.parse::<DesktopCaptureTarget>() {
        Ok(target) => target,
        Err(error) => {
            error!("Invalid --desktop-capture-target: {error}");
            std::process::exit(2);
        }
    };
    let (x11_x, x11_y) = match parse_x11_offset(args.x11_offset.as_deref()) {
        Ok(offset) => offset,
        Err(error) => {
            error!("Invalid --x11-offset: {error}");
            std::process::exit(2);
        }
    };
    let x11_requested = args.x11_display.is_some()
        || args.x11_offset.is_some()
        || args.x11_width.is_some()
        || args.x11_height.is_some()
        || args.x11_fps.is_some();
    let portal_requested = args.portal_source_type.is_some()
        || args.portal_cursor.is_some()
        || args.portal_restore_token.is_some()
        || args.portal_timeout_ms.is_some();
    let audio_capture_target = match args.audio_capture_target.parse::<ozma_audio::AudioTarget>() {
        Ok(target) => target,
        Err(error) => {
            error!("Invalid --audio-capture-target: {error}");
            std::process::exit(2);
        }
    };
    let mut config = NodeConfig {
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
        profile: args.profile,
        providers,
        provider_overrides: provider_overrides.clone(),
        capture_device: args.capture_device.clone(),
        qmp_socket: args.qmp_socket.clone(),
        qmp_input_socket: args.qmp_input_socket.clone(),
        libvirt_domain: args
            .libvirt_domain
            .as_ref()
            .map(|domain| domain.trim().to_owned()),
        libvirt_uri: args.libvirt_uri.as_ref().map(|uri| uri.trim().to_owned()),
        qmp_connect_timeout: std::time::Duration::from_millis(args.qmp_connect_timeout_ms),
        qmp_command_timeout: std::time::Duration::from_millis(args.qmp_command_timeout_ms),
        vnc_host: args.vnc_host.clone(),
        vnc_port: args.vnc_port,
        vnc_width: args.vnc_width,
        vnc_height: args.vnc_height,
        vnc_fps: args.vnc_fps,
        vnc_server: args.vnc_server,
        vnc_bind: args.vnc_bind.clone(),
        vnc_server_port: args.vnc_server_port,
        vnc_password_file: args.vnc_password_file.clone(),
        vnc_allow_unauthenticated: args.vnc_allow_unauthenticated,
        vnc_max_clients: args.vnc_max_clients,
        vnc_idle_timeout_seconds: args.vnc_idle_timeout_seconds,
        vnc_clipboard: args.vnc_clipboard,
        vnc_clipboard_max_bytes: args.vnc_clipboard_max_bytes,
        dbus_console: args
            .capture_console
            .or(args.dbus_console)
            .unwrap_or(ozma_capture::DBUS_DEFAULT_CONSOLE_INDEX),
        dbus_width: args.dbus_width.unwrap_or(ozma_capture::DBUS_DEFAULT_WIDTH),
        dbus_height: args
            .dbus_height
            .unwrap_or(ozma_capture::DBUS_DEFAULT_HEIGHT),
        dbus_fps: args.dbus_fps.unwrap_or(ozma_capture::DBUS_DEFAULT_FPS),
        dbus_requested: args.dbus_console.is_some()
            || args.dbus_width.is_some()
            || args.dbus_height.is_some()
            || args.dbus_fps.is_some(),
        capture_console: args.capture_console,
        display_output_specs: args.display_output.clone(),
        display_outputs: Vec::new(),
        looking_glass_shm: args.looking_glass_shm.clone(),
        looking_glass_fps: args.looking_glass_fps,
        looking_glass_id: args.looking_glass_id.clone(),
        looking_glass_name: args.looking_glass_name.clone(),
        fake_width: args.fake_width.unwrap_or(DEFAULT_FAKE_WIDTH),
        fake_height: args.fake_height.unwrap_or(DEFAULT_FAKE_HEIGHT),
        fake_fps: args.fake_fps.unwrap_or(DEFAULT_FAKE_FPS),
        desktop_capture_width: args.desktop_capture_width,
        desktop_capture_height: args.desktop_capture_height,
        desktop_capture_fps: args.desktop_capture_fps,
        desktop_capture_target,
        desktop_capture_remote: args.desktop_capture_remote.clone(),
        x11_display: args.x11_display.clone(),
        x11_x,
        x11_y,
        x11_width: args.x11_width.unwrap_or(ozma_capture::X11_DEFAULT_WIDTH),
        x11_height: args.x11_height.unwrap_or(ozma_capture::X11_DEFAULT_HEIGHT),
        x11_fps: args.x11_fps.unwrap_or(ozma_capture::X11_DEFAULT_FPS),
        x11_requested,
        portal_source_type: args.portal_source_type.unwrap_or_default(),
        portal_cursor: args.portal_cursor.unwrap_or_default(),
        portal_restore_token: args.portal_restore_token.clone(),
        portal_timeout: std::time::Duration::from_millis(
            args.portal_timeout_ms.unwrap_or_else(|| {
                ozma_capture::PORTAL_DEFAULT_RESPONSE_TIMEOUT.as_millis() as u64
            }),
        ),
        portal_requested,
        audio_capture_target,
        audio_router: args.audio_router,
        audio_track_root: args.audio_track_root.clone(),
        audio_profile_dir: args.audio_profile_dir.clone(),
        ffmpeg_bin: args.ffmpeg_bin.clone(),
        vaapi_device: args.vaapi_device.clone(),
        nvenc_gpu: args.nvenc_gpu,
        audio_sink: args.audio_sink.clone(),
        webrtc_audio_source: args.webrtc_audio_source.clone(),
        webrtc_audio_backend: args.webrtc_audio_backend,
        multi_console_webrtc_audio_source: args.multi_console_webrtc_audio_source.clone(),
        vm_guest_ip: args.vm_guest_ip.clone(),
        display_outputs_json: args.display_outputs_json.clone(),
        pci_devices_json: args.pci_devices_json.clone(),
        usb_gadget: args.usb_gadget,
        usb_configfs_root: args
            .usb_configfs_root
            .clone()
            .unwrap_or_else(|| std::path::PathBuf::from(DEFAULT_USB_CONFIGFS_ROOT)),
        usb_udc: args.usb_udc.clone(),
        usb_media: args.usb_media.clone(),
        usb_media_ro: args.usb_media_ro,
        usb_media_cdrom: args.usb_media_cdrom,
        usb_media_export: args.usb_media_export.clone(),
        usb_media_export_format: args.usb_media_export_format.clone(),
        usb_media_export_read_write: args.usb_media_export_read_write,
        usb_media_export_persist: args.usb_media_export_persist.clone(),
        usb_media_export_persist_on_flush: !args.usb_media_export_no_persist_on_flush,
        usb_media_export_persist_on_stop: !args.usb_media_export_no_persist_on_stop,
        usb_media_export_reload: args.usb_media_export_reload,
        usb_media_export_timeout_ms: args.usb_media_export_timeout_ms,
        usb_media_export_max_transfer_bytes: args.usb_media_export_max_transfer_bytes,
        usb_media_restart_max_attempts: args.usb_media_restart_max_attempts,
        usb_media_restart_backoff_ms: args.usb_media_restart_backoff_ms,
        usb_media_restart_max_backoff_ms: args.usb_media_restart_max_backoff_ms,
        usb_media_restart_reload_overlay: !args.usb_media_restart_no_overlay_reload,
        usb_uac2: args.usb_uac2,
        usb_vendor: args.usb_vendor,
        usb_vendor_daemon: args.usb_vendor_daemon.clone(),
        usb_functionfs_mount: args.usb_functionfs_mount.clone(),
        usbip: args.usbip,
        usbip_host: args.usbip_host.clone(),
        usbip_port: args.usbip_port,
        usbip_linux_adapters: args.usbip_linux_adapters,
        usbip_sysfs_root: args.usbip_sysfs_root.clone(),
        usbip_program: args.usbip_program.clone(),
        usb_uac2_bridge: args.usb_uac2_bridge,
        usb_uac2_sink_device: args.usb_uac2_sink_device.clone(),
        usb_uac2_source_device: args.usb_uac2_source_device.clone(),
    };

    // A configured power backend must be visible to the controller dashboard.
    // The controller renders power controls from the registration `cap` string,
    // not from the protocol capability object, so keep both in sync.
    let has_power_backend = config
        .vm_providers()
        .map(|providers| providers.power.is_some())
        .unwrap_or(false);
    if has_power_backend && !config.has_capability("power") {
        config.capabilities.push("power".to_owned());
        let advertised = args.cap.split(',').map(str::trim).collect::<Vec<_>>();
        if !advertised.contains(&"power") {
            if !args.cap.trim().is_empty() {
                args.cap.push(',');
            }
            args.cap.push_str("power");
        }
    }

    // A monitor capture target names an existing sink; advertise that sink to
    // the controller unless --audio-sink already did.
    if config.audio_sink.is_none() {
        if let ozma_audio::AudioTarget::SinkMonitor(sink) = &config.audio_capture_target {
            config.audio_sink = Some(sink.clone());
            if args.audio_sink.is_none() {
                args.audio_sink = Some(sink.clone());
            }
        }
    }

    if let Err(error) = config.validate() {
        error!("Invalid node configuration: {error}");
        std::process::exit(2);
    }
    audio_runtime::configure_target(config.audio_capture_target.clone());

    let vm_providers = match config.vm_providers() {
        Ok(providers) => providers,
        Err(error) => {
            error!("Invalid provider configuration: {error}");
            std::process::exit(2);
        }
    };

    // Resolve the controller-facing display outputs. Explicit
    // `--display-output` specs go through `VmDisplayTopology`; otherwise a VM
    // capture provider contributes one implicit console. The raw JSON escape
    // hatch (`--display-outputs-json`) is left untouched for host outputs.
    match config.resolve_display_outputs() {
        Ok(selection) => {
            if let Some(limitation) = selection.limitation.as_deref() {
                warn!("display outputs: {limitation}");
            }
            if !selection.outputs.is_empty() {
                let json = selection.to_json();
                config.display_outputs = selection.outputs;
                config.display_outputs_json = json;
                // `build_txt` mirrors the registration fields from `Args`, so
                // keep both copies identical.
                args.display_outputs_json = config.display_outputs_json.clone();
            }
        }
        Err(error) => {
            error!("Invalid display-output configuration: {error}");
            std::process::exit(2);
        }
    }

    if config.fake_capture_requested() {
        warn!(
            width = config.fake_width,
            height = config.fake_height,
            fps = config.fake_fps,
            "DEV/TEST SYNTHETIC CAPTURE: `--capture-provider fake` is producing a \
             deterministic RGB24 pattern. This is not a real camera, guest, or hardware \
             source; never use it outside development/validation."
        );
    }

    // Say exactly which desktop capture transport was selected. Portal consent
    // may appear later, and an X11 display is configuration, not a probe.
    if config.profile == NodeProfile::Desktop {
        if let Ok(providers) = config.desktop_providers() {
            match &providers.capture {
                DesktopCaptureSelection::X11(x11) => {
                    let ffmpeg = &x11.ffmpeg_bin;
                    let ffmpeg_found = if ffmpeg.components().count() > 1 {
                        ffmpeg.is_file()
                    } else {
                        command_on_path(&ffmpeg.to_string_lossy())
                    };
                    if ffmpeg_found {
                        info!(
                            display = %x11.display,
                            region = %format!("{}+{},{}", x11.display, x11.x, x11.y),
                            mode = %format!("{}x{}@{}", x11.width, x11.height, x11.fps),
                            ffmpeg = %ffmpeg.display(),
                            "desktop capture: X11 x11grab selected; the display is not probed \
                             until the first frame"
                        );
                    } else {
                        warn!(
                            display = %x11.display,
                            ffmpeg = %ffmpeg.display(),
                            "desktop capture: X11 selected but ffmpeg was not found; opening \
                             capture will fail with a typed error and the node stays running \
                             without video"
                        );
                    }
                }
                DesktopCaptureSelection::Portal(options) => info!(
                    source = options.source.as_str(),
                    cursor = options.cursor.as_str(),
                    timeout_ms = options.timeout.as_millis() as u64,
                    restore_token = options.restore_token.is_some(),
                    "desktop capture: XDG Portal ScreenCast selected; the consent/selection \
                     handshake runs before capture starts. A denial/timeout leaves the node \
                     running with no capture."
                ),
                _ => {}
            }
        }
    }

    // QEMU accepts one client per `-qmp` chardev. The node creates exactly one
    // shared control client here and hands clones to power, QMP capture,
    // snapshots (through the capture registry), and QMP input. A dedicated
    // `--qmp-input-socket` is a separate input-only client and never takes a
    // second control connection.
    let shared_qmp = match vm_providers.qmp_control_config() {
        Some(qmp_config) => {
            let socket = qmp_config.control_path.clone();
            let shared = SharedQmpClient::new(qmp_config);
            match shared.ensure_connected().await {
                Ok(()) => info!(
                    socket = %socket.display(),
                    owner = "node-shared-control",
                    "QMP control socket: one shared client for power, capture, snapshots, and input"
                ),
                Err(error) => warn!(
                    socket = %socket.display(),
                    owner = "node-shared-control",
                    error = %error,
                    "QMP control socket: shared client created; initial connect failed and commands will retry"
                ),
            }
            Some(shared)
        }
        None => None,
    };
    if let Some(input_socket) = vm_providers.qmp_input_socket.as_ref() {
        info!(
            socket = %input_socket.display(),
            "QMP input uses the dedicated input socket; it does not open another control client"
        );
    }

    // Capture: hardware keeps V4L2/PipeWire; a VM uses only its QMP or VNC
    // display. A VM without either flag gets an empty registry so it never
    // advertises host hardware it does not own. QMP display sources share the
    // control client above, so HLS and snapshots serialize their screendumps.
    //
    // A desktop portal stream is resolved first because the consent/selection
    // handshake is asynchronous and may prompt the user. A denial/timeout or a
    // missing portal is a warned no-capture state: the registry stays empty and
    // no X11/camera/fake source is substituted.
    #[cfg(target_os = "linux")]
    let capture_registry = {
        let desktop_capture = if config.profile == NodeProfile::Desktop {
            match config.desktop_providers() {
                Ok(providers) => Some(providers.capture),
                Err(error) => {
                    error!("Invalid desktop provider configuration: {error}");
                    std::process::exit(2);
                }
            }
        } else {
            None
        };
        let portal_stream = match desktop_capture {
            Some(DesktopCaptureSelection::Portal(options)) => {
                let portal_config = platform::portal_screencast_config(&options);
                match ozma_capture::PortalScreenCast::connect(portal_config).await {
                    Ok(portal) => match platform::acquire_portal_stream(&portal).await {
                        Some(stream) => {
                            let node_id = stream.node_id().ok();
                            info!(
                                node_id = ?node_id,
                                has_fd = stream.pipewire_fd.is_some(),
                                restore_token = stream.restore_token.is_some(),
                                "portal ScreenCast: stream selected; the portal D-Bus session and \
                                 fd are held for the capture source's lifetime. A restore token is \
                                 reported but not persisted by this node."
                            );
                            Some(stream)
                        }
                        None => None,
                    },
                    Err(error) => {
                        let api = error.to_api_error();
                        warn!(
                            code = %api.code,
                            error = %error,
                            "portal ScreenCast: could not connect to the session bus/portal. The \
                             node keeps running without capture; no X11, camera, or fake source \
                             is substituted."
                        );
                        None
                    }
                }
            }
            _ => None,
        };
        platform::capture_registry_with_portal(&config, shared_qmp.as_ref(), portal_stream)
    };
    #[cfg(not(target_os = "linux"))]
    let capture_registry = platform::capture_registry_with_qmp_shared(&config, shared_qmp.as_ref());
    // HLS, WebRTC, snapshots, and the runtime console selector all read this
    // slot. A `POST /display/select` swaps it only after every running pipeline
    // restarted from the new `Console_N`; a failed switch leaves the old
    // registry in place.
    let registry_slot = Arc::new(RwLock::new(capture_registry.clone()));
    if config.profile == NodeProfile::Vm && !vm_providers.has_capture() {
        warn!(
            "vm profile has no --qmp-socket, --vnc-host/--vnc-port, D-Bus display flags, or \
             --looking-glass-shm: no capture source is configured. Hardware capture is not \
             advertised on a VM."
        );
    }
    let mut capabilities = platform::detect_with_registry(&config, &capture_registry);
    if config.profile == NodeProfile::Vm && capabilities.capture.is_empty() {
        if let Some(source) = config.capture_display_source().ok().flatten() {
            if source.backend == ozma_display::vm::VmConsoleBackend::LookingGlass {
                warn!(
                    shm = ?config.looking_glass_shm,
                    "Looking Glass capture: no KVMFR source was advertised. The path is absent, \
                     is not a regular file, lacks the KVMFR magic, or has no published frame. \
                     The node keeps running without video and never substitutes a synthetic or \
                     emulated-display source."
                );
            }
        }
    }
    if config.profile == NodeProfile::Desktop && !config.fake_capture_requested() {
        let desktop_selection = config
            .desktop_providers()
            .ok()
            .map(|providers| providers.capture);
        if capabilities.capture.is_empty() {
            for scan in capture_registry.scan() {
                if let Some(error) = scan.error {
                    warn!(
                        kind = ?scan.kind,
                        error = %error.message,
                        "desktop capture: provider discovery failed; no source is advertised"
                    );
                }
            }
            match desktop_selection {
                Some(DesktopCaptureSelection::X11(x11)) => warn!(
                    display = %x11.display,
                    ffmpeg = %x11.ffmpeg_bin.display(),
                    "desktop capture: the X11 provider advertised no source. The display was \
                     rejected, ffmpeg is absent, or the X server is not reachable. The node \
                     keeps running without video; no fake or camera source is substituted."
                ),
                Some(DesktopCaptureSelection::Portal(options)) => warn!(
                    source = options.source.as_str(),
                    timeout_ms = options.timeout.as_millis() as u64,
                    "desktop capture: the portal ScreenCast handshake produced no capturable \
                     source (denied, timed out, portal unavailable, or the fd is unsupported by \
                     the PipeWire backend). The node keeps running without video; no X11, \
                     camera, or fake source is substituted."
                ),
                Some(DesktopCaptureSelection::PipeWire(_)) => warn!(
                    target = %config.desktop_capture_target,
                    remote = ?config.desktop_capture_remote,
                    "desktop capture: no PipeWire Video/Source node was discovered; the node \
                     keeps running without video (no fake source is substituted). Use \
                     --capture-provider portal for the ScreenCast consent flow, --capture-provider \
                     x11 with --x11-display, or pass the portal-selected node id/name as \
                     --desktop-capture-target."
                ),
                _ => warn!(
                    "desktop capture: no capture provider is configured; the node keeps running \
                     without video."
                ),
            }
        } else {
            let first = &capabilities.capture[0];
            info!(
                provider = desktop_selection
                    .as_ref()
                    .map(DesktopCaptureSelection::provider_name)
                    .unwrap_or("unknown"),
                target = %config.desktop_capture_target,
                sources = capabilities.capture.len(),
                source_id = %first.id,
                source_name = %first.name,
                "desktop capture: video source discovered"
            );
        }
    }
    // Advertise the discovered desktop source to the controller instead of
    // leaving `capture_device` blank. An explicit --capture-device still wins.
    if config.profile == NodeProfile::Desktop && config.capture_device.is_none() {
        if let Some(source) = capabilities.capture.first() {
            config.capture_device = Some(source.id.clone());
            args.capture_device = Some(source.id.clone());
        }
    }
    let quic_port = config.http_port.saturating_add(2);

    // One input sink is shared by session input and the controller's UDP HID
    // listener. The chain comes from the profile and flags; every real sink is
    // wrapped in a recorder so the path is observable. The QMP task must stay
    // alive for injection to continue.
    //
    // Precedence for QMP input: a dedicated `--qmp-input-socket` (input only),
    // else the shared control client, else the legacy owned-client config.
    let mut input_factory = input_select::SystemInputFactory::new(vm_providers.qmp_input_config());
    if let Some(shared) = shared_qmp.clone() {
        input_factory = input_factory.with_shared_client(shared);
    }
    if let Some(dedicated) = vm_providers.qmp_dedicated_input_config() {
        input_factory = input_factory.with_dedicated_input(dedicated);
    }
    let selected_input =
        input_select::select_input(&input_factory, &vm_providers.input_chain).await;
    let input_sink = selected_input.sink;
    let sink_kind = sink_kind_name(selected_input.kind);
    // Report only the input modes the selected sink can actually inject. A
    // recording-only fallback (uinput/QMP/HID unavailable) advertises none.
    capabilities.input = platform::input_capability_for(Some(selected_input.kind));
    if config.profile == NodeProfile::Desktop && selected_input.kind == SinkKind::Recording {
        warn!(
            "desktop input: no injection sink opened; input is recorded but NOT injected. \
             /dev/uinput access (udev rule or input group) is required for host input. \
             The node stays up and advertises no keyboard/pointer injection."
        );
    }
    if config.profile == NodeProfile::Desktop && !capabilities.input.keyboard {
        warn!(
            selected = %selected_input.kind,
            "desktop input: selected sink does not advertise keyboard/pointer injection"
        );
    }
    if let Some(handle) = selected_input.handle {
        background.push(("qmp-input", handle));
    }
    info!(sink = sink_kind, "Input sink ready");

    // Resolve the advertised address before the opt-in RFB listener so a
    // wildcard bind can report a connectable host instead of 0.0.0.0.
    let local_ip = resolve_local_ip(&args);
    info!(ip = %local_ip, "Resolved local IP");

    // Read the VNC password once, before binding. A bad file is a startup
    // error rather than a silently unauthenticated listener. The password
    // value itself is never logged.
    let vnc_password = match args.vnc_password_file.as_deref() {
        Some(path) => match ozma_node::vnc_server::VncPassword::from_file(path) {
            Ok(password) => Some(password),
            Err(error) => {
                error!("Invalid VNC password file: {error}");
                std::process::exit(2);
            }
        },
        None => None,
    };
    // Desktop controls are built here so the opt-in VNC clipboard bridge can
    // share the same X11 selection owner as the /clipboard route. They remain
    // attached to /api/v1/connection below.
    let desktop_controls = (config.profile == NodeProfile::Desktop).then(|| {
        let x11_display = config
            .x11_display
            .clone()
            .or_else(|| std::env::var("DISPLAY").ok())
            .map(|display| display.trim().to_owned())
            .filter(|display| !display.is_empty());
        let wayland_display = std::env::var("WAYLAND_DISPLAY")
            .ok()
            .map(|display| display.trim().to_owned())
            .filter(|display| !display.is_empty());
        ozma_node::desktop_compat::DesktopControls::system(x11_display, wayland_display)
    });
    let vnc_clipboard: Option<Arc<dyn ozma_node::vnc_server::ClipboardAccess>> =
        if args.vnc_clipboard {
            match (config.profile, desktop_controls.as_ref()) {
                (NodeProfile::Desktop, Some(controls)) => Some(Arc::new(
                    ozma_node::vnc_server::DesktopClipboard::new(controls.clone()),
                )),
                _ => {
                    warn!(
                        "--vnc-clipboard requires --profile desktop with a desktop clipboard; \
                         cut text is ignored (no-op) and status reports clipboard unavailable"
                    );
                    None
                }
            }
        } else {
            None
        };
    if args.vnc_clipboard
        && vnc_clipboard
            .as_ref()
            .is_some_and(|clipboard| !clipboard.available())
    {
        warn!(
            "--vnc-clipboard is enabled but the desktop clipboard is unavailable; \
             VNC cut text will be ignored and status will report clipboard unavailable"
        );
    }
    let vnc_options = ozma_node::vnc_server::VncServerOptions {
        bind: args.vnc_bind.clone(),
        port: args.vnc_server_port,
        password: vnc_password,
        max_clients: args.vnc_max_clients,
        idle_timeout: (args.vnc_idle_timeout_seconds > 0)
            .then(|| Duration::from_secs(args.vnc_idle_timeout_seconds)),
        allow_unauthenticated: args.vnc_allow_unauthenticated,
        clipboard: vnc_clipboard,
        clipboard_max_bytes: args.vnc_clipboard_max_bytes,
    };

    // Opt-in RFB 3.8 server. It binds only when --vnc-server is set *and* the
    // capture registry has a source to feed it; otherwise registration keeps
    // the honest typed not-configured state. The endpoint is captured before
    // the handle is moved to its shutdown task.
    let vnc_server = match ozma_node::vnc_server::start_if_configured(
        args.vnc_server,
        vnc_options,
        &capture_registry,
        input_sink.clone(),
        &config.name,
    )
    .await
    {
        Ok(server) => server,
        Err(
            error @ ozma_node::vnc_server::VncServerError::UnauthenticatedWiderBindRefused {
                ..
            },
        ) => {
            // A policy failure must stop startup, not fall through to a
            // warning that could be mistaken for "VNC simply unavailable".
            error!("VNC server bind policy refused: {error}");
            std::process::exit(2);
        }
        Err(error) => {
            warn!(
                error = %error,
                "VNC server not started; /api/v1/connection.registration keeps VNC \
                 not-configured unless --vnc-host/--vnc-port names an external endpoint"
            );
            None
        }
    };
    let served_vnc = vnc_server
        .as_ref()
        .map(|server| server.registration_endpoint(&local_ip));
    // Registration keeps the live listener first and the configured external
    // QEMU endpoint as fallback. The listener task clears the live slot as soon
    // as it stops (capture failure or shutdown), so `/api/v1/connection` never
    // advertises a dead port until restart. mDNS TXT is advertised once at
    // startup and is not retracted mid-run.
    let external_vnc = match (args.vnc_host.as_deref(), args.vnc_port) {
        (Some(host), Some(port)) => Some((host.to_owned(), port)),
        _ => None,
    };
    let vnc_registration =
        controller_compat::VncRegistrationState::new(served_vnc.clone(), external_vnc);
    let vnc_live_handle = vnc_registration.live_handle();
    if let Some(server) = vnc_server {
        let (host, port) = served_vnc
            .clone()
            .expect("the VNC endpoint was resolved before moving the handle");
        let vnc_live_flag = server.live_flag();
        let vnc_live_slot = vnc_live_handle.clone();
        background.push((
            "vnc-registration",
            tokio::spawn(async move {
                while vnc_live_flag.load(std::sync::atomic::Ordering::SeqCst) {
                    tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                }
                *vnc_live_slot
                    .write()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
            }),
        ));
        info!(
            bind = %server.bound_addr(),
            advertised_host = %host,
            advertised_port = port,
            security = if server.auth_enabled() { "vnc-auth" } else { "none" },
            encrypted = false,
            clipboard = if !server.clipboard_status().enabled {
                "disabled"
            } else if server.clipboard_status().available {
                "available"
            } else {
                "unavailable"
            },
            clipboard_max_bytes = args.vnc_clipboard_max_bytes,
            "RFB 3.8 VNC server is serving the first capture source \
             (transport is plain TCP; VNC auth is not encryption)"
        );
        let vnc_shutdown = shutdown.clone();
        background.push((
            "vnc-server",
            tokio::spawn(async move {
                vnc_shutdown.cancelled().await;
                server.shutdown().await;
            }),
        ));
    } else if args.vnc_server {
        warn!(
            "VNC server was requested but no capture source or usable bind was available; \
             no VNC listener is advertised"
        );
    }

    let power = build_power(&vm_providers, shared_qmp.clone()).await;

    // Audio routing controls an existing PipeWire/PulseAudio graph. `none`
    // leaves /audio/* at typed 501. A missing `pactl` marks the backend failed
    // (reported on /api/v1/connection) instead of claiming success later.
    let audio_control = build_audio_control(&config).await;
    if let Some(control) = audio_control.as_ref() {
        match control.error() {
            Some(error) => warn!(
                backend = control.backend(),
                error = %error,
                "Audio routing unavailable: no success will be claimed on /audio/*"
            ),
            None => info!(backend = control.backend(), "Audio router ready"),
        }
    }

    if let Some(template) = args.multi_console_hls_audio_source.as_deref() {
        if let Err(error) = ozma_node::multi_hls::validate_audio_source_template(template) {
            error!("Invalid --multi-console-hls-audio-source: {error}");
            std::process::exit(2);
        }
    }

    // Local HLS compatibility pipeline. HLS is opt-in; a missing source or
    // ffmpeg leaves it unavailable and reported as `failed`, never as running.
    // The controller owns the live pipeline so POST /codec can restart it with
    // a newly selected software encoder.
    let hls_output_dir = (args.hls || args.multi_console_hls).then(|| {
        args.hls_dir
            .clone()
            .unwrap_or_else(|| default_hls_dir(&config.name))
    });
    // Per-console HLS owns every encoder only for a provider that really
    // exposes independent console indexes (D-Bus). A single-head provider keeps
    // the historical active single-HLS path and reports the per-console
    // limitation instead of losing its stream.
    let multi_console_hls_owns_stream = args.multi_console_hls
        && config
            .capture_display_source()
            .ok()
            .flatten()
            .is_some_and(|source| source.backend == ozma_display::vm::VmConsoleBackend::Dbus);
    let mut hls_controller: Option<HlsController> = None;
    let mut hls_config_for_capabilities: Option<HlsConfig> = None;
    let hls_status = if let Some(directory) = hls_output_dir.as_ref() {
        let encoder = match HlsEncoder::parse(&args.hls_encoder) {
            Ok(encoder) => encoder,
            Err(error) => {
                error!("Invalid HLS encoder: {error}");
                std::process::exit(2);
            }
        };
        let pixel_format = match resolve_hls_pixel_format(
            args.hls_pixel_format.as_deref(),
            args.hls_10bit,
            args.hls_hdr,
        ) {
            Ok(pixel_format) => pixel_format,
            Err(error) => {
                error!("Invalid HLS pixel format: {error}");
                std::process::exit(2);
            }
        };
        let audio_source = match resolve_hls_audio_source(
            args.hls_audio,
            args.hls_audio_source.as_deref(),
            config.audio_sink.as_deref(),
            &config.audio_capture_target,
        ) {
            Ok(source) => source,
            Err(error) => {
                error!("Invalid HLS audio configuration: {error}");
                std::process::exit(2);
            }
        };
        let audio =
            audio_source.map(|source| HlsAudioConfig::new(source, args.hls_audio_bitrate_kbps));
        let hls_config = HlsConfig {
            output_dir: directory.clone(),
            fps: args.hls_fps,
            bitrate_kbps: args.hls_bitrate_kbps,
            audio,
            video_encoder: encoder,
            pixel_format,
            color_primaries: args.hls_color_primaries.clone(),
            color_transfer: args.hls_color_transfer.clone(),
            color_matrix: args.hls_color_matrix.clone(),
            hdr: args.hls_hdr,
            ffmpeg_bin: args.ffmpeg_bin.clone(),
            vaapi_device: config.vaapi_device.clone(),
            nvenc_gpu: config.nvenc_gpu,
            ..HlsConfig::default()
        };
        hls_config_for_capabilities = Some(hls_config.clone());
        if let Err(error) = hls_config.validate() {
            error!("Invalid HLS configuration: {error}");
            std::process::exit(2);
        }
        if let Err(error) = hls_config.validate_encoder_options() {
            error!("Invalid HLS configuration: {error}");
            std::process::exit(2);
        }
        if multi_console_hls_owns_stream {
            // The per-console manager owns every pipeline (including the
            // active console) so a console is never encoded twice. It is
            // constructed after the display selector, below.
            if !config.has_capability("video") {
                warn!(
                    "--multi-console-hls was requested but the video capability is disabled; \
                     per-console HLS is unavailable"
                );
                HlsStatus::failed("video capability is disabled")
            } else {
                HlsStatus::not_configured()
            }
        } else if !config.has_capability("video") {
            warn!("--hls was requested but the video capability is disabled; HLS is unavailable");
            HlsStatus::failed("video capability is disabled")
        } else {
            let registry = registry_slot.clone();
            let requested_source = capture_source_selection(&config);
            let source_factory: HlsSourceFactory = std::sync::Arc::new(move || {
                let registry = registry
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                open_capture_source(&registry, requested_source.as_deref())
                    .map_err(|message| HlsError::Source { message })
            });
            let controller = HlsController::new(source_factory, hls_config);
            match controller.start().await {
                Ok(status) => {
                    info!(
                        dir = %directory.display(),
                        manifest = %controller.config().manifest_path().display(),
                        encoder = controller.config().video_encoder.ffmpeg_name(),
                        "HLS pipeline started (status becomes running when the manifest exists)"
                    );
                    hls_controller = Some(controller);
                    status
                }
                Err(error) => {
                    warn!(
                        error = %error,
                        "HLS unavailable: the local stream pipeline did not start"
                    );
                    hls_controller = Some(controller);
                    HlsStatus::failed(format!("HLS unavailable: {error}"))
                }
            }
        }
    } else {
        HlsStatus::not_configured()
    };

    // Advertise the encoder that HLS is actually configured to run, with the
    // real source envelope and HDR only when a 10-bit HDR target passed
    // validation. Without HLS this leaves the conservative native capability
    // untouched.
    if let Some(hls_config) = hls_config_for_capabilities.as_ref() {
        platform::apply_hls_encode_capability(&mut capabilities, hls_config);
    }

    // WebRTC helper (external GStreamer/Python or the native Rust binary).
    // WebRTC is disabled unless `--webrtc-helper` or `--webrtc-native` is
    // configured. A configured helper that cannot start is reported as
    // `failed`, never `running`, and never crashes the node.
    // Per-console WebRTC only owns the helper set when the configured capture
    // provider really exposes independent console indexes (D-Bus). A
    // single-head provider keeps the historical active helper and the manager
    // reports a typed `unsupported` limitation instead.
    let webrtc_capture_provider = config
        .capture_display_source()
        .ok()
        .flatten()
        .map(|source| source.backend);
    let multi_console_webrtc_owns = args.multi_console_webrtc
        && webrtc_capture_provider == Some(ozma_display::vm::VmConsoleBackend::Dbus);
    let mut webrtc_switch_config: Option<WebRtcConfig> = None;
    let mut webrtc_selection_error: Option<(String, WebRtcBackend)> = None;
    let webrtc_selection: Option<HelperSelection> = if args.webrtc_native
        || args.webrtc_helper.is_some()
    {
        match select_helper(
            args.webrtc_native,
            args.webrtc_helper.as_deref(),
            args.webrtc_helper_script.as_deref(),
        ) {
            Ok(selection) => {
                if selection.script_ignored {
                    warn!(
                        "--webrtc-helper-script is ignored for the native WebRTC helper; \
                             the binary takes its flags directly"
                    );
                }
                Some(selection)
            }
            Err(error) => {
                warn!(
                    error = %error,
                    native = args.webrtc_native,
                    "WebRTC unavailable: the configured helper could not be resolved"
                );
                let backend = if args.webrtc_native {
                    WebRtcBackend::Native
                } else {
                    WebRtcBackend::External
                };
                webrtc_selection_error = Some((format!("WebRTC unavailable: {error}"), backend));
                None
            }
        }
    } else {
        None
    };
    let webrtc_control: Option<WebRtcControl> = if let Some((error, backend)) =
        webrtc_selection_error
    {
        Some(WebRtcControl::failed_with_audio_and_backend(
            error,
            args.webrtc_audio_source.clone(),
            backend,
        ))
    } else {
        match webrtc_selection {
            None => None,
            Some(selection) => {
                let audio_source = args.webrtc_audio_source.clone();
                let backend = selection.backend;
                let webrtc_config = WebRtcConfig {
                    helper_program: selection.program,
                    helper_script: selection.script,
                    backend,
                    socket_path: args
                        .webrtc_socket
                        .clone()
                        .unwrap_or_else(|| default_webrtc_socket(&config.name)),
                    shm_path: args.webrtc_shm.clone().unwrap_or_else(default_webrtc_shm),
                    width: args.webrtc_width,
                    height: args.webrtc_height,
                    scale_to_fit: args.webrtc_fit_capture,
                    fps: args.webrtc_fps,
                    bitrate_kbps: args.webrtc_bitrate_kbps,
                    audio_source: audio_source.clone(),
                    audio_backend: args.webrtc_audio_backend,
                    stun_server: args.webrtc_stun.clone(),
                    turn_server: args.webrtc_turn.clone(),
                    turn_user: args.webrtc_turn_user.clone(),
                    turn_password: args.webrtc_turn_password.clone(),
                    startup_timeout: std::time::Duration::from_secs(5),
                    offer_timeout: std::time::Duration::from_secs(10),
                };
                webrtc_switch_config = Some(webrtc_config.clone());
                if let Err(error) = webrtc_config.validate() {
                    warn!(error = %error, "WebRTC unavailable: invalid helper configuration");
                    Some(WebRtcControl::failed_with_audio_and_backend(
                        format!("WebRTC unavailable: {error}"),
                        audio_source,
                        backend,
                    ))
                } else if !config.has_capability("video") {
                    warn!(
                        "--webrtc-helper/--webrtc-native was configured but the video \
                         capability is disabled; WebRTC is unavailable"
                    );
                    Some(WebRtcControl::failed_with_audio_and_backend(
                        "WebRTC unavailable: the video capability is disabled".to_owned(),
                        audio_source,
                        backend,
                    ))
                } else if multi_console_webrtc_owns {
                    // The per-console manager starts one helper per advertised
                    // console (including the active console), so starting the
                    // historical single helper would duplicate that capture
                    // source and encoder. It is constructed after the display
                    // selector below.
                    info!(
                        socket = %webrtc_config.socket_path.display(),
                        shm = %webrtc_config.shm_path.display(),
                        "per-console WebRTC manager owns the helper set; the single active helper is not started"
                    );
                    None
                } else {
                    let requested_source = capture_source_selection(&config);
                    match open_capture_source(&capture_registry, requested_source.as_deref()) {
                        Err(error) => {
                            warn!(
                                error = %error,
                                "WebRTC unavailable: no capture source could be opened"
                            );
                            Some(WebRtcControl::failed_with_audio_and_backend(
                                format!("WebRTC unavailable: {error}"),
                                audio_source,
                                backend,
                            ))
                        }
                        Ok(mut source) => {
                            let mut start_config = webrtc_config.clone();
                            let mut fit_source_geometry: Option<(u32, u32)> = None;
                            let mut fit_settled: Option<bool> = None;
                            if args.webrtc_fit_capture {
                                // QMP/VNC sources report 0x0 until the first
                                // screendump, and the guest can modeset during
                                // boot. When discovery geometry is unknown, wait
                                // for a stable size instead of latching the first
                                // transient frame.
                                let declared = source.format();
                                let resolved = if declared.width > 0 && declared.height > 0 {
                                    Some((declared.width, declared.height))
                                } else {
                                    let settle_ms = args.webrtc_settle_ms.min(MAX_FIT_SETTLE_MS);
                                    let probe = FitProbe {
                                        settle_window: std::time::Duration::from_millis(settle_ms),
                                        max_wait: MAX_FIT_PROBE,
                                    };
                                    match probe_settled_geometry(source.as_mut(), probe).await {
                                        Some(result) => {
                                            fit_settled = Some(result.settled);
                                            info!(
                                                width = result.width,
                                                height = result.height,
                                                frames = result.frames,
                                                settled = result.settled,
                                                settle_ms,
                                                "WebRTC fit probe resolved the capture geometry"
                                            );
                                            Some((result.width, result.height))
                                        }
                                        None => {
                                            warn!(
                                                settle_ms,
                                                "WebRTC fit: no usable capture frame arrived; \
                                             keeping the configured size"
                                            );
                                            None
                                        }
                                    }
                                };
                                if let Some((width, height)) = resolved {
                                    if width != start_config.width || height != start_config.height
                                    {
                                        info!(
                                            configured = %format!(
                                                "{}x{}",
                                                start_config.width, start_config.height
                                            ),
                                            capture = %format!("{width}x{height}"),
                                            settled = ?fit_settled,
                                            "fitting WebRTC helper to the capture frame size"
                                        );
                                    }
                                    start_config.width = width;
                                    start_config.height = height;
                                    fit_source_geometry = Some((width, height));
                                }
                            }
                            match WebRtcHelper::start(source, start_config) {
                                Ok(helper) => {
                                    // Keep the geometry actually used, so a later
                                    // console switch restarts at the same size.
                                    let used = helper.config();
                                    webrtc_switch_config = Some(used.clone());
                                    let mut geometry = WebRtcGeometryStatus::from_config(used);
                                    geometry.fitted =
                                        fit_source_geometry.is_some_and(|(width, height)| {
                                            width != webrtc_config.width
                                                || height != webrtc_config.height
                                        });
                                    if let Some((width, height)) = fit_source_geometry {
                                        geometry.capture_width = Some(width);
                                        geometry.capture_height = Some(height);
                                    }
                                    geometry.settled = fit_settled;
                                    helper.record_geometry(geometry);
                                    info!(
                                        pid = ?helper.status().helper_pid(),
                                        socket = %used.socket_path.display(),
                                        shm = %used.shm_path.display(),
                                        width = used.width,
                                        height = used.height,
                                        fps = used.fps,
                                        backend = used.backend.as_str(),
                                        audio = ?used
                                            .audio_source
                                            .as_ref()
                                            .filter(|_| used.audio_supported()),
                                        audio_backend = used.audio_backend.as_arg(),
                                        stun = ?used.stun_server,
                                        turn = ?used.turn_server,
                                        turn_auth = used.turn_user.is_some()
                                            && used.turn_password.is_some(),
                                        "WebRTC helper started"
                                    );
                                    Some(WebRtcControl::from_helper(helper))
                                }
                                Err(error) => {
                                    warn!(
                                        error = %error,
                                        "WebRTC unavailable: the configured helper did not start"
                                    );
                                    Some(WebRtcControl::failed_with_audio_and_backend(
                                        format!("WebRTC unavailable: {error}"),
                                        audio_source,
                                        backend,
                                    ))
                                }
                            }
                        }
                    }
                }
            }
        }
    };

    // USB gadget creation is strictly opt-in. A configured gadget that cannot
    // be created/bound (missing UDC, ConfigFS root, or media image) is a warned
    // unavailable state: the node keeps running, advertises no USB capability,
    // and /usb answers a typed 503. A node without --usb-gadget keeps the typed
    // 501 surface.
    let mut usb_control = build_usb_control(&config);
    if let Some(control) = usb_control.as_mut() {
        if let Some(auto) = usb_media_auto_start_config(&config) {
            if control.is_available() {
                control.auto_start_media_export(&auto);
            } else {
                warn!(
                    error = control.error().unwrap_or("USB unavailable"),
                    "userspace SCSI media auto-start was requested but the gadget is unavailable"
                );
            }
        }
    }
    if let Some(control) = usb_control
        .as_mut()
        .filter(|control| control.is_available())
    {
        match build_usbip_runtime(&config) {
            Ok(runtime) => control.set_usbip_runtime(runtime),
            Err(error) => {
                warn!(
                    kind = error.kind(),
                    error = %error,
                    "USB/IP listener requested but not started; /usb reports the typed failure"
                );
                control.set_usbip_runtime(None);
            }
        }
    } else if config.usbip {
        warn!("USB/IP listener not started because the USB gadget is unavailable");
    }
    // Keep a cloned handle (shared `Arc`s) so shutdown can stop the media
    // export/USB/IP listener and apply its persistence policy after the state is
    // served.
    let usb_media_shutdown = usb_control.clone();
    let mut uac2_alsa_runtime: Option<usb_uac2::Uac2BridgeRuntime> = None;
    if let Some(control) = usb_control.as_mut() {
        if !control.is_available() {
            if config.usb_uac2_bridge {
                let reason = control
                    .error()
                    .map(str::to_owned)
                    .unwrap_or_else(|| "USB gadget is unavailable".to_owned());
                control.set_uac2_bridge(Some(usb_uac2::Uac2BridgeShared::unsupported(format!(
                    "UAC2 bridge not started: {reason}"
                ))));
            }
        } else if config.usb_uac2_bridge {
            match usb_uac2::start_real(&config) {
                Ok(runtime) => {
                    info!(
                        report = ?runtime.shared().report(),
                        "UAC2 isochronous bridge started"
                    );
                    control.set_uac2_bridge(Some(runtime.shared()));
                    uac2_alsa_runtime = Some(runtime);
                }
                Err(error) => {
                    let reason = format!("{}: {error}", error.kind());
                    warn!(
                        kind = error.kind(),
                        error = %error,
                        "UAC2 bridge requested but not started; /usb will report unsupported"
                    );
                    control.set_uac2_bridge(Some(usb_uac2::Uac2BridgeShared::unsupported(reason)));
                }
            }
        } else if config.usb_uac2 {
            control.set_uac2_bridge(Some(usb_uac2::Uac2BridgeShared::unsupported(
                "--usb-uac2-bridge was not requested; the UAC2 function is descriptor-only",
            )));
        }
    }
    if let Some(mut runtime) = uac2_alsa_runtime {
        let uac2_shutdown = shutdown.clone();
        background.push((
            "uac2-alsa-bridge",
            tokio::spawn(async move {
                uac2_shutdown.cancelled().await;
                runtime.stop();
            }),
        ));
    }
    if let Some(control) = usb_control
        .as_ref()
        .filter(|control| control.is_available())
    {
        capabilities.usb = control.protocol_capability();
        set_capability(&mut args, &mut config, "usb", true);
        info!(
            gadget = DEFAULT_USB_GADGET_NAME,
            udc = config.usb_udc.as_deref().unwrap_or("none"),
            media = ?config.usb_media,
            read_only = config.usb_media_ro || config.usb_media_cdrom,
            cdrom = config.usb_media_cdrom,
            "USB gadget created and bound; usb capability advertised"
        );
    } else {
        capabilities.usb = Default::default();
        let had_capability = config.has_capability("usb");
        set_capability(&mut args, &mut config, "usb", false);
        if let Some(control) = usb_control.as_ref() {
            warn!(
                error = control.error().unwrap_or("USB unavailable"),
                "USB gadget was requested but is unavailable; no usb capability is advertised \
                 and /usb will report the typed failure"
            );
        } else if had_capability {
            warn!(
                "the `usb` capability was requested without --usb-gadget; no USB gadget is \
                 created or advertised"
            );
        }
    }

    // Controller-compatibility registration fields that depend on real
    // configuration. Everything absent here is omitted from mDNS and direct
    // registration instead of being sent as an empty endpoint. The same value
    // is reported on `/api/v1/connection.registration` so a controller (or an
    // operator) can distinguish "not available" from "configured empty".
    // Precedence: a listening internal RFB server wins over an external QEMU
    // VNC host/port, because the controller can reach the node's advertised
    // address while a guest-local endpoint may be unreachable.
    let registration_extras = registration::RegistrationExtras::resolve_with_served_vnc(
        registration::seat_id_from_env().as_deref(),
        served_vnc
            .as_ref()
            .map(|(host, port)| (host.as_str(), *port)),
        args.vnc_host.as_deref(),
        args.vnc_port,
        args.vm_guest_ip.as_deref(),
    );

    let compat_stats = controller_compat::CompatStats::default();
    let connection_info = controller_compat::ConnectionInfo::new(
        node_id.as_str(),
        config.profile.as_str(),
        config.http_port,
        quic_port,
        config.hid_udp_port,
        sink_kind,
    )
    .with_registration(registration_extras.clone());
    // Runtime codec override control. Probed against the host exactly like the
    // session backend. When HLS is running, POST /codec restarts the HLS
    // pipeline with the selected software encoder; when HLS is off the override
    // is stored for a future start. WebRTC keeps its existing encoder.
    let mut codec_control = if config.has_capability("video") && !capabilities.encode.is_empty() {
        let detected = ozma_encode::EncoderRegistry::with_detected_encoders_for_device(Some(
            config.vaapi_device.as_path(),
        ))
        .await;
        // The encoder registry's own hardware caps still carry a conservative
        // 1080p ceiling. Fit the advertised list to the real source envelope
        // and the configured HLS encoder, so GET/POST /codec and its error
        // details describe the stream this node can actually run.
        let envelope = platform::capture_envelope(&capabilities.capture);
        let mut registry_caps: Vec<_> = detected.capabilities().iter().cloned().collect();
        let mut hdr_codec = None;
        if let Some(hls) = hls_config_for_capabilities.as_ref() {
            let hls_capability = platform::hls_encode_capability(hls, envelope);
            registry_caps.retain(|capability| {
                capability.codec != hls_capability.codec
                    || capability.backend != hls_capability.backend
            });
            registry_caps.push(hls_capability.clone());
            if hls.pixel_format.is_10bit() {
                // The HLS target bit depth is fixed for the pipeline lifetime.
                // Do not offer a codec/backend that has no Main10 mapping and
                // would fail the restart with a typed error.
                registry_caps.retain(|capability| {
                    matches!(
                        (capability.codec, capability.backend),
                        (
                            ozma_protocol::Codec::Hevc,
                            ozma_protocol::EncoderBackend::Software
                                | ozma_protocol::EncoderBackend::Vaapi
                                | ozma_protocol::EncoderBackend::Nvenc
                        ) | (
                            ozma_protocol::Codec::Av1,
                            ozma_protocol::EncoderBackend::Software
                        )
                    )
                });
                if hls.hdr {
                    hdr_codec = Some(hls_capability.codec);
                }
            }
        }
        platform::fit_encode_capabilities(&mut registry_caps, envelope, hdr_codec);
        let registry = ozma_encode::EncoderRegistry::from_capabilities(registry_caps);
        let format = ozma_protocol::MediaFormat::new(
            ozma_protocol::Codec::H264,
            envelope.width,
            envelope.height,
            args.hls_fps.max(1).min(envelope.fps.max(1)),
            args.hls_bitrate_kbps.max(1),
        );
        Some(CodecControl::new(registry, format).with_hls(hls_controller.clone()))
    } else {
        None
    };
    let codec_status = controller_compat::CodecStatus::from_codecs(
        capabilities.encode.iter().map(|encode| encode.codec),
    );
    let display_source = config.capture_display_source().ok().flatten();
    let display_selection = config.resolve_display_outputs().ok();
    let display_outputs_value: serde_json::Value =
        serde_json::from_str(&config.display_outputs_json)
            .unwrap_or_else(|_| serde_json::json!([]));
    let display_capture = controller_compat::DisplayCaptureStatus::new(
        display_source
            .as_ref()
            .map(|source| source.console_index)
            .or_else(|| {
                display_selection
                    .as_ref()
                    .filter(|selection| !selection.outputs.is_empty())
                    .map(|selection| selection.active_console)
            }),
        display_source
            .as_ref()
            .map(|source| source.backend.as_str().to_owned()),
        display_selection.and_then(|selection| selection.limitation),
    );
    // Runtime console selection. The selector shares the registry slot with
    // HLS/WebRTC/snapshots and restarts only pipelines that are actually
    // running. Single-head providers still construct a control so a request for
    // their active console is an idempotent 200 and any other console is a
    // typed 409.
    let mut display_pipelines: Vec<Arc<dyn SwitchablePipeline>> = Vec::new();
    if let Some(controller) = hls_controller.as_ref() {
        display_pipelines.push(HlsPipelineSwitch::new(controller.clone()));
    }
    if let (Some(control), Some(webrtc_config)) =
        (webrtc_control.as_ref(), webrtc_switch_config.clone())
    {
        if control.is_running() {
            display_pipelines.push(WebRtcPipelineSwitch::new(control.clone(), webrtc_config));
        }
    }
    let console_factory: Arc<dyn ConsoleSourceFactory> = Arc::new(ConfigConsoleFactory::new(
        config.clone(),
        shared_qmp.clone(),
    ));
    let display_control = match DisplayControl::new(
        console_factory.clone(),
        registry_slot.clone(),
        display_pipelines,
    ) {
        Ok(control) => Some(control),
        Err(error) => {
            warn!(
                error = %error,
                "runtime display console selection is unavailable; the node keeps the \
                 startup console and advertised outputs"
            );
            None
        }
    };
    // Opt-in true simultaneous per-console HLS. The manager owns one pipeline
    // per advertised console (including the active console) and is reconciled
    // by the selector observer below. It is deliberately separate from the
    // WebRTC/active-registry path.
    let mut display_control = display_control;
    let multi_console_hls = if args.multi_console_hls && config.has_capability("video") {
        let root = hls_output_dir
            .clone()
            .unwrap_or_else(|| default_hls_dir(&config.name));
        let base = hls_config_for_capabilities
            .clone()
            .unwrap_or_else(|| HlsConfig {
                output_dir: root.clone(),
                ..HlsConfig::default()
            });
        let manager = MultiConsoleHls::from_console_factory(
            console_factory.clone(),
            MultiConsoleHlsConfig {
                max_pipelines: args.multi_console_hls_max,
                idle_timeout: Duration::from_secs(args.multi_console_hls_idle_seconds),
                root: root.clone(),
                base,
                audio_source_template: args.multi_console_hls_audio_source.clone(),
                audio_bitrate_kbps: args.hls_audio_bitrate_kbps,
            },
        );
        let selection = display_control
            .as_ref()
            .map(DisplayControl::selection)
            .unwrap_or_else(|| {
                config
                    .resolve_display_outputs()
                    .unwrap_or_else(|_| ozma_node::display_outputs::DisplayOutputSelection::empty())
            });
        manager.sync(&selection).await;
        let status = manager.status().await;
        info!(
            dir = %root.display(),
            max_pipelines = status.max_pipelines,
            active_console = status.active_console,
            consoles = status.consoles.len(),
            limitation = ?status.limitation,
            "per-console HLS manager reconciled advertised consoles"
        );
        Some(manager)
    } else {
        None
    };
    // Opt-in true simultaneous per-console WebRTC. Like the HLS manager, it
    // reconciles the advertised consoles at startup and after every committed
    // display selection. It is constructed even for a single-head provider so
    // `/api/v1/connection` can report the typed limitation; in that case the
    // legacy active helper above remains the stream source.
    let multi_console_webrtc = if args.multi_console_webrtc && config.has_capability("video") {
        match webrtc_switch_config.clone() {
            None => {
                warn!(
                    "--multi-console-webrtc was requested without --webrtc-helper or \
                     --webrtc-native; no per-console WebRTC helper can start"
                );
                None
            }
            Some(base) => {
                let manager = MultiConsoleWebrtc::from_console_factory(
                    console_factory.clone(),
                    base,
                    MultiConsoleWebrtcConfig {
                        max_helpers: args.multi_console_webrtc_max,
                        idle_timeout: Duration::from_secs(args.multi_console_webrtc_idle_seconds),
                        audio_source_template: args.multi_console_webrtc_audio_source.clone(),
                    },
                );
                let selection = display_control
                    .as_ref()
                    .map(DisplayControl::selection)
                    .unwrap_or_else(|| {
                        config.resolve_display_outputs().unwrap_or_else(|_| {
                            ozma_node::display_outputs::DisplayOutputSelection::empty()
                        })
                    });
                manager.sync(&selection).await;
                let status = manager.status().await;
                info!(
                    max_helpers = status.max_helpers,
                    active_console = status.active_console,
                    consoles = status.consoles.len(),
                    limitation = ?status.limitation,
                    "per-console WebRTC manager reconciled advertised consoles"
                );
                Some(manager)
            }
        }
    } else {
        None
    };
    if let Some(control) = codec_control.take() {
        codec_control = Some(control.with_multi_console_hls(multi_console_hls.clone()));
    }
    if let Some(manager) = multi_console_hls.clone() {
        display_control =
            display_control.map(|control| control.with_observer(std::sync::Arc::new(manager)));
    }
    if let Some(manager) = multi_console_webrtc.clone() {
        display_control =
            display_control.map(|control| control.with_observer(std::sync::Arc::new(manager)));
    }
    // The broker is a separate snapshot/info path for advertised consoles. It
    // never replaces `registry_slot`, so the active HLS/WebRTC source keeps its
    // console. Sources for other consoles are opened lazily, bounded, and idle
    // evicted.
    let console_broker = display_control
        .as_ref()
        .map(|_| ConsoleBroker::new(console_factory, ConsoleBrokerConfig::default()));
    let capture_provider = config
        .capture_provider_name()
        .ok()
        .flatten()
        .unwrap_or_else(|| "none".to_owned());
    // Advertise the source the node actually selected, not whichever device
    // happened to enumerate first. A configured-but-absent device is reported
    // as unavailable with its requested id rather than as another camera.
    let capture_status = match capture_source_selection(&config) {
        Some(requested) => match capabilities.capture.iter().find(|capability| {
            ozma_node::display_control::capability_matches(capability, &requested)
        }) {
            Some(capability) => controller_compat::CaptureStatus::from_capability(
                capture_provider,
                Some(capability),
            ),
            None => controller_compat::CaptureStatus {
                provider: capture_provider,
                kind: None,
                source: Some(requested),
                name: None,
                available: false,
            },
        },
        None => controller_compat::CaptureStatus::from_capability(
            capture_provider,
            capabilities.capture.first(),
        ),
    };
    let hardware_state = ozma_node::hardware::HardwareState::new();
    let backup_service = match args.backup_repo.clone() {
        None => None,
        Some(repo) => {
            let config = ozma_node::backup::BackupConfig {
                repo,
                password_file: args.backup_password_file.clone().unwrap_or_default(),
                sources: args.backup_source.clone(),
                restore_roots: args.backup_restore_dir.clone(),
                restic_bin: args
                    .backup_restic
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from("restic")),
                timeouts: ozma_node::backup::BackupTimeouts::default(),
            };
            match ozma_node::backup::BackupService::new(config) {
                Ok(service) => {
                    info!(
                        repo = %service.config().repo.display(),
                        sources = service.config().sources.len(),
                        restore_roots = service.config().restore_roots.len(),
                        "local backup provider configured"
                    );
                    Some(service)
                }
                Err(error) => {
                    error!(
                        code = error.code(),
                        error = %error,
                        remediation = error.remediation(),
                        "invalid local backup configuration"
                    );
                    std::process::exit(2);
                }
            }
        }
    };
    // Opt-in Bluetooth provider. It is installed even when the host has no
    // adapter or bluetoothctl, so every route can return the typed 503 instead
    // of a fake list. The registration capability is only advertised after a
    // real (read-only) adapter probe succeeds.
    let bluetooth_service = args.bluetooth.then(|| {
        ozma_node::bluetooth::BluetoothService::new(ozma_node::bluetooth::BluetoothConfig {
            program: args
                .bluetooth_program
                .clone()
                .unwrap_or_else(|| std::path::PathBuf::from("bluetoothctl")),
            adapter: args.bluetooth_adapter.clone(),
            timeouts: ozma_node::bluetooth::BluetoothTimeouts::default(),
        })
    });
    let bluetooth_available = match bluetooth_service.as_ref() {
        None => false,
        Some(service) => match service.probe().await {
            Ok(adapter) => {
                info!(adapter = %adapter, "Bluetooth provider probed; bluetooth capability advertised");
                true
            }
            Err(error) => {
                warn!(
                    code = error.code(),
                    error = %error,
                    remediation = error.remediation(),
                    "Bluetooth was requested but is unavailable; /bluetooth/* reports the typed failure"
                );
                false
            }
        },
    };
    set_capability(&mut args, &mut config, "bluetooth", bluetooth_available);
    // Opt-in serial/UART console. The service stays installed even when the
    // device cannot be opened so every route can return a typed reason instead
    // of a fabricated buffer or success. No path is opened without the flag.
    let serial_service = match args.serial_device.clone() {
        None => None,
        Some(device) => {
            let parity = match args.serial_parity.parse::<serial::SerialParity>() {
                Ok(parity) => parity,
                Err(detail) => {
                    error!(error = %detail, "invalid --serial-parity");
                    std::process::exit(2);
                }
            };
            let flow_control = match args
                .serial_flow_control
                .parse::<serial::SerialFlowControl>()
            {
                Ok(flow_control) => flow_control,
                Err(detail) => {
                    error!(error = %detail, "invalid --serial-flow-control");
                    std::process::exit(2);
                }
            };
            let mut serial_config = match serial::SerialConfig::new(
                device,
                args.serial_baud,
                args.serial_data_bits,
                parity,
                args.serial_stop_bits,
                Duration::from_millis(args.serial_timeout_ms),
            ) {
                Ok(config) => config,
                Err(error) => {
                    error!(
                        code = error.code(),
                        error = %error,
                        remediation = error.remediation(),
                        "invalid serial configuration"
                    );
                    std::process::exit(2);
                }
            };
            serial_config.buffer_bytes = args.serial_buffer_bytes;
            serial_config.flow_control = flow_control;
            serial_config.initial_dtr = args.serial_dtr;
            serial_config.initial_rts = args.serial_rts;
            serial_config.control_enabled = args.serial_control;
            serial_config.reconnect = serial::SerialReconnectConfig {
                max_attempts: args.serial_reconnect_attempts,
                min_delay: Duration::from_millis(args.serial_reconnect_min_ms),
                max_delay: Duration::from_millis(args.serial_reconnect_max_ms),
            };
            if let Err(error) = serial_config.validate() {
                error!(
                    code = error.code(),
                    error = %error,
                    remediation = error.remediation(),
                    "invalid serial configuration"
                );
                std::process::exit(2);
            }
            let service = serial::SerialService::new(serial_config);
            match service.start() {
                Ok(()) => info!(
                    device = %service.device().display(),
                    baud = args.serial_baud,
                    flow_control = flow_control.as_str(),
                    control_enabled = args.serial_control,
                    reconnect_attempts = args.serial_reconnect_attempts,
                    "serial provider opened; /serial/status|write|read|stream enabled"
                ),
                Err(error) => warn!(
                    code = error.code(),
                    error = %error,
                    remediation = error.remediation(),
                    "serial device was configured but could not be opened; /serial/* reports the typed failure and retries the same path"
                ),
            }
            Some(service)
        }
    };
    let serial_available = serial_service
        .as_ref()
        .is_some_and(|service| service.status().connected);
    set_capability(&mut args, &mut config, "serial", serial_available);
    // Opt-in low-speed buses. Nothing is probed or opened without an explicit
    // allowlist; the default state keeps every route at typed 503.
    let bus_config = match ozma_node::buses::BusConfig::from_cli_all(
        args.onewire.as_deref(),
        &args.i2c_allow,
        &args.spi_allow,
        &args.gpio_allow,
        &args.can_allow,
        &args.can_send_allow,
        &args.iio_allow,
        args.iio_root.as_deref(),
        &args.pwm_allow,
        args.pwm_root.as_deref(),
        &args.ddc_allow,
        args.i2c_pec,
        args.bus_max_transfer_bytes,
        args.bus_max_block_bytes,
        args.bus_max_devices,
        args.can_max_frames,
    ) {
        Ok(config) => config,
        Err(error) => {
            error!(
                code = error.code(),
                error = %error,
                remediation = error.remediation(),
                "invalid bus configuration"
            );
            std::process::exit(2);
        }
    };
    let bus_state = match ozma_node::buses::BusState::new(bus_config) {
        Ok(state) => state,
        Err(error) => {
            error!(
                code = error.code(),
                error = %error,
                remediation = error.remediation(),
                "invalid bus configuration"
            );
            std::process::exit(2);
        }
    };
    let bus_status = bus_state.status();
    let buses_configured = bus_status.onewire.configured
        || bus_status.i2c.configured
        || bus_status.spi.configured
        || bus_status.gpio.configured
        || bus_status.can.configured
        || bus_status.iio.configured
        || bus_status.pwm.configured
        || bus_status.ddc.configured;
    set_capability(&mut args, &mut config, "buses", buses_configured);
    // Opt-in application control. Nothing is signalled unless this flag is
    // present; without it the routes stay mounted and return a typed 503.
    let app_control = args.app_control.then(|| {
        let control =
            ozma_node::app_control::AppControl::new(ozma_node::app_control::AppControlConfig::new(
                Duration::from_secs(args.app_control_kill_after_seconds),
                args.app_control_allow_other_users,
            ));
        let policy = control.config();
        info!(
            kill_after_seconds = policy.kill_after.as_secs(),
            allow_other_users = policy.allow_other_users,
            "application control enabled; /apps/<pid> control routes are guarded"
        );
        control
    });
    // Keep a handle for shutdown: a clipboard selection owner or persistent
    // wallpaper daemon must not outlive the node.
    let desktop_controls_shutdown = desktop_controls.clone();
    // Live `/video?codec=h264|hevc` encoder configuration. It reuses the HLS
    // encoder/backend preference and ffmpeg/device settings even when `--hls`
    // is not enabled, because this path writes MPEG-TS directly and needs no
    // HLS output directory.
    let video_encoding = if config.has_capability("video") {
        let template = match hls_config_for_capabilities.clone() {
            Some(config) => config,
            None => {
                let encoder = HlsEncoder::parse(&args.hls_encoder).unwrap_or_else(|error| {
                    warn!(
                        error = %error,
                        "Invalid --hls-encoder for live /video; using the software H.264 encoder"
                    );
                    HlsEncoder::H264
                });
                HlsConfig {
                    fps: args.hls_fps,
                    bitrate_kbps: args.hls_bitrate_kbps,
                    video_encoder: encoder,
                    ffmpeg_bin: args.ffmpeg_bin.clone(),
                    vaapi_device: config.vaapi_device.clone(),
                    nvenc_gpu: config.nvenc_gpu,
                    ..HlsConfig::default()
                }
            }
        };
        Some(ozma_node::video::VideoEncoding::system(template))
    } else {
        None
    };
    let compat_state =
        controller_compat::ConnectionState::new(connection_info, compat_stats.clone())
            .with_power(power)
            .with_hls(Some(hls_status))
            .with_hls_controller(hls_controller.clone())
            .with_input_sink(Some(input_sink.clone()))
            .with_keyboard_layout(args.keyboard_layout)
            .with_codecs(codec_status)
            .with_codec_control(codec_control)
            .with_audio(audio_control)
            .with_usb(usb_control)
            .with_webrtc(webrtc_control.clone())
            .with_video_encoding(video_encoding)
            .with_snapshot(Some({
                let control = SnapshotControl::from_shared(registry_slot.clone());
                match capture_source_selection(&config) {
                    Some(source_id) => control.with_source_id(source_id),
                    None => control,
                }
            }))
            .with_display_outputs(display_outputs_value, display_capture)
            .with_display_control(display_control)
            .with_console_broker(console_broker.clone())
            .with_multi_console_hls(multi_console_hls.clone())
            .with_multi_console_webrtc(multi_console_webrtc.clone())
            .with_capture(capture_status)
            .with_hardware(hardware_state.clone())
            .with_vnc_registration(vnc_registration)
            .with_desktop_controls(desktop_controls)
            .with_backup(backup_service)
            .with_bluetooth(bluetooth_service)
            .with_serial(serial_service.clone())
            .with_buses(bus_state)
            .with_app_control(app_control);
    // Control routes stay behind the bearer guard; the controller's read-only
    // probes are mounted after it so the unmodified controller works with
    // `--api-token`. See `controller_compat::{compat_router, public_router}`.
    let thermal_options = ozma_node::thermal::ThermalOptions {
        policy_path: args.thermal_config.clone(),
        control: args.thermal_control,
        interval_secs: args.thermal_interval,
        hwmon_root: std::path::PathBuf::from(&args.thermal_hwmon_root),
    };
    let thermal_policy = args.thermal_config.as_deref().and_then(|path| {
        match ozma_node::thermal::load_policy(path) {
            Ok(policy) => Some(Arc::new(policy)),
            Err(error) => {
                error!(error = %error, "thermal policy could not be loaded");
                None
            }
        }
    });
    let thermal_status = Arc::new(RwLock::new(ThermalStatus::default()));
    let thermal_router = ozma_node::thermal::router(thermal_status.clone(), thermal_policy);
    let hardware_router = ozma_node::hardware::router(hardware_state.clone());
    let compat_router = controller_compat::compat_router(compat_state.clone())
        .merge(thermal_router)
        .merge(hardware_router);

    if args.aida64_port != 0 {
        let aida_bind = args.aida64_bind.clone();
        let aida_port = args.aida64_port;
        let aida_hardware = hardware_state.clone();
        let aida_shutdown = shutdown.clone();
        background.push((
            "aida64",
            tokio::spawn(async move {
                if let Err(error) =
                    ozma_node::aida64::serve(&aida_bind, aida_port, aida_hardware, aida_shutdown)
                        .await
                {
                    error!(error = %error, "AIDA64 RemoteSensor listener exited");
                }
            }),
        ));
    }

    // Opt-in mesh service. It is merged into the guarded compatibility router,
    // so `/api/v1/connect/*` uses the same bearer-token guard as the other
    // control routes. Without `--mesh` no service exists and no socket is bound.
    let mesh_service = build_mesh_service(&args);
    let compat_router = compat_router.merge(mesh_service::router(mesh_service.clone()));
    if let Some(service) = mesh_service.clone() {
        match service.state() {
            mesh_service::MeshState::Unsupported => {
                if let Some(error) = service.error() {
                    warn!(
                        code = %error.code,
                        reason = %error.reason,
                        "Mesh service is configured but unsupported; /api/v1/connect/* \
                         will return a typed unsupported document"
                    );
                }
            }
            _ => match service.start().await {
                Ok(()) => {
                    let wireguard = service
                        .identity()
                        .map(|identity| identity.wireguard)
                        .unwrap_or_else(|| "unavailable".to_owned());
                    info!(%wireguard, "Mesh service started");
                }
                Err(error) => warn!(
                    code = %error.code,
                    reason = %error.reason,
                    "Mesh service failed to start; /api/v1/connect/* reports the runtime state"
                ),
            },
        }
        let mesh_shutdown = shutdown.clone();
        background.push((
            "mesh-service",
            tokio::spawn(async move {
                mesh_shutdown.cancelled().await;
                if let Err(error) = service.stop().await {
                    warn!(
                        code = %error.code,
                        reason = %error.reason,
                        "Mesh service shutdown was not clean"
                    );
                }
            }),
        ));
    }

    let public_compat_router = controller_compat::public_router(compat_state);

    // Read-only persisted multiseat report. This loads `--seats-file` once; it
    // never starts or stops a seat. The standalone `ozma-multiseat` supervisor
    // owns process lifecycle, so the route is merged next to the other public
    // read-only probes and reports `lifecycle:"external"`.
    let seat_report = if args.multiseat {
        let path = args
            .seats_file
            .as_ref()
            .expect("clap requires --seats-file with --multiseat");
        let seat_id = registration::seat_id_from_env();
        let discovered = multiseat_display_names(&config);
        let report = ozma_node::seats::SeatReportState::load(path, seat_id, &discovered);
        let summary = report.summary();
        match &summary.code {
            Some(code) => warn!(
                mode = summary.mode,
                code = %code,
                path = %path.display(),
                "Multiseat seat report could not load the persisted store; \
                 /api/v1/seats will return a typed error"
            ),
            None => info!(
                mode = summary.mode,
                seats = summary.seat_count,
                path = %path.display(),
                "Multiseat seat report enabled (read-only; lifecycle stays external)"
            ),
        }
        report
    } else {
        if args.seats_file.is_some() {
            warn!(
                "--seats-file has no effect without --multiseat; /api/v1/seats stays not-configured"
            );
        }
        ozma_node::seats::SeatReportState::not_configured()
    };
    let public_compat_router = public_compat_router.merge(ozma_node::seats::router(seat_report));

    // Thermal sampling runs read-only unless a policy and --thermal-control are
    // both present. It restores the original hwmon automatic mode on shutdown.
    let thermal_shutdown = shutdown.clone();
    background.push((
        "thermal",
        tokio::spawn(async move {
            ozma_node::thermal::run(thermal_options, thermal_status, thermal_shutdown).await;
        }),
    ));

    // Idle console sources must not stay open forever when no snapshot is
    // requested. The timer uses the same shutdown token as the other tasks.
    if let Some(broker) = console_broker.clone() {
        let broker_shutdown = shutdown.clone();
        background.push((
            "display-broker-idle",
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = broker_shutdown.cancelled() => break,
                        _ = ticker.tick() => {
                            let evicted =
                                broker.evict_idle(std::time::Instant::now()).await;
                            if evicted > 0 {
                                info!(
                                    evicted,
                                    "display broker evicted idle console sources"
                                );
                            }
                        }
                    }
                }
                broker.shutdown().await;
            }),
        ));
    }

    if let Some(manager) = multi_console_hls.clone() {
        let manager_shutdown = shutdown.clone();
        background.push((
            "multi-console-hls-idle",
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = manager_shutdown.cancelled() => break,
                        _ = ticker.tick() => {
                            let evicted = manager.evict_idle(std::time::Instant::now()).await;
                            if evicted > 0 {
                                info!(
                                    evicted,
                                    "per-console HLS evicted idle non-active pipelines"
                                );
                            }
                        }
                    }
                }
                manager.shutdown().await;
            }),
        ));
    }

    if let Some(manager) = multi_console_webrtc.clone() {
        let manager_shutdown = shutdown.clone();
        background.push((
            "multi-console-webrtc-idle",
            tokio::spawn(async move {
                let mut ticker = tokio::time::interval(Duration::from_secs(5));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    tokio::select! {
                        _ = manager_shutdown.cancelled() => break,
                        _ = ticker.tick() => {
                            let evicted = manager.evict_idle(std::time::Instant::now()).await;
                            if evicted > 0 {
                                info!(
                                    evicted,
                                    "per-console WebRTC evicted idle non-active helpers"
                                );
                            }
                        }
                    }
                }
                manager.shutdown().await;
            }),
        ));
    }

    // Start the controller-facing UDP HID listener only when the node
    // advertises the `hid` capability.
    if config.has_capability("hid") {
        match controller_compat::HidUdpListener::bind(
            config.hid_udp_port,
            input_sink.clone(),
            compat_stats.clone(),
        )
        .await
        {
            Ok(listener) => {
                let actual_port = listener
                    .local_addr()
                    .map(|address| address.port())
                    .unwrap_or(config.hid_udp_port);
                info!(port = actual_port, "UDP HID compatibility listener running");
                let hid_shutdown = shutdown.clone();
                background.push((
                    "udp-hid",
                    tokio::spawn(async move {
                        tokio::select! {
                            _ = hid_shutdown.cancelled() => {}
                            result = listener.run() => {
                                if let Err(error) = result {
                                    error!("UDP HID listener exited: {error:#}");
                                }
                            }
                        }
                    }),
                ));
            }
            Err(error) => {
                error!(
                    port = config.hid_udp_port,
                    error = %error,
                    "UDP HID listener disabled: binding the HID UDP port failed"
                );
                compat_stats.record_listener_error(&error);
            }
        }
    } else {
        info!("HID capability not advertised; UDP HID listener disabled");
    }

    match config.profile {
        NodeProfile::Hardware => {}
        NodeProfile::Vm => {
            let capture = vm_providers.capture.as_deref().unwrap_or("none");
            let power = vm_providers
                .power
                .map(PowerProvider::as_str)
                .unwrap_or("none");
            warn!(
                profile = %config.profile,
                capture = %capture,
                input = ?vm_providers.input_chain_names(),
                audio = %config.providers.audio,
                audio_router = %config.audio_router,
                display = %config.providers.display,
                power = %power,
                "Profile 'vm' is PARTIALLY WIRED: QMP/D-Bus/VNC capture, uinput/QMP input \
                 injection, QMP/libvirt power control, and optional PipeWire/PulseAudio routing \
                 start when their flags are configured. Local HLS is opt-in via `--hls` and feeds \
                 `/stream/*` from the first capture source. WebRTC runs only through a configured \
                 helper (`--webrtc-helper` or the opt-in native `--webrtc-native`). POST /codec \
                 validates an override and, when HLS is running, restarts the pipeline with the \
                 mapped encoder; with HLS off it is stored with pending_restart. Still not \
                 implemented: live D-Bus/VNC validation in CI, per-VM provisioning \
                 (ozma-softnode supervisor), and WebRTC audio (native is video-only). Do not treat \
                 WebRTC as functional without a configured helper."
            );
        }
        NodeProfile::Desktop => {
            warn!(
                profile = "desktop",
                capture = %vm_providers.capture.as_deref().unwrap_or("none"),
                capture_target = %config.desktop_capture_target,
                input = ?vm_providers.input_chain_names(),
                audio = %config.providers.audio,
                audio_capture_target = %config.audio_capture_target,
                audio_router = %config.audio_router,
                "Profile 'desktop' runs real capture for the selected transport (PipeWire \
                 --desktop-capture-target, X11 --x11-display, or the XDG Portal ScreenCast \
                 handshake), the uinput -> QMP/HID -> recording input chain, and PipeWire audio \
                 capture. /health, /status, /snapshot (and /display/snapshot), /display/info, \
                 /node/status, /api/v1/connection, and the configured /audio/* routes are \
                 available. /display/geometry, /clipboard, and /wallpaper use xrandr, \
                 xclip/xsel, and feh/xwallpaper/swaybg when present; a missing tool or display \
                 is a typed 503, never a fabricated success. Portal consent runs at startup; a \
                 denial, timeout, or missing portal leaves the node running without capture \
                 (never a fake source). Remaining limits: a compositor that exposes the screen \
                 only through the portal fd cannot be opened by the current PipeWire backend \
                 (typed error), returned portal restore tokens are not persisted, /audio/nodes \
                 is still a typed 501, and desktop sink creation is not implemented. Capture or \
                 input failures downgrade capabilities; the node keeps running rather than \
                 claiming a source it did not open."
            );
        }
    }

    info!(
        name = %config.name,
        node_id = %node_id,
        profile = %config.profile,
        capture_provider = %vm_providers.capture.as_deref().unwrap_or("none"),
        desktop_capture_target = %config.desktop_capture_target,
        input_provider = ?vm_providers.input_chain_names(),
        audio_provider = %config.providers.audio,
        audio_capture_target = %config.audio_capture_target,
        audio_router = %config.audio_router,
        display_provider = %config.providers.display,
        power_provider = %vm_providers.power.map(PowerProvider::as_str).unwrap_or("none"),
        hid_udp_port = config.hid_udp_port,
        http_port = config.http_port,
        cap = %args.cap,
        hw = %config.hw,
        platform = %capabilities.platform,
        "Ozma node starting"
    );

    let txt = build_txt(&args, &local_ip, &registration_extras);

    let mdns_handle = match mdns::advertise_on(
        &config.name,
        config.hid_udp_port,
        &txt,
        local_ip.parse().ok(),
    ) {
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

        // Prepare both controller loops before either spawned future takes
        // ownership of the URL/node-id values.
        let software_url = url.clone();
        let software_host = node_id.to_string();
        let heartbeat_url = url.clone();
        let heartbeat_node_id = node_id.clone();
        let interval = config.heartbeat_interval_secs;
        let heartbeat_shutdown = shutdown.clone();
        background.push((
            "registration-heartbeat",
            tokio::spawn(async move {
                tokio::select! {
                    _ = heartbeat_shutdown.cancelled() => {}
                    _ = registration::heartbeat_loop(
                        &heartbeat_url,
                        heartbeat_node_id.as_str(),
                        interval,
                    ) => {}
                }
            }),
        ));

        // Fleet software inventory. Steam, the Microsoft Store, and native
        // package managers are sources of installed software; the controller
        // stores one latest snapshot per host. Vendor-managed records are
        // reported as external_managed and are never patched in place.
        let software_shutdown = shutdown.clone();
        background.push((
            "software-inventory",
            tokio::spawn(async move {
                let reporter = match InventoryReporter::new(software_url, software_host) {
                    Ok(reporter) => reporter,
                    Err(error) => {
                        error!("Software inventory reporter disabled: {error}");
                        return;
                    }
                };
                reporter
                    .run(DEFAULT_REPORT_INTERVAL, software_shutdown)
                    .await;
            }),
        ));
    }

    if config.has_capability("audio")
        && config.profile != NodeProfile::Desktop
        && !config.usb_uac2_bridge
    {
        let audio_shutdown = shutdown.clone();
        background.push((
            "uac2-bridge",
            tokio::spawn(async move {
                tokio::select! {
                    _ = audio_shutdown.cancelled() => {}
                    _ = async {
                        match usb_audio::UsbAudioGadget::open(true).await {
                            Ok(gadget) => {
                                if let Err(error) = gadget.run_bridge().await {
                                    tracing::error!("UAC2 bridge exited with error: {error:#}");
                                }
                            }
                            Err(error) => tracing::error!("UAC2 gadget init failed: {error:#}"),
                        }
                    } => {}
                }
            }),
        ));
    } else if config.profile == NodeProfile::Desktop {
        info!(
            target = %config.audio_capture_target,
            "Desktop audio uses PipeWire capture; no UAC2 USB gadget is created"
        );
    }

    let http_port = config.http_port;
    let webtransport = if args.webtransport_port == 0 {
        None
    } else {
        match args
            .webtransport_bind
            .parse::<std::net::IpAddr>()
            .map(|ip| std::net::SocketAddr::new(ip, args.webtransport_port))
        {
            Ok(address) => Some(address),
            Err(error) => {
                error!(
                    bind = %args.webtransport_bind,
                    error = %error,
                    "Invalid --webtransport-bind; the WebTransport listener is disabled"
                );
                None
            }
        }
    };
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
    let api_capture_registry = capture_registry.clone();
    let api_multi_console_hls = multi_console_hls.clone();
    let api_shutdown = shutdown.clone();
    background.push((
        "node-api",
        tokio::spawn(async move {
            if let Err(error) = api::serve_with_scene_replay_and_sink(
                http_port,
                quic_port,
                api_capabilities,
                consent_policy,
                scene_replay,
                api_token,
                config.rtsp_port,
                webtransport,
                Some(input_sink),
                Some(compat_router),
                Some(public_compat_router),
                Some(api_capture_registry),
                hls_output_dir,
                api_multi_console_hls,
                api_shutdown,
            )
            .await
            {
                error!("Node API exited: {error:#}");
            }
        }),
    ));

    let signal = shutdown::wait_for_signal().await;
    info!(signal, "shutdown requested; stopping node tasks");
    // Stop accepting new work first. Every long-lived listener selects on this
    // token, so cancellation closes its socket instead of waiting for the next
    // packet or poll timeout.
    shutdown.cancel();
    if let Some(service) = serial_service.as_ref() {
        service.stop();
    }

    if let Some(controller) = hls_controller.as_ref() {
        if shutdown::with_timeout("hls-stop", shutdown::TASK_STOP_TIMEOUT, controller.stop())
            .await
            .is_none()
        {
            warn!("HLS pipeline did not stop within its bound");
        }
    }
    if let Some(manager) = multi_console_hls.as_ref() {
        if shutdown::with_timeout(
            "multi-console-hls-stop",
            shutdown::TASK_STOP_TIMEOUT,
            manager.shutdown(),
        )
        .await
        .is_none()
        {
            warn!("per-console HLS pipelines did not stop within their bound");
        }
    }
    if let Some(manager) = multi_console_webrtc.as_ref() {
        if shutdown::with_timeout(
            "multi-console-webrtc-stop",
            shutdown::TASK_STOP_TIMEOUT,
            manager.shutdown(),
        )
        .await
        .is_none()
        {
            warn!("per-console WebRTC helpers did not stop within their bound");
        }
    }
    if let Some(control) = webrtc_control.as_ref() {
        if shutdown::with_timeout(
            "webrtc-stop",
            shutdown::TASK_STOP_TIMEOUT,
            control.shutdown(),
        )
        .await
        .is_none()
        {
            warn!("WebRTC helper did not stop within its bound");
        }
    }
    if let Some(broker) = console_broker.as_ref() {
        if shutdown::with_timeout(
            "display-broker-stop",
            shutdown::TASK_STOP_TIMEOUT,
            broker.shutdown(),
        )
        .await
        .is_none()
        {
            warn!("display snapshot broker did not stop within its bound");
        }
    }
    // Stop the userspace media export last so its overlay is flushed according
    // to `persist_on_stop`. A failure is logged; shutdown still proceeds.
    if let Some(control) = usb_media_shutdown.as_ref() {
        if let Some(stats) = control.shutdown_usbip() {
            info!(
                sessions = stats.sessions,
                imports = stats.imports,
                "USB/IP listener stopped"
            );
        }
        match control.shutdown_media_export() {
            Ok(()) => info!("userspace SCSI media export stopped; persistence policy applied"),
            Err(error) => warn!(
                kind = error.kind(),
                error = %error,
                "userspace SCSI media export did not stop cleanly during shutdown"
            ),
        }
    }

    // Stop the API/QUIC/HID/heartbeat tasks. `stop_task` aborts after the
    // cooperative window, so a task that never yields cannot hold exit.
    for (name, task) in background {
        shutdown::stop_task(task, name).await;
    }

    // Close the shared QMP control socket only after the pipelines stopped
    // using it. This is best-effort and bounded: QEMU may already be gone.
    if let Some(shared) = shared_qmp.as_ref() {
        let _ = shutdown::with_timeout(
            "qmp-disconnect",
            shutdown::TASK_STOP_TIMEOUT,
            shared.disconnect(),
        )
        .await;
    }

    // Terminate the X11 clipboard selection owner and any persistent wallpaper
    // process this node started. Best-effort and idempotent.
    if let Some(controls) = desktop_controls_shutdown.as_ref() {
        controls.shutdown();
    }

    // Dropping the responder unregisters the mDNS record. It is a std thread
    // with its own runtime, so it cannot block process exit even if it stalls.
    drop(mdns_handle);

    info!("Ozma node shutting down");
    std::process::ExitCode::SUCCESS
}

/// Default HLS output directory, with a filesystem-safe node name.
fn default_hls_dir(name: &str) -> std::path::PathBuf {
    let safe: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    std::path::PathBuf::from("/tmp").join(format!("ozma-stream-{safe}"))
}

/// Open the registry source for a helper/pipeline.
///
/// This uses the same registry as the VM/hardware profile: a VM node streams
/// its QMP display and a hardware node streams its V4L2/PipeWire source. When
/// `requested` is set (`--capture-device`), only an exact source id/name match
/// is opened; a missing device fails instead of falling back to the first
/// camera.
fn open_capture_source(
    registry: &ozma_capture::CaptureRegistry,
    requested: Option<&str>,
) -> Result<Box<dyn ozma_media::CaptureSource>, String> {
    ozma_node::display_control::open_selected_source(registry, requested)
}

/// The explicit `--capture-device` source selector, normalized for comparison.
///
/// `--capture-device` is a host-device selector (V4L2 path/name); a VM node's
/// capture source is its QMP/VNC/D-Bus display, not a device path.
fn capture_source_selection(config: &NodeConfig) -> Option<String> {
    (config.profile != NodeProfile::Vm)
        .then(|| config.capture_device.as_deref())
        .flatten()
        .map(str::trim)
        .filter(|id| !id.is_empty())
        .map(str::to_owned)
}

/// Default WebRTC signaling socket. `/tmp` is writable without extra
/// privileges; the helper path is per-process so two nodes do not collide.
fn default_webrtc_socket(name: &str) -> std::path::PathBuf {
    let safe: String = name
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect();
    std::env::temp_dir().join(format!("ozma-webrtc-{safe}-{}.sock", std::process::id()))
}

/// Default shared-memory frame path. `/dev/shm` is a tmpfs on Linux; fall back
/// to the temp directory on systems that do not provide it.
fn default_webrtc_shm() -> std::path::PathBuf {
    let base = if std::path::Path::new("/dev/shm").is_dir() {
        std::path::PathBuf::from("/dev/shm")
    } else {
        std::env::temp_dir()
    };
    base.join(format!("ozma-webrtc-fb-{}", std::process::id()))
}

/// Build the optional USB gadget control.
///
/// Returns `None` unless `--usb-gadget` is set. When enabled it requires a UDC
/// and an existing ConfigFS root; any typed failure is logged and represented
/// as a failed control so `/usb` can report it without crashing the node. The
/// gadget is only advertised after `create_and_bind` succeeds.
fn build_usb_control(config: &NodeConfig) -> Option<controller_compat::UsbControl> {
    if !config.usb_gadget {
        return None;
    }
    let gadget_config = match usb_gadget_config(config) {
        Ok(config) => config,
        Err(error) => {
            warn!(
                kind = error.kind(),
                error = %error,
                "USB gadget unavailable: invalid configuration; no gadget was created"
            );
            return Some(controller_compat::UsbControl::failed(format!(
                "{}: {error}",
                error.kind()
            )));
        }
    };
    if !ConfigFsUsbController::probe(&config.usb_configfs_root) {
        let error = UsbError::MissingRoot(config.usb_configfs_root.clone());
        warn!(
            kind = error.kind(),
            root = %config.usb_configfs_root.display(),
            error = %error,
            "USB gadget unavailable: ConfigFS root is missing; no gadget was created"
        );
        return Some(controller_compat::UsbControl::failed(format!(
            "{}: {error}",
            error.kind()
        )));
    }

    let controller = Arc::new(ConfigFsUsbController::new(config.usb_configfs_root.clone()));
    if let Some(mountpoint) = config.usb_functionfs_mount.clone() {
        let instance = gadget_config
            .functions
            .iter()
            .find_map(|function| match function {
                UsbFunction::Vendor { config } => Some(config.instance.clone()),
                _ => None,
            })
            .unwrap_or_else(|| "vendor0".to_owned());
        let mut handoff = match FunctionFsHandoff::new(
            controller.clone(),
            DEFAULT_USB_GADGET_NAME,
            instance,
            gadget_config.udc.clone(),
            mountpoint.clone(),
        ) {
            Ok(handoff) => handoff.with_mounter(CommandFunctionFsMounter::new()),
            Err(error) => {
                warn!(
                    kind = error.kind(),
                    error = %error,
                    mount = %mountpoint.display(),
                    "USB FunctionFS handoff is invalid; no gadget was created"
                );
                return Some(controller_compat::UsbControl::failed(format!(
                    "{}: {error}",
                    error.kind()
                )));
            }
        };
        if let Err(error) = handoff.prepare(&gadget_config) {
            warn!(
                kind = error.kind(),
                error = %error,
                "USB gadget unavailable: FunctionFS prepare failed and was rolled back"
            );
            return Some(controller_compat::UsbControl::failed(format!(
                "{}: {error}",
                error.kind()
            )));
        }
        if let Err(error) = handoff.mount() {
            warn!(
                kind = error.kind(),
                error = %error,
                mount = %mountpoint.display(),
                "USB FunctionFS mount failed after function creation and was rolled back"
            );
            return Some(controller_compat::UsbControl::failed(format!(
                "{}: {error}",
                error.kind()
            )));
        }
        info!(
            gadget = DEFAULT_USB_GADGET_NAME,
            udc = gadget_config.udc.as_str(),
            mount = %mountpoint.display(),
            "USB FunctionFS function created and mounted; waiting for the daemon before UDC bind"
        );
        let control =
            controller_compat::UsbControl::configured(controller, DEFAULT_USB_GADGET_NAME);
        control.set_functionfs_handoff(Some(handoff));
        return Some(control);
    }
    match controller.create_and_bind(&gadget_config) {
        Ok(status) => {
            info!(
                gadget = %status.name,
                udc = status.udc.as_deref().unwrap_or("none"),
                functions = status.functions.len(),
                "USB gadget created and bound"
            );
            Some(controller_compat::UsbControl::configured(
                controller,
                DEFAULT_USB_GADGET_NAME,
            ))
        }
        Err(error) => {
            warn!(
                kind = error.kind(),
                gadget = DEFAULT_USB_GADGET_NAME,
                udc = config.usb_udc.as_deref().unwrap_or("none"),
                error = %error,
                "USB gadget unavailable: create/bind failed; no USB capability is advertised"
            );
            Some(controller_compat::UsbControl::failed(format!(
                "{}: {error}",
                error.kind()
            )))
        }
    }
}

/// Build and start the bounded USB/IP listener.
///
/// The default adapter set is the typed `usbip-kernel-required` pair, so a
/// successful import is never fabricated. `--usbip-linux-adapters` explicitly
/// selects the argv/sysfs host adapter and the TCP URB data adapter.
fn build_usbip_runtime(
    config: &NodeConfig,
) -> Result<Option<controller_compat::UsbIpRuntime>, UsbError> {
    if !config.usbip {
        return Ok(None);
    }
    let daemon_config = UsbIpDaemonConfig::server(config.usbip_host.clone(), config.usbip_port);
    let (daemon, data, adapter): (
        Arc<UsbIpDaemon>,
        Arc<dyn UsbIpKernelDataEndpoint>,
        &'static str,
    ) = if config.usbip_linux_adapters {
        let sysfs = config
            .usbip_sysfs_root
            .clone()
            .map(UsbIpKernelSysfs::with_root)
            .unwrap_or_else(UsbIpKernelSysfs::system);
        let program = config
            .usbip_program
            .clone()
            .unwrap_or_else(|| ozma_usb::DEFAULT_USBIP_PROGRAM.to_owned());
        let backend = LinuxUsbIpHostBackend::new(SystemCommandRunner, program, sysfs);
        (
            Arc::new(UsbIpDaemon::new(daemon_config)?.with_kernel(Arc::new(backend))),
            Arc::new(LinuxUsbIpKernelDataEndpoint::system()),
            "linux",
        )
    } else {
        (
            Arc::new(UsbIpDaemon::new(daemon_config)?),
            Arc::new(NoUsbIpKernelDataEndpoint),
            "kernel-required",
        )
    };
    daemon.start()?;
    let server = UsbIpServer::bind_tcp(
        (config.usbip_host.as_str(), config.usbip_port),
        daemon,
        UsbIpServerConfig::default(),
    )?
    .with_data_endpoint(data);
    let handle = server.start()?;
    info!(
        host = config.usbip_host.as_str(),
        port = config.usbip_port,
        adapter,
        "USB/IP control/data listener started"
    );
    Ok(Some(controller_compat::UsbIpRuntime::new(
        handle,
        adapter,
        config.usbip_host.clone(),
        config.usbip_port,
    )))
}

/// Build the opt-in userspace media auto-start configuration.
///
/// `None` when `--usb-media-export` was not supplied. This is separate from
/// the ConfigFS kernel LUN built by [`usb_gadget_config`].
fn usb_media_auto_start_config(
    config: &NodeConfig,
) -> Option<controller_compat::UsbMediaAutoStartConfig> {
    let path = config.usb_media_export.clone()?;
    let normalized = config.usb_media_export_format.trim().to_ascii_lowercase();
    let format = match normalized.as_str() {
        "fat32" | "vfat" => MediaFormat::Fat32,
        "iso" | "iso9660" => MediaFormat::Iso9660,
        _ => MediaFormat::Raw,
    };
    let read_only = !config.usb_media_export_read_write
        || matches!(format, MediaFormat::Fat32 | MediaFormat::Iso9660);
    Some(controller_compat::UsbMediaAutoStartConfig {
        path,
        format,
        read_only,
        persist_path: if read_only {
            None
        } else {
            config.usb_media_export_persist.clone()
        },
        persist_on_flush: config.usb_media_export_persist_on_flush,
        persist_on_stop: config.usb_media_export_persist_on_stop,
        reload: config.usb_media_export_reload,
        timeout_ms: config.usb_media_export_timeout_ms,
        max_transfer_bytes: config.usb_media_export_max_transfer_bytes,
        restart: MediaRestartPolicy {
            max_attempts: config.usb_media_restart_max_attempts,
            backoff_ms: config.usb_media_restart_backoff_ms,
            max_backoff_ms: config.usb_media_restart_max_backoff_ms,
            reload_overlay: config.usb_media_restart_reload_overlay,
        },
        functionfs_mount: config.usb_functionfs_mount.clone(),
    })
}

/// Build the composite gadget description for `--usb-gadget`.
fn usb_gadget_config(config: &NodeConfig) -> Result<UsbGadgetConfig, UsbError> {
    let udc = config
        .usb_udc
        .as_deref()
        .map(str::trim)
        .filter(|udc| !udc.is_empty())
        .ok_or_else(|| UsbError::InvalidField {
            field: "udc",
            reason: "--usb-gadget requires --usb-udc <udc>; no controller is guessed".to_owned(),
        })?;
    let mut gadget = UsbGadgetConfig::new(DEFAULT_USB_GADGET_NAME, udc)
        .with_function(UsbFunction::keyboard("kbd"))
        .with_function(UsbFunction::mouse("mouse"));
    if let Some(path) = config.usb_media.as_ref() {
        let image = if config.usb_media_cdrom {
            MassStorageImage::cdrom(path)
        } else {
            MassStorageImage::disk(path).with_read_only(config.usb_media_ro)?
        };
        gadget = gadget.with_function(UsbFunction::mass_storage(
            DEFAULT_USB_MASS_STORAGE_INSTANCE,
            Some(image),
        ));
    }
    if config.usb_uac2 {
        // Descriptors and ConfigFS attributes only. No ALSA bridge is started;
        // status reports `bridge: unsupported`.
        gadget = gadget.with_function(UsbFunction::uac2("audio", 2, 1, 48_000, 16));
    }
    if config.usb_vendor {
        // The userspace media daemon speaks SCSI Bulk-Only Transport on this
        // FunctionFS interface, so advertise Mass Storage / SCSI transparent /
        // Bulk-Only when it is the export path. A plain vendor function stays
        // class 0xff.
        let (class, subclass, protocol) =
            if config.usb_functionfs_mount.is_some() && config.usb_media_export.is_some() {
                (0x08, 0x06, 0x50)
            } else {
                (0xff, 0x00, 0x00)
            };
        let mut vendor = FunctionFsConfig::new("vendor0", 0x1d6b, 0x0106)
            .with_interface(class, subclass, protocol)
            .with_endpoint(ozma_usb::FunctionFsEndpoint::bulk_out(1, 512))
            .with_endpoint(ozma_usb::FunctionFsEndpoint::bulk_in(2, 512));
        if let Some(daemon) = config.usb_vendor_daemon.as_ref() {
            vendor = vendor.with_daemon(FunctionFsDaemonSpec::new(daemon));
        }
        gadget = gadget.with_function(UsbFunction::vendor_with_config(vendor));
    }
    Ok(gadget)
}

/// Add or remove a capability from both the parsed config and the TXT/args copy.
fn set_capability(args: &mut Args, config: &mut NodeConfig, capability: &str, enabled: bool) {
    if enabled {
        if !config.has_capability(capability) {
            config.capabilities.push(capability.to_owned());
        }
        let advertised: Vec<&str> = args.cap.split(',').map(str::trim).collect();
        if !advertised.contains(&capability) {
            if !args.cap.trim().is_empty() {
                args.cap.push(',');
            }
            args.cap.push_str(capability);
        }
    } else {
        config
            .capabilities
            .retain(|value| value.trim() != capability);
        let remaining: Vec<&str> = args
            .cap
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty() && *value != capability)
            .collect();
        args.cap = remaining.join(",");
    }
}

/// Build the optional power backend. A connection failure is logged and leaves
/// power unavailable; it must never crash the node.
async fn build_power(
    vm: &VmProviderConfig,
    shared: Option<SharedQmpClient>,
) -> Option<controller_compat::SharedPower> {
    match vm.power {
        None => None,
        Some(PowerProvider::Qmp) => {
            if let Some(shared) = shared {
                info!(
                    socket = %shared.config().control_path.display(),
                    "QMP power backend uses the shared control client"
                );
                return Some(shared_power(QmpPower::with_shared_client(shared)));
            }
            let Some(qmp) = vm.qmp_control_config() else {
                warn!("QMP power selected without a control socket; power is unavailable");
                return None;
            };
            match QmpPower::connect(qmp).await {
                Ok(power) => {
                    info!("QMP power backend connected");
                    Some(shared_power(power))
                }
                Err(error) => {
                    warn!(
                        error = %error,
                        "QMP power backend unavailable; /power/* will report not-configured"
                    );
                    None
                }
            }
        }
        Some(PowerProvider::Libvirt) => {
            let Some(domain) = vm.libvirt_domain.as_deref() else {
                warn!("libvirt power selected without a domain; power is unavailable");
                return None;
            };
            let mut power = LibvirtPower::new(domain);
            if let Some(uri) = vm.libvirt_uri.as_deref() {
                power = power.with_uri(uri);
            }
            match power.connect().await {
                Ok(()) => {
                    info!(
                        domain,
                        uri = vm.libvirt_uri.as_deref().unwrap_or("<virsh-default>"),
                        "libvirt power backend connected"
                    );
                    Some(shared_power(power))
                }
                Err(error) => {
                    // Keep the backend attached so /power/state and actions
                    // report the real reason (missing domain, missing virsh, or
                    // a failed connection) instead of silently not-configured.
                    warn!(
                        domain,
                        uri = vm.libvirt_uri.as_deref().unwrap_or("<virsh-default>"),
                        kind = error.kind(),
                        error = %error,
                        "libvirt power backend unavailable; /power/* will report the retained error"
                    );
                    Some(shared_power(UnavailablePower::new("libvirt", &error)))
                }
            }
        }
    }
}

fn shared_power<P>(backend: P) -> controller_compat::SharedPower
where
    P: PowerBackend + 'static,
{
    Arc::new(tokio::sync::Mutex::new(
        Box::new(backend) as Box<dyn PowerBackend>
    ))
}

/// Build the optional audio routing control.
///
/// `none` keeps the honest 501 surface. A configured backend is marked
/// unavailable (never crashed) when `pactl` is missing or `pactl info` cannot
/// reach the audio server; the connection endpoint then reports `failed` plus
/// `audio_error`, and every `/audio/*` route answers a typed 503 without
/// claiming success.
async fn build_audio_control(config: &NodeConfig) -> Option<AudioControl> {
    let kind = config.audio_router;
    if !kind.is_configured() {
        return None;
    }
    let profile_store = config
        .audio_profile_dir
        .clone()
        .map(RoomProfileStore::new)
        .map(Arc::new);
    // A fresh process cannot own a filter-chain child from an earlier run. Clear
    // any stale active record before a new apply can write one; the child itself
    // disappears when its PipeWire client disconnects.
    if let Some(store) = &profile_store {
        if let Err(error) = store.clear_active() {
            warn!(error = %error, "could not clear the stale active room-EQ state");
        }
    }
    if !command_on_path("pactl") {
        return Some(
            AudioControl::failed(format!(
                "`pactl` was not found on PATH; {kind} audio routing requires \
                 pulseaudio-utils or pipewire-pulse"
            ))
            .with_profile_store(profile_store),
        );
    }
    if let Err(error) = probe_audio_server().await {
        return Some(
            AudioControl::failed(format!("{kind} audio server is unavailable: {error}"))
                .with_profile_store(profile_store),
        );
    }

    let router_config = RouterConfig {
        track_root: config.audio_track_root.clone(),
    };
    let runner = Arc::new(TokioCommandRunner::new());
    let router: Arc<dyn AudioRouter> = match kind {
        AudioRouterKind::PipeWire => {
            Arc::new(PipeWireAudioRouter::with_config(runner, router_config))
        }
        AudioRouterKind::PulseAudio => {
            Arc::new(PulseAudioAudioRouter::with_config(runner, router_config))
        }
        AudioRouterKind::None => return None,
    };
    Some(
        AudioControl::configured(router, config.audio_sink.clone(), kind.as_str())
            .with_profile_store(profile_store),
    )
}

/// Ask `pactl` for server information with a bounded timeout.
///
/// This is a health check, not a routing command: it changes nothing and is
/// only run when `--audio-router` is configured.
async fn probe_audio_server() -> Result<(), String> {
    const PROBE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);
    let output = tokio::time::timeout(
        PROBE_TIMEOUT,
        tokio::process::Command::new("pactl").arg("info").output(),
    )
    .await
    .map_err(|_| "`pactl info` timed out".to_owned())?
    .map_err(|error| format!("`pactl info` could not run: {error}"))?;
    if output.status.success() {
        return Ok(());
    }
    let stderr = String::from_utf8_lossy(&output.stderr);
    let detail = probe_failure_detail(&output.status, &stderr);
    Err(format!("`pactl info` failed with {detail}"))
}

/// Render a failed `pactl info` probe without repeating its status prefix.
///
/// `ExitStatus` already displays as `exit status: 1`, so prepending
/// `exit status` produced `exit status exit status: 1` in
/// `/api/v1/connection.audio_error` and the typed 503 body. stderr is
/// collapsed to one line because it is embedded in JSON.
fn probe_failure_detail(status: &impl std::fmt::Display, stderr: &str) -> String {
    let stderr = stderr.split_whitespace().collect::<Vec<_>>().join(" ");
    if stderr.is_empty() {
        format!("{status}")
    } else {
        format!("{status}: {stderr}")
    }
}

/// Whether an executable named `name` exists on `PATH`.
fn command_on_path(name: &str) -> bool {
    use std::os::unix::fs::PermissionsExt;

    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|directory| {
        let candidate = directory.join(name);
        std::fs::metadata(&candidate)
            .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .unwrap_or(false)
    })
}

/// Display outputs this node can honestly observe for the seat report.
///
/// Only configured display outputs and the explicit multiseat ownership
/// environment qualify. The node does not run `xrandr` here, so an empty result
/// means "no discovery", not "every persisted display is missing". Output names
/// are sorted and deduplicated for a stable report.
fn multiseat_display_names(config: &NodeConfig) -> Vec<String> {
    let mut names: Vec<String> = config
        .display_outputs
        .iter()
        .map(|output| output.name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .collect();
    if let Ok(output) = std::env::var(ozma_multiseat::OZMA_X11_OUTPUT_ENV) {
        let output = output.trim();
        if !output.is_empty() {
            names.push(output.to_owned());
        }
    }
    names.sort();
    names.dedup();
    names
}

/// Build the opt-in mesh service from CLI configuration.
///
/// Returns `None` when `--mesh` is absent, so a node without mesh config keeps
/// its previous behavior. A configured-but-unsupported node gets an explicit
/// typed service instead of an error that would hide the reason.
/// Build the optional kernel dataplane request from CLI configuration.
///
/// Address/peer parsing is intentionally lenient here: invalid values produce
/// an explicit `DataplaneConfig` so the service preflight reports the typed
/// `mesh.ip-invalid` / `mesh.dataplane-invalid` state instead of guessing.
fn build_dataplane_config(
    args: &Args,
    peers: &[ozma_mesh::MeshNode],
) -> Option<ozma_mesh::dataplane::DataplaneConfig> {
    if !args.mesh_dataplane {
        return None;
    }

    let address = args
        .mesh_ip
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .and_then(|value| value.parse::<Ipv4Addr>().ok())
        .unwrap_or(Ipv4Addr::UNSPECIFIED);
    let peer_routes = peers
        .iter()
        .map(|peer| {
            peer.mesh_ip
                .parse::<Ipv4Addr>()
                .unwrap_or(Ipv4Addr::UNSPECIFIED)
        })
        .collect();

    let mut config = ozma_mesh::dataplane::DataplaneConfig::new(
        args.mesh_tun.clone(),
        address,
        args.mesh_prefix_len,
        args.mesh_mtu,
    )
    .with_peer_routes(peer_routes);

    if args.mesh_dataplane_nat {
        let mut nat = ozma_mesh::dataplane::NatConfig::new();
        if let Some(out_interface) = &args.mesh_nat_out {
            nat = nat.with_out_interface(out_interface.clone());
        }
        config = config.with_nat(nat);
    }
    Some(config)
}

/// Parse one `--mesh-trust` authorization entry.
///
/// Accepts JSON (`{"id":"...","wg_pubkey":"...","mesh_ip":"..."}`) or the
/// compact `ID=BASE64_KEY` form. Key material is validated before it can
/// authorize anything.
fn parse_mesh_trust(spec: &str) -> Result<ozma_mesh::TrustedPeer, String> {
    let spec = spec.trim();
    if spec.is_empty() {
        return Err("--mesh-trust must not be empty".to_owned());
    }
    let trusted = if spec.starts_with('{') {
        serde_json::from_str::<ozma_mesh::TrustedPeer>(spec)
            .map_err(|error| format!("--mesh-trust is not valid TrustedPeer JSON: {error}"))?
    } else {
        let (id, key) = spec.split_once('=').ok_or_else(|| {
            "--mesh-trust must be JSON or ID=BASE64_KEY (no '=' separator found)".to_owned()
        })?;
        ozma_mesh::TrustedPeer::new(id.trim(), key.trim())
    };
    if trusted.id.trim().is_empty() {
        return Err("--mesh-trust node id must not be empty".to_owned());
    }
    ozma_mesh::WgPublicKey(trusted.wg_pubkey.clone())
        .to_bytes()
        .map_err(|error| format!("--mesh-trust public key is invalid: {error}"))?;
    if let Some(mesh_ip) = &trusted.mesh_ip {
        mesh_ip
            .parse::<Ipv4Addr>()
            .map_err(|_| format!("--mesh-trust mesh_ip {mesh_ip:?} is not IPv4"))?;
    }
    Ok(trusted)
}

/// Parse one `--mesh-peer-endpoint` entry (`ID=ADDRESS:PORT`).
fn parse_mesh_peer_endpoint(spec: &str) -> Result<(String, std::net::SocketAddr), String> {
    let (id, endpoint) = spec.split_once('=').ok_or_else(|| {
        "--mesh-peer-endpoint must be ID=ADDRESS:PORT (no '=' separator found)".to_owned()
    })?;
    let id = id.trim();
    if id.is_empty() {
        return Err("--mesh-peer-endpoint node id must not be empty".to_owned());
    }
    let endpoint = endpoint
        .trim()
        .parse::<std::net::SocketAddr>()
        .map_err(|error| {
            format!("--mesh-peer-endpoint {id:?} is not a valid ADDRESS:PORT endpoint: {error}")
        })?;
    if !endpoint.is_ipv4() {
        return Err("--mesh-peer-endpoint must be an IPv4 address:port".to_owned());
    }
    if endpoint.port() == 0 {
        return Err("--mesh-peer-endpoint port must be non-zero".to_owned());
    }
    Ok((id.to_owned(), endpoint))
}

fn build_mesh_service(args: &Args) -> Option<mesh_service::MeshService> {
    if !args.mesh {
        return None;
    }

    let log = || mesh_service::MeshEventLog::new(mesh_service::MESH_EVENT_CAPACITY);
    if args.mesh_kernel && args.mesh_dataplane_nat {
        return Some(mesh_service::MeshService::unsupported(
            mesh_service::MeshServiceError::new(
                "mesh.dataplane-invalid",
                "NAT is not implemented for kernel WireGuard mode; remove \
                 --mesh-dataplane-nat",
            ),
            log(),
        ));
    }
    if args.mesh_kernel && args.mesh_dataplane {
        warn!(
            "--mesh-kernel owns the wg link; --mesh-dataplane does not create a TUN/pump \
             (its tun/mtu/prefix flags only size the kernel link)"
        );
    }
    let mut peers = Vec::new();
    for spec in &args.mesh_peer {
        match serde_json::from_str::<ozma_mesh::MeshNode>(spec) {
            Ok(peer) => peers.push(peer),
            Err(error) => {
                return Some(mesh_service::MeshService::unsupported(
                    mesh_service::MeshServiceError::new(
                        "mesh.peer-invalid",
                        format!("--mesh-peer is not a valid MeshNode JSON object: {error}"),
                    ),
                    log(),
                ));
            }
        }
    }

    let mut peer_endpoints = std::collections::BTreeMap::new();
    for spec in &args.mesh_peer_endpoint {
        match parse_mesh_peer_endpoint(spec) {
            Ok((id, endpoint)) => {
                if !peers.iter().any(|peer| peer.id == id) {
                    return Some(mesh_service::MeshService::unsupported(
                        mesh_service::MeshServiceError::new(
                            "mesh.peer-invalid",
                            format!("--mesh-peer-endpoint {id} has no matching --mesh-peer entry"),
                        ),
                        log(),
                    ));
                }
                peer_endpoints.insert(id, endpoint);
            }
            Err(reason) => {
                return Some(mesh_service::MeshService::unsupported(
                    mesh_service::MeshServiceError::new("mesh.peer-invalid", reason),
                    log(),
                ));
            }
        }
    }

    let mut trusted = Vec::new();
    for spec in &args.mesh_trust {
        match parse_mesh_trust(spec) {
            Ok(peer) => trusted.push(peer),
            Err(reason) => {
                return Some(mesh_service::MeshService::unsupported(
                    mesh_service::MeshServiceError::new("mesh.discovery-invalid", reason),
                    log(),
                ));
            }
        }
    }

    let cluster_key = match &args.mesh_cluster_key_file {
        Some(path) => match ozma_mesh::ClusterSigningKey::load(path) {
            Ok(key) => Some(std::sync::Arc::new(key)),
            Err(error) => {
                return Some(mesh_service::MeshService::unsupported(
                    mesh_service::MeshServiceError::new(
                        "mesh.cluster-key-invalid",
                        format!("--mesh-cluster-key-file could not be loaded: {error}"),
                    ),
                    log(),
                ))
            }
        },
        None => None,
    };

    let capabilities: Vec<String> = args
        .cap
        .split(',')
        .map(str::trim)
        .filter(|capability| !capability.is_empty())
        .map(str::to_owned)
        .collect();

    let rotation_schedule = if args.mesh_rotate_hours == 0 {
        None
    } else {
        let schedule = ozma_mesh::RotationSchedule::new(args.mesh_rotate_hours)
            .with_jitter_secs(args.mesh_rotate_jitter_secs)
            .with_grace_secs(args.mesh_rotate_grace_secs);
        if let Err(reason) = schedule.validate() {
            return Some(mesh_service::MeshService::unsupported(
                mesh_service::MeshServiceError::new("mesh.rotation-invalid", reason),
                log(),
            ));
        }
        Some(schedule)
    };

    let discovery = args.mesh_discover.then(|| {
        ozma_mesh::DiscoveryConfig::new(args.name.clone(), peers.clone(), trusted.clone())
            .with_max_peers(args.mesh_discover_max_peers)
            .with_ttl(Duration::from_secs(args.mesh_discover_ttl))
    });

    let kernel = args.mesh_kernel.then(|| {
        let address = args
            .mesh_ip
            .as_deref()
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .and_then(|value| value.parse::<Ipv4Addr>().ok())
            .unwrap_or(Ipv4Addr::UNSPECIFIED);
        ozma_mesh::KernelWgSettings::new(
            args.mesh_tun.clone(),
            address,
            args.mesh_prefix_len,
            args.mesh_mtu,
            args.mesh_port,
        )
    });

    let config = mesh_service::MeshServiceConfig {
        node_id: args.name.clone(),
        mesh_ip: args.mesh_ip.clone().unwrap_or_default(),
        wg_port: args.mesh_port,
        key_file: args.mesh_key_file.clone(),
        requires_root: false,
        requires_wg: false,
        kernel,
        peers: peers.clone(),
        peer_endpoints,
        capabilities,
        discovery,
        cluster_key,
        rotation_schedule,
        // Kernel mode owns its link and routes; a TUN dataplane/pump would be
        // a competing second interface, so it is not built.
        dataplane: if args.mesh_kernel {
            None
        } else {
            build_dataplane_config(args, &peers)
        },
        event_capacity: None,
    };
    let environment = mesh_service::MeshEnvironment::detect();

    Some(mesh_service::MeshService::from_config(config, &environment))
}

/// Controller-facing name for a selected input sink kind.
fn sink_kind_name(kind: SinkKind) -> &'static str {
    match kind {
        SinkKind::Uinput => controller_compat::SINK_UINPUT,
        SinkKind::Qmp => controller_compat::SINK_QMP,
        SinkKind::HidGadget => controller_compat::SINK_HID_GADGET,
        SinkKind::Recording => controller_compat::SINK_FAKE_RECORDING,
    }
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

/// Build the TXT record map that mirrors `node.py`'s `txt` dict and the
/// controller's `DirectRegisterRequest` fields.
///
/// Every value is a string: the controller's direct-registration model expects
/// strings for ports and JSON-encoded lists.
fn build_txt(
    args: &Args,
    local_ip: &str,
    registration_extras: &registration::RegistrationExtras,
) -> std::collections::HashMap<String, String> {
    let mut txt = std::collections::HashMap::new();
    txt.insert("proto".into(), "1".into());
    txt.insert("role".into(), args.role.clone());
    txt.insert("hw".into(), args.hw.clone());
    txt.insert("fw".into(), args.fw.clone());
    txt.insert("cap".into(), args.cap.clone());
    txt.insert("api_port".into(), args.http_port.to_string());
    txt.insert(
        "quic_port".into(),
        args.http_port.saturating_add(2).to_string(),
    );
    txt.insert(
        "machine_class".into(),
        ozma_auth::MachineClass::parse(&args.machine_class)
            .unwrap_or(ozma_auth::MachineClass::Workstation)
            .as_str()
            .to_owned(),
    );
    txt.insert(
        "capture_device".into(),
        args.capture_device.clone().unwrap_or_default(),
    );
    txt.insert("display_outputs".into(), args.display_outputs_json.clone());
    txt.insert("pci_devices".into(), args.pci_devices_json.clone());
    if args.rtsp_port != 0 {
        txt.insert("rtsp_port".into(), args.rtsp_port.to_string());
    }
    let capabilities: Vec<&str> = args
        .cap
        .split(',')
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect();
    if capabilities.contains(&"video") {
        txt.insert("stream_port".into(), args.http_port.to_string());
        txt.insert("stream_path".into(), "/stream/stream.m3u8".into());
    }
    if capabilities.contains(&"audio") {
        let audio_sink = args.audio_sink.as_deref().map(str::trim).unwrap_or("");
        txt.insert(
            "audio_type".into(),
            if audio_sink.is_empty() {
                String::new()
            } else {
                "pipewire".to_owned()
            },
        );
        txt.insert("audio_sink".into(), audio_sink.to_owned());
    }
    // seat_id, VNC, and the vm_ip/vm_guest_ip pair only exist when configured.
    // VBAN, camera/Frigate, and Sunshine fields stay absent because the Rust
    // node has no backend for them; /api/v1/connection.registration names them
    // as unsupported controller fields.
    registration_extras.apply_to_txt(&mut txt);
    txt.insert("host".into(), local_ip.to_string());
    txt
}

/// Keep `NodeId` imported in the binary's public surface for integrations that
/// build a node id from the same convention.
pub fn node_id_for(name: &str) -> NodeId {
    NodeConfig::node_id_from_name(name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn probe_failure_detail_does_not_repeat_the_status_prefix() {
        assert_eq!(
            probe_failure_detail(&"exit status: 1", "Connection refused\n"),
            "exit status: 1: Connection refused"
        );
        assert_eq!(
            probe_failure_detail(
                &"exit status: 1",
                "  Connection failure:\nConnection refused \n"
            ),
            "exit status: 1: Connection failure: Connection refused"
        );
        assert_eq!(
            probe_failure_detail(&"exit status: 1", ""),
            "exit status: 1"
        );
    }

    #[test]
    fn app_control_is_off_by_default_and_requires_app_control_for_other_users() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(!defaults.app_control, "application control must be opt-in");
        assert_eq!(defaults.app_control_kill_after_seconds, 2);
        assert!(!defaults.app_control_allow_other_users);

        let enabled = Args::parse_from([
            "ozma-node",
            "--app-control",
            "--app-control-kill-after-seconds",
            "7",
            "--app-control-allow-other-users",
        ]);
        assert!(enabled.app_control);
        assert_eq!(enabled.app_control_kill_after_seconds, 7);
        assert!(enabled.app_control_allow_other_users);

        assert!(
            Args::try_parse_from(["ozma-node", "--app-control-allow-other-users"]).is_err(),
            "widening the ownership policy requires --app-control"
        );
    }

    #[test]
    fn webrtc_is_disabled_by_default() {
        let args = Args::parse_from(["ozma-node"]);
        assert!(
            args.webrtc_helper.is_none(),
            "WebRTC must not default to an implicit Python helper"
        );
        assert!(args.webrtc_helper_script.is_none());
        assert!(
            !args.webrtc_native,
            "the native helper must be selected explicitly"
        );
        assert_eq!(args.webrtc_width, 640);
        assert_eq!(args.webrtc_height, 480);
        assert_eq!(args.webrtc_fps, 30);
        assert_eq!(args.webrtc_bitrate_kbps, 4_000);
        assert!(args.webrtc_socket.is_none());
        assert!(args.webrtc_shm.is_none());
        assert!(
            args.webrtc_audio_source.is_none(),
            "WebRTC audio must stay disabled unless a monitor source is supplied"
        );
        assert_eq!(args.webrtc_audio_backend, WebRtcAudioBackend::Auto);
        assert!(args.multi_console_webrtc_audio_source.is_none());
        assert!(args.webrtc_stun.is_none());
        assert!(args.webrtc_turn.is_none());
        assert!(args.webrtc_turn_user.is_none());
        assert!(args.webrtc_turn_password.is_none());
    }

    #[test]
    fn webrtc_native_flag_parses_and_stays_opt_in() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(!defaults.webrtc_native);

        let native = Args::parse_from(["ozma-node", "--webrtc-native"]);
        assert!(native.webrtc_native);
        assert!(native.webrtc_helper.is_none());

        let named = Args::parse_from(["ozma-node", "--webrtc-helper", "ozma-webrtc-native"]);
        assert!(!named.webrtc_native);
        assert_eq!(
            named.webrtc_helper.as_deref(),
            Some(std::path::Path::new("ozma-webrtc-native"))
        );
    }

    #[test]
    fn webrtc_stun_and_turn_flags_parse_without_defaults() {
        let args = Args::parse_from([
            "ozma-node",
            "--webrtc-stun",
            "stun:stun.example.org:3478",
            "--webrtc-turn",
            "turn:turn.example.org:3478",
            "--webrtc-turn-user",
            "alice",
            "--webrtc-turn-password",
            "s3cret",
        ]);
        assert_eq!(
            args.webrtc_stun.as_deref(),
            Some("stun:stun.example.org:3478")
        );
        assert_eq!(
            args.webrtc_turn.as_deref(),
            Some("turn:turn.example.org:3478")
        );
        assert_eq!(args.webrtc_turn_user.as_deref(), Some("alice"));
        assert_eq!(args.webrtc_turn_password.as_deref(), Some("s3cret"));
    }

    #[test]
    fn keyboard_layout_defaults_to_us_and_parses_supported_names() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert_eq!(defaults.keyboard_layout, KeyboardLayout::Us);

        for (name, expected) in [
            ("us", KeyboardLayout::Us),
            ("uk", KeyboardLayout::Uk),
            ("gb", KeyboardLayout::Uk),
            ("de", KeyboardLayout::De),
            ("fr", KeyboardLayout::Fr),
        ] {
            let args = Args::parse_from(["ozma-node", "--keyboard-layout", name]);
            assert_eq!(args.keyboard_layout, expected, "{name}");
        }
        assert!(
            Args::try_parse_from(["ozma-node", "--keyboard-layout", "dvorak"]).is_err(),
            "unknown layouts must fail rather than guess"
        );
    }

    #[test]
    fn libvirt_uri_flag_parses_and_is_optional() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert_eq!(defaults.libvirt_uri, None);

        let args = Args::parse_from([
            "ozma-node",
            "--libvirt-domain",
            "vm1",
            "--libvirt-uri",
            "qemu:///system",
        ]);
        assert_eq!(args.libvirt_domain.as_deref(), Some("vm1"));
        assert_eq!(args.libvirt_uri.as_deref(), Some("qemu:///system"));
    }

    #[test]
    fn webrtc_audio_flags_parse_and_default_to_disabled() {
        let args = Args::parse_from([
            "ozma-node",
            "--webrtc-audio-source",
            "ozma-vm1.monitor",
            "--webrtc-audio-backend",
            "pipewire",
        ]);
        assert_eq!(
            args.webrtc_audio_source.as_deref(),
            Some("ozma-vm1.monitor")
        );
        assert_eq!(args.webrtc_audio_backend, WebRtcAudioBackend::PipeWire);

        let per_console = Args::parse_from([
            "ozma-node",
            "--multi-console-webrtc-audio-source",
            "ozma-vm{console}.monitor",
        ]);
        assert_eq!(
            per_console.multi_console_webrtc_audio_source.as_deref(),
            Some("ozma-vm{console}.monitor")
        );

        let pulse = Args::parse_from([
            "ozma-node",
            "--webrtc-audio-source=ozma-vm1.monitor",
            "--webrtc-audio-backend=pulse",
        ]);
        assert_eq!(pulse.webrtc_audio_backend, WebRtcAudioBackend::Pulse);

        assert!(Args::try_parse_from(["ozma-node", "--webrtc-audio-backend", "alsa"]).is_err());
    }

    #[test]
    fn vnc_capture_flags_parse_with_vm_defaults() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(defaults.vnc_host.is_none());
        assert!(defaults.vnc_port.is_none());
        assert_eq!(defaults.vnc_width, ozma_capture::VNC_DEFAULT_WIDTH);
        assert_eq!(defaults.vnc_height, ozma_capture::VNC_DEFAULT_HEIGHT);
        assert_eq!(defaults.vnc_fps, ozma_capture::VNC_DEFAULT_FPS);
        assert_eq!(defaults.ffmpeg_bin, std::path::PathBuf::from("ffmpeg"));

        let args = Args::parse_from([
            "ozma-node",
            "--profile",
            "vm",
            "--vnc-host",
            "127.0.0.1",
            "--vnc-port",
            "5901",
            "--vnc-width",
            "1280",
            "--vnc-height",
            "720",
            "--vnc-fps",
            "15",
            "--ffmpeg-bin",
            "/usr/bin/ffmpeg",
        ]);
        assert_eq!(args.vnc_host.as_deref(), Some("127.0.0.1"));
        assert_eq!(args.vnc_port, Some(5901));
        assert_eq!(
            (args.vnc_width, args.vnc_height, args.vnc_fps),
            (1280, 720, 15)
        );
        assert_eq!(args.ffmpeg_bin, std::path::PathBuf::from("/usr/bin/ffmpeg"));
    }

    #[test]
    fn vnc_server_flags_are_opt_in_and_auto_port_by_default() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(!defaults.vnc_server);
        assert!(defaults.vnc_bind.is_none());
        assert_eq!(defaults.vnc_server_port, 0);
        assert!(defaults.vnc_password_file.is_none());
        assert!(!defaults.vnc_allow_unauthenticated);
        assert_eq!(defaults.vnc_max_clients, 4);
        assert_eq!(defaults.vnc_idle_timeout_seconds, 0);

        let args = Args::parse_from([
            "ozma-node",
            "--vnc-server",
            "--vnc-bind",
            "0.0.0.0",
            "--vnc-server-port",
            "5900",
            "--vnc-password-file",
            "/run/ozma/vnc.pass",
            "--vnc-max-clients",
            "8",
            "--vnc-idle-timeout-seconds",
            "120",
        ]);
        assert!(args.vnc_server);
        assert_eq!(args.vnc_bind.as_deref(), Some("0.0.0.0"));
        assert_eq!(args.vnc_server_port, 5900);
        assert_eq!(
            args.vnc_password_file.as_deref(),
            Some(std::path::Path::new("/run/ozma/vnc.pass"))
        );
        assert_eq!(args.vnc_max_clients, 8);
        assert_eq!(args.vnc_idle_timeout_seconds, 120);
        assert!(!args.vnc_allow_unauthenticated);

        let overridden = Args::parse_from([
            "ozma-node",
            "--vnc-server",
            "--vnc-bind",
            "0.0.0.0",
            "--vnc-allow-unauthenticated",
        ]);
        assert!(overridden.vnc_allow_unauthenticated);
    }

    #[test]
    fn desktop_x11_and_portal_capture_flags_parse() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(defaults.x11_display.is_none());
        assert!(defaults.x11_offset.is_none());
        assert!(defaults.x11_width.is_none());
        assert!(defaults.x11_height.is_none());
        assert!(defaults.x11_fps.is_none());
        assert!(defaults.portal_source_type.is_none());
        assert!(defaults.portal_cursor.is_none());
        assert!(defaults.portal_restore_token.is_none());
        assert!(defaults.portal_timeout_ms.is_none());

        let x11 = Args::parse_from([
            "ozma-node",
            "--profile",
            "desktop",
            "--capture-provider",
            "x11",
            "--x11-display",
            ":0.0",
            "--x11-offset",
            "+10,20",
            "--x11-width",
            "640",
            "--x11-height",
            "480",
            "--x11-fps",
            "15",
            "--ffmpeg-bin",
            "/opt/ffmpeg",
        ]);
        assert_eq!(x11.x11_display.as_deref(), Some(":0.0"));
        assert_eq!(parse_x11_offset(x11.x11_offset.as_deref()), Ok((10, 20)));
        assert_eq!(
            (x11.x11_width, x11.x11_height, x11.x11_fps),
            (Some(640), Some(480), Some(15))
        );
        assert_eq!(x11.ffmpeg_bin, std::path::PathBuf::from("/opt/ffmpeg"));

        let portal = Args::parse_from([
            "ozma-node",
            "--profile",
            "desktop",
            "--capture-provider",
            "portal",
            "--portal-source-type",
            "window",
            "--portal-cursor",
            "embedded",
            "--portal-restore-token",
            "token-1",
            "--portal-timeout-ms",
            "5000",
        ]);
        assert_eq!(portal.portal_source_type, Some(PortalSourceKind::Window));
        assert_eq!(portal.portal_cursor, Some(PortalCursorKind::Embedded));
        assert_eq!(portal.portal_restore_token.as_deref(), Some("token-1"));
        assert_eq!(portal.portal_timeout_ms, Some(5_000));

        assert!(Args::try_parse_from([
            "ozma-node",
            "--profile",
            "desktop",
            "--portal-source-type",
            "screen"
        ])
        .is_err());
        assert!(parse_x11_offset(Some("10")).is_err());
        assert!(parse_x11_offset(Some("10,-1")).is_err());
        assert_eq!(parse_x11_offset(None), Ok((0, 0)));
    }

    #[test]
    fn nvenc_gpu_flag_parses_and_rejects_invalid_values() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert_eq!(defaults.nvenc_gpu, None, "no GPU is invented by default");

        let explicit = Args::parse_from(["ozma-node", "--nvenc-gpu", "1"]);
        assert_eq!(explicit.nvenc_gpu, Some(1));

        let alias = Args::parse_from(["ozma-node", "--cuda-device", "0"]);
        assert_eq!(alias.nvenc_gpu, Some(0));

        assert!(Args::try_parse_from(["ozma-node", "--nvenc-gpu", "abc"]).is_err());
        assert!(Args::try_parse_from(["ozma-node", "--nvenc-gpu", "-1"]).is_err());
    }

    #[test]
    fn fake_capture_flags_parse_and_require_the_fake_provider() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert_eq!(defaults.fake_width, None);
        assert_eq!(defaults.fake_height, None);
        assert_eq!(defaults.fake_fps, None);

        let args = Args::parse_from([
            "ozma-node",
            "--profile",
            "vm",
            "--capture-provider",
            "fake",
            "--fake-width",
            "3840",
            "--fake-height",
            "2160",
            "--fake-fps",
            "30",
        ]);
        assert_eq!(
            (args.fake_width, args.fake_height, args.fake_fps),
            (Some(3_840), Some(2_160), Some(30))
        );
        assert!(
            validate_fake_flags("fake", args.fake_width, args.fake_height, args.fake_fps).is_ok()
        );

        // The geometry flags are dev/test settings, not a stealth way to
        // switch a real provider to a synthetic source.
        let error = validate_fake_flags("", Some(3_840), None, None).unwrap_err();
        assert!(error.contains("--capture-provider fake"), "{error}");
        assert!(validate_fake_flags("v4l2", None, None, Some(60)).is_err());
        assert!(validate_fake_flags("", None, None, None).is_ok());
    }

    #[test]
    fn hls_pixel_format_flags_resolve_and_reject_conflicts() {
        assert_eq!(
            resolve_hls_pixel_format(None, false, false).unwrap(),
            HlsPixelFormat::Yuv420p
        );
        assert_eq!(
            resolve_hls_pixel_format(None, true, false).unwrap(),
            HlsPixelFormat::P010
        );
        assert_eq!(
            resolve_hls_pixel_format(None, false, true).unwrap(),
            HlsPixelFormat::P010,
            "--hls-hdr implies a 10-bit target"
        );
        assert_eq!(
            resolve_hls_pixel_format(Some("p010"), false, false).unwrap(),
            HlsPixelFormat::P010
        );
        assert!(resolve_hls_pixel_format(Some("yuv420p"), true, false).is_err());
        assert!(resolve_hls_pixel_format(Some("yuv420p"), false, true).is_err());
        assert!(resolve_hls_pixel_format(Some("nv12"), false, false).is_err());

        let args = Args::parse_from([
            "ozma-node",
            "--hls",
            "--hls-encoder",
            "hevc_vaapi",
            "--hls-10bit",
            "--hls-hdr",
            "--hls-color-primaries",
            "bt2020",
            "--hls-color-transfer",
            "smpte2084",
            "--hls-color-matrix",
            "bt2020nc",
        ]);
        assert!(args.hls_10bit);
        assert!(args.hls_hdr);
        assert_eq!(args.hls_color_primaries.as_deref(), Some("bt2020"));
        assert_eq!(args.hls_color_transfer.as_deref(), Some("smpte2084"));
        assert_eq!(args.hls_color_matrix.as_deref(), Some("bt2020nc"));
    }

    #[test]
    fn hls_audio_source_is_explicit_or_derived_from_a_named_monitor() {
        let default_target = ozma_audio::AudioTarget::Default;
        assert_eq!(
            resolve_hls_audio_source(false, None, None, &default_target).unwrap(),
            None
        );
        assert_eq!(
            resolve_hls_audio_source(true, Some("ozma-vm1.monitor"), None, &default_target)
                .unwrap()
                .as_deref(),
            Some("ozma-vm1.monitor")
        );
        assert_eq!(
            resolve_hls_audio_source(true, None, Some("ozma-vm1"), &default_target)
                .unwrap()
                .as_deref(),
            Some("ozma-vm1.monitor")
        );
        assert_eq!(
            resolve_hls_audio_source(true, None, Some("ozma-vm1.monitor"), &default_target)
                .unwrap()
                .as_deref(),
            Some("ozma-vm1.monitor"),
            "an already-monitor sink is not suffixed twice"
        );
        assert_eq!(
            resolve_hls_audio_source(
                true,
                None,
                None,
                &ozma_audio::AudioTarget::SinkMonitor("ozma-vm2".to_owned())
            )
            .unwrap()
            .as_deref(),
            Some("ozma-vm2.monitor")
        );
        let error =
            resolve_hls_audio_source(true, None, None, &default_target).expect_err("no named sink");
        assert!(error.contains("--hls-audio-source"), "{error}");
        assert!(
            resolve_hls_audio_source(true, Some("   "), Some("sink"), &default_target).is_err()
        );

        let args = Args::parse_from([
            "ozma-node",
            "--hls-audio",
            "--hls-audio-source",
            "ozma-vm1.monitor",
            "--hls-audio-bitrate-kbps",
            "96",
        ]);
        assert!(args.hls_audio);
        assert_eq!(args.hls_audio_source.as_deref(), Some("ozma-vm1.monitor"));
        assert_eq!(args.hls_audio_bitrate_kbps, 96);
    }

    #[test]
    fn desktop_capture_and_audio_target_flags_parse() {
        let defaults = Args::parse_from(["ozma-node", "--profile", "desktop"]);
        assert_eq!(defaults.desktop_capture_target, "default");
        assert_eq!(defaults.desktop_capture_width, 1920);
        assert_eq!(defaults.desktop_capture_height, 1080);
        assert_eq!(defaults.desktop_capture_fps, 30);
        assert_eq!(defaults.audio_capture_target, "default");
        assert!(defaults.desktop_capture_remote.is_none());

        let args = Args::parse_from([
            "ozma-node",
            "--profile",
            "desktop",
            "--desktop-capture-target",
            "node:42",
            "--desktop-capture-remote",
            "ozma-desktop",
            "--desktop-capture-width",
            "2560",
            "--desktop-capture-height",
            "1440",
            "--desktop-capture-fps",
            "60",
            "--audio-capture-target",
            "monitor:ozma-desktop",
        ]);
        assert_eq!(args.desktop_capture_target, "node:42");
        assert_eq!(args.desktop_capture_remote.as_deref(), Some("ozma-desktop"));
        assert_eq!(
            (args.desktop_capture_width, args.desktop_capture_height),
            (2560, 1440)
        );
        assert_eq!(args.desktop_capture_fps, 60);
        assert_eq!(args.audio_capture_target, "monitor:ozma-desktop");

        assert!(Args::try_parse_from(["ozma-node", "--desktop-capture-fps", "abc"]).is_err());
    }

    #[test]
    fn display_output_and_looking_glass_flags_parse() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(defaults.display_output.is_empty());
        assert_eq!(defaults.display_outputs_json, "[]");
        assert_eq!(defaults.capture_console, None);
        assert!(defaults.looking_glass_shm.is_none());
        assert_eq!(
            defaults.looking_glass_fps,
            ozma_capture::LOOKING_GLASS_DEFAULT_FPS
        );
        assert_eq!(
            defaults.looking_glass_id,
            ozma_capture::LOOKING_GLASS_DEFAULT_ID
        );
        assert_eq!(
            defaults.looking_glass_name,
            ozma_capture::LOOKING_GLASS_DEFAULT_NAME
        );

        let args = Args::parse_from([
            "ozma-node",
            "--profile",
            "vm",
            "--display-output",
            "0:Console_0:1920x1080:primary:dbus",
            "--display-output",
            "1:Console_1:1280x1024:dbus",
            "--capture-console",
            "1",
            "--looking-glass-shm",
            "/dev/shm/looking-glass",
            "--looking-glass-fps",
            "60",
            "--looking-glass-id",
            "lg-vm1",
            "--looking-glass-name",
            "VM1 desktop",
        ]);
        assert_eq!(args.display_output.len(), 2);
        assert_eq!(args.capture_console, Some(1));
        assert_eq!(
            args.looking_glass_shm.as_deref(),
            Some(std::path::Path::new("/dev/shm/looking-glass"))
        );
        assert_eq!(args.looking_glass_fps, 60);
        assert_eq!(args.looking_glass_id, "lg-vm1");
        assert_eq!(args.looking_glass_name, "VM1 desktop");

        // The spec grammar is validated when the node resolves providers, not
        // by clap; keep the argv opaque here so the error stays actionable.
        assert!(Args::try_parse_from(["ozma-node", "--capture-console", "abc"]).is_err());
        assert!(Args::try_parse_from(["ozma-node", "--looking-glass-fps", "abc"]).is_err());
    }

    #[test]
    fn usb_flags_are_opt_in_and_parse() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(!defaults.usb_gadget);
        assert!(defaults.usb_configfs_root.is_none());
        assert!(defaults.usb_udc.is_none());
        assert!(defaults.usb_media.is_none());
        assert!(!defaults.usb_media_ro);
        assert!(!defaults.usb_media_cdrom);
        assert!(defaults.usb_media_export.is_none());
        assert_eq!(defaults.usb_media_export_format, "raw");
        assert!(!defaults.usb_media_export_read_write);
        assert_eq!(defaults.usb_media_restart_max_attempts, 3);
        assert_eq!(defaults.usb_media_restart_backoff_ms, 250);
        assert_eq!(defaults.usb_media_restart_max_backoff_ms, 4_000);
        assert!(!defaults.usb_media_restart_no_overlay_reload);
        assert!(!defaults.usb_uac2);
        assert!(!defaults.usb_vendor);
        assert!(defaults.usb_vendor_daemon.is_none());

        let args = Args::parse_from([
            "ozma-node",
            "--usb-gadget",
            "--usb-configfs-root",
            "/tmp/ozma-configfs",
            "--usb-udc",
            "dummy_udc.0",
            "--usb-media",
            "/srv/installer.iso",
            "--usb-media-ro",
            "--usb-media-cdrom",
            "--usb-uac2",
            "--usb-vendor",
            "--usb-vendor-daemon",
            "/usr/local/bin/ozma-ffs",
        ]);
        assert!(args.usb_gadget);
        assert_eq!(
            args.usb_configfs_root.as_deref(),
            Some(std::path::Path::new("/tmp/ozma-configfs"))
        );
        assert_eq!(args.usb_udc.as_deref(), Some("dummy_udc.0"));
        assert_eq!(
            args.usb_media.as_deref(),
            Some(std::path::Path::new("/srv/installer.iso"))
        );
        assert!(args.usb_media_ro);
        assert!(args.usb_media_cdrom);
        assert!(args.usb_uac2);
        assert!(args.usb_vendor);
        assert_eq!(
            args.usb_vendor_daemon.as_deref(),
            Some(std::path::Path::new("/usr/local/bin/ozma-ffs"))
        );
    }

    #[test]
    fn txt_carries_registration_parity_fields() {
        let args = Args::parse_from([
            "ozma-node",
            "--capture-device",
            "/dev/video0",
            "--vm-guest-ip",
            "10.0.0.42",
            "--audio-sink",
            "ozma-node",
        ]);
        let extras = registration::RegistrationExtras::resolve(
            None,
            None,
            None,
            args.vm_guest_ip.as_deref(),
        );
        let txt = build_txt(&args, "192.0.2.10", &extras);

        assert_eq!(txt["capture_device"], "/dev/video0");
        assert_eq!(txt["vm_ip"], "10.0.0.42");
        assert_eq!(txt["vm_guest_ip"], "10.0.0.42");
        assert_eq!(txt["audio_sink"], "ozma-node");
        assert_eq!(txt["audio_type"], "pipewire");
        assert_eq!(txt["machine_class"], "workstation");
        assert_eq!(txt["api_port"], "7382");
        assert_eq!(txt["display_outputs"], "[]");
        assert_eq!(txt["pci_devices"], "[]");
        assert_eq!(txt["host"], "192.0.2.10");
        assert!(!txt.contains_key("seat_id"));
        assert!(!txt.contains_key("vnc_host"));
        assert!(!txt.contains_key("audio_vban_port"));
        assert!(!txt.contains_key("camera_streams"));
        assert!(!txt.contains_key("frigate_host"));
        assert!(!txt.contains_key("sunshine_port"));
    }

    #[test]
    fn txt_vm_case_emits_vnc_endpoint_and_both_guest_ip_keys() {
        let args = Args::parse_from([
            "ozma-node",
            "--profile",
            "vm",
            "--vnc-host",
            "127.0.0.1",
            "--vnc-port",
            "5901",
            "--vm-guest-ip",
            "10.0.0.42",
        ]);
        let extras = registration::RegistrationExtras::resolve(
            None,
            args.vnc_host.as_deref(),
            args.vnc_port,
            args.vm_guest_ip.as_deref(),
        );
        let txt = build_txt(&args, "192.0.2.20", &extras);

        assert_eq!(txt["vnc_host"], "127.0.0.1");
        assert_eq!(txt["vnc_port"], "5901");
        assert_eq!(txt["vm_ip"], "10.0.0.42");
        assert_eq!(txt["vm_guest_ip"], "10.0.0.42");
        assert!(!txt.contains_key("seat_id"));
    }

    #[test]
    fn txt_multiseat_case_emits_only_the_explicit_seat_id() {
        let args = Args::parse_from(["ozma-node", "--profile", "desktop"]);
        let extras =
            registration::RegistrationExtras::resolve(Some("rig-1-seat-0"), None, None, None);
        let txt = build_txt(&args, "192.0.2.30", &extras);

        assert_eq!(txt["seat_id"], "rig-1-seat-0");
        assert!(!txt.contains_key("vnc_host"));
        assert!(!txt.contains_key("vm_ip"));
        assert!(!txt.contains_key("vm_guest_ip"));
    }

    #[test]
    fn multiseat_is_off_by_default_and_accepts_the_plan_alias() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(!defaults.multiseat);
        assert!(defaults.seats_file.is_none());

        let plan = Args::parse_from([
            "ozma-node",
            "--multiseat-plan",
            "--seats-file",
            "/etc/ozma/seats.json",
        ]);
        assert!(plan.multiseat);
        assert_eq!(
            plan.seats_file.as_deref(),
            Some(std::path::Path::new("/etc/ozma/seats.json"))
        );

        let multiseat = Args::parse_from([
            "ozma-node",
            "--multiseat",
            "--seats-file",
            "/etc/ozma/seats.json",
        ]);
        assert!(multiseat.multiseat);
    }

    #[test]
    fn multiseat_requires_a_seats_file() {
        assert!(Args::try_parse_from(["ozma-node", "--multiseat"]).is_err());
        assert!(Args::try_parse_from(["ozma-node", "--multiseat-plan"]).is_err());
    }

    #[test]
    fn mesh_discovery_is_off_by_default_and_trust_parsing_is_typed() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(!defaults.mesh_discover);
        assert_eq!(defaults.mesh_discover_ttl, 60);
        assert_eq!(defaults.mesh_discover_max_peers, 32);
        assert!(defaults.mesh_trust.is_empty());
        assert!(defaults.mesh_peer_endpoint.is_empty());
        assert!(defaults.mesh_cluster_key_file.is_none());
        assert!(!defaults.mesh_kernel);

        let (id, endpoint) = parse_mesh_peer_endpoint("node-b=10.10.30.2:51820").unwrap();
        assert_eq!(id, "node-b");
        assert_eq!(endpoint.to_string(), "10.10.30.2:51820");
        assert!(parse_mesh_peer_endpoint("missing-separator").is_err());
        assert!(parse_mesh_peer_endpoint("node-b=not-an-endpoint").is_err());
        assert!(parse_mesh_peer_endpoint("node-b=10.10.30.2:0").is_err());
        assert!(parse_mesh_peer_endpoint("=10.10.30.2:51820").is_err());

        let args = Args::parse_from([
            "ozma-node",
            "--mesh",
            "--mesh-discover",
            "--mesh-discover-ttl",
            "15",
            "--mesh-discover-max-peers",
            "8",
            "--mesh-cluster-key-file",
            "/tmp/ozma-cluster.key",
        ]);
        assert!(args.mesh_discover);
        assert_eq!(args.mesh_discover_ttl, 15);
        assert_eq!(args.mesh_discover_max_peers, 8);
        assert_eq!(
            args.mesh_cluster_key_file.as_deref(),
            Some(std::path::Path::new("/tmp/ozma-cluster.key"))
        );

        let (node, _key) = ozma_mesh::MeshNode::generate("node-a", "10.200.9.1", 51_820);
        let compact = parse_mesh_trust(&format!("node-a={}", node.wg_pubkey.0)).unwrap();
        assert_eq!(compact.id, "node-a");
        assert_eq!(compact.wg_pubkey, node.wg_pubkey.0);

        let json = parse_mesh_trust(&format!(
            r#"{{"id":"node-a","wg_pubkey":"{}","mesh_ip":"10.200.9.1"}}"#,
            node.wg_pubkey.0
        ))
        .unwrap();
        assert_eq!(json.mesh_ip.as_deref(), Some("10.200.9.1"));
        assert!(parse_mesh_trust("missing-separator").is_err());
        assert!(parse_mesh_trust("node-a=not-a-key").is_err());
        assert!(parse_mesh_trust("=abc").is_err());
    }

    #[test]
    fn mesh_dataplane_is_off_by_default_and_nat_is_opt_in() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert!(!defaults.mesh_dataplane);
        assert!(defaults.mesh_ip.is_none());
        assert_eq!(defaults.mesh_tun, "ozma0");
        assert_eq!(defaults.mesh_mtu, 1420);
        assert_eq!(defaults.mesh_prefix_len, 24);
        assert!(!defaults.mesh_dataplane_nat);
        assert!(defaults.mesh_nat_out.is_none());
        assert!(build_dataplane_config(&defaults, &[]).is_none());

        let args = Args::parse_from([
            "ozma-node",
            "--mesh",
            "--mesh-ip",
            "10.200.9.1",
            "--mesh-dataplane",
            "--mesh-dataplane-nat",
            "--mesh-nat-out",
            "eth0",
            "--mesh-mtu",
            "1400",
            "--mesh-prefix-len",
            "16",
        ]);
        let (peer, _key) = ozma_mesh::MeshNode::generate("peer", "10.200.9.2", 51_821);
        let config = build_dataplane_config(&args, &[peer]).unwrap();
        assert_eq!(config.interface, "ozma0");
        assert_eq!(config.address, Ipv4Addr::new(10, 200, 9, 1));
        assert_eq!(config.prefix_len, 16);
        assert_eq!(config.mtu, 1400);
        assert_eq!(config.peer_routes, vec![Ipv4Addr::new(10, 200, 9, 2)]);
        assert!(config.forwarding);
        let nat = config.nat.unwrap();
        assert_eq!(nat.table, "ozma_mesh");
        assert_eq!(nat.out_interface.as_deref(), Some("eth0"));

        let without_nat = Args::parse_from([
            "ozma-node",
            "--mesh",
            "--mesh-ip",
            "10.200.9.1",
            "--mesh-dataplane",
        ]);
        let config = build_dataplane_config(&without_nat, &[]).unwrap();
        assert!(
            config.nat.is_none(),
            "NAT must never be implicit when --mesh-dataplane is set"
        );
    }

    #[test]
    fn mesh_rotation_is_off_by_default_and_bounded_when_enabled() {
        let defaults = Args::parse_from(["ozma-node"]);
        assert_eq!(defaults.mesh_rotate_hours, 0);
        assert_eq!(defaults.mesh_rotate_jitter_secs, 300);
        assert_eq!(defaults.mesh_rotate_grace_secs, 90);

        let enabled = Args::parse_from([
            "ozma-node",
            "--mesh",
            "--mesh-rotate-hours",
            "6",
            "--mesh-rotate-jitter-secs",
            "30",
            "--mesh-rotate-grace-secs",
            "45",
        ]);
        assert_eq!(enabled.mesh_rotate_hours, 6);
        assert_eq!(enabled.mesh_rotate_jitter_secs, 30);
        assert_eq!(enabled.mesh_rotate_grace_secs, 45);

        let schedule = ozma_mesh::RotationSchedule::new(enabled.mesh_rotate_hours)
            .with_jitter_secs(enabled.mesh_rotate_jitter_secs)
            .with_grace_secs(enabled.mesh_rotate_grace_secs);
        schedule.validate().unwrap();
        assert!(ozma_mesh::RotationSchedule::new(0).validate().is_err());
        assert!(ozma_mesh::RotationSchedule::new(1)
            .with_grace_secs(1)
            .validate()
            .is_err());
    }
}
