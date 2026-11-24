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

use std::sync::{Arc, RwLock};

use clap::Parser;
use ozma_audio::{
    AudioRouter, PipeWireAudioRouter, PulseAudioAudioRouter, RouterConfig, TokioCommandRunner,
};
use ozma_input::SinkKind;
use ozma_power::{LibvirtPower, PowerBackend, QmpPower};
use ozma_protocol::NodeId;
use ozma_qmp::SharedQmpClient;
use ozma_usb::{
    ConfigFsUsbController, MassStorageImage, UsbError, UsbFunction, UsbGadgetConfig,
    UsbGadgetController,
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
    display_compat::SnapshotControl,
    display_control::{
        ConfigConsoleFactory, DisplayControl, HlsPipelineSwitch, SwitchablePipeline,
        WebRtcPipelineSwitch,
    },
    hls::{
        HlsConfig, HlsController, HlsEncoder, HlsError, HlsPixelFormat, HlsSourceFactory, HlsStatus,
    },
    input_select,
    keyboard_layout::KeyboardLayout,
    mdns, platform, registration, usb_audio,
    webrtc::{WebRtcAudioBackend, WebRtcConfig, WebRtcControl, WebRtcHelper},
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
    /// `--hls`; WebRTC runs only through an external helper when
    /// `--webrtc-helper` is set. The per-VM supervisor remains a later phase.
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
    /// WebRTC stays disabled (`not-configured`) unless this is set. The helper
    /// is the existing GStreamer/Python worker; native WebRTC is future work.
    #[arg(long, env = "OZMA_WEBRTC_HELPER", value_name = "PATH")]
    pub webrtc_helper: Option<std::path::PathBuf>,

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

#[tokio::main]
async fn main() {
    let mut args = Args::parse();

    let level = if args.debug { "debug" } else { "info" };
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(level)),
        )
        .init();

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
        qmp_connect_timeout: std::time::Duration::from_millis(args.qmp_connect_timeout_ms),
        qmp_command_timeout: std::time::Duration::from_millis(args.qmp_command_timeout_ms),
        vnc_host: args.vnc_host.clone(),
        vnc_port: args.vnc_port,
        vnc_width: args.vnc_width,
        vnc_height: args.vnc_height,
        vnc_fps: args.vnc_fps,
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
        ffmpeg_bin: args.ffmpeg_bin.clone(),
        vaapi_device: args.vaapi_device.clone(),
        nvenc_gpu: args.nvenc_gpu,
        audio_sink: args.audio_sink.clone(),
        webrtc_audio_source: args.webrtc_audio_source.clone(),
        webrtc_audio_backend: args.webrtc_audio_backend,
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
    let _input_qmp_handle = selected_input.handle;
    info!(sink = sink_kind, "Input sink ready");

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

    // Local HLS compatibility pipeline. HLS is opt-in; a missing source or
    // ffmpeg leaves it unavailable and reported as `failed`, never as running.
    // The controller owns the live pipeline so POST /codec can restart it with
    // a newly selected software encoder.
    let hls_output_dir = args.hls.then(|| {
        args.hls_dir
            .clone()
            .unwrap_or_else(|| default_hls_dir(&config.name))
    });
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
        let hls_config = HlsConfig {
            output_dir: directory.clone(),
            fps: args.hls_fps,
            bitrate_kbps: args.hls_bitrate_kbps,
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
        if !config.has_capability("video") {
            warn!("--hls was requested but the video capability is disabled; HLS is unavailable");
            HlsStatus::failed("video capability is disabled")
        } else {
            let registry = registry_slot.clone();
            let source_factory: HlsSourceFactory = std::sync::Arc::new(move || {
                let registry = registry
                    .read()
                    .unwrap_or_else(|poisoned| poisoned.into_inner())
                    .clone();
                open_first_capture_source(&registry).map_err(|message| HlsError::Source { message })
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

    // External WebRTC helper (GStreamer/Python first; native WebRTC is future
    // work). WebRTC is disabled unless `--webrtc-helper` is configured. A
    // configured helper that cannot start is reported as `failed`, never
    // `running`, and never crashes the node.
    let mut webrtc_switch_config: Option<WebRtcConfig> = None;
    let webrtc_control: Option<WebRtcControl> = match args.webrtc_helper.clone() {
        None => None,
        Some(program) => {
            let audio_source = args.webrtc_audio_source.clone();
            let webrtc_config = WebRtcConfig {
                helper_program: program,
                helper_script: args.webrtc_helper_script.clone(),
                socket_path: args
                    .webrtc_socket
                    .clone()
                    .unwrap_or_else(|| default_webrtc_socket(&config.name)),
                shm_path: args.webrtc_shm.clone().unwrap_or_else(default_webrtc_shm),
                width: args.webrtc_width,
                height: args.webrtc_height,
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
                Some(WebRtcControl::failed_with_audio(
                    format!("WebRTC unavailable: {error}"),
                    audio_source,
                ))
            } else if !config.has_capability("video") {
                warn!(
                    "--webrtc-helper was configured but the video capability is disabled; \
                     WebRTC is unavailable"
                );
                Some(WebRtcControl::failed_with_audio(
                    "WebRTC unavailable: the video capability is disabled".to_owned(),
                    audio_source,
                ))
            } else {
                match open_first_capture_source(&capture_registry).and_then(|source| {
                    WebRtcHelper::start(source, webrtc_config.clone())
                        .map_err(|error| error.to_string())
                }) {
                    Ok(helper) => {
                        info!(
                            pid = ?helper.status().helper_pid(),
                            socket = %webrtc_config.socket_path.display(),
                            shm = %webrtc_config.shm_path.display(),
                            width = webrtc_config.width,
                            height = webrtc_config.height,
                            fps = webrtc_config.fps,
                            audio = ?webrtc_config.audio_source,
                            audio_backend = webrtc_config.audio_backend.as_arg(),
                            stun = ?webrtc_config.stun_server,
                            turn = ?webrtc_config.turn_server,
                            turn_auth = webrtc_config.turn_user.is_some()
                                && webrtc_config.turn_password.is_some(),
                            "WebRTC helper started"
                        );
                        Some(WebRtcControl::from_helper(helper))
                    }
                    Err(error) => {
                        warn!(
                            error = %error,
                            "WebRTC unavailable: the external helper did not start"
                        );
                        Some(WebRtcControl::failed_with_audio(
                            format!("WebRTC unavailable: {error}"),
                            audio_source,
                        ))
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
    let usb_control = build_usb_control(&config);
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

    let compat_stats = controller_compat::CompatStats::default();
    let connection_info = controller_compat::ConnectionInfo::new(
        node_id.as_str(),
        config.profile.as_str(),
        config.http_port,
        quic_port,
        config.hid_udp_port,
        sink_kind,
    );
    // Runtime codec override control. Probed against the host exactly like the
    // session backend. When HLS is running, POST /codec restarts the HLS
    // pipeline with the selected software encoder; when HLS is off the override
    // is stored for a future start. WebRTC keeps its existing encoder.
    let codec_control = if config.has_capability("video") && !capabilities.encode.is_empty() {
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
    let display_control = match DisplayControl::new(
        Arc::new(ConfigConsoleFactory::new(
            config.clone(),
            shared_qmp.clone(),
        )),
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
    let capture_provider = config
        .capture_provider_name()
        .ok()
        .flatten()
        .unwrap_or_else(|| "none".to_owned());
    let capture_status = controller_compat::CaptureStatus::from_capability(
        capture_provider,
        capabilities.capture.first(),
    );
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
            .with_snapshot(Some(SnapshotControl::from_shared(registry_slot.clone())))
            .with_display_outputs(display_outputs_value, display_capture)
            .with_display_control(display_control)
            .with_capture(capture_status);
    // Control routes stay behind the bearer guard; the controller's read-only
    // probes are mounted after it so the unmodified controller works with
    // `--api-token`. See `controller_compat::{compat_router, public_router}`.
    let compat_router = controller_compat::compat_router(compat_state.clone());
    let public_compat_router = controller_compat::public_router(compat_state);

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
                tokio::spawn(async move {
                    if let Err(error) = listener.run().await {
                        error!("UDP HID listener exited: {error:#}");
                    }
                });
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
                 `/stream/*` from the first capture source. WebRTC runs only through an external \
                 helper and only when `--webrtc-helper` is set. POST /codec validates an override \
                 and, when HLS is running, restarts the pipeline with the mapped encoder; with HLS \
                 off it is stored with pending_restart. Still not implemented: native WebRTC, live \
                 D-Bus/VNC validation in CI, per-VM provisioning (ozma-softnode supervisor), and \
                 WebRTC audio. Do not treat WebRTC as functional without a configured helper."
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
                 available. Portal consent runs at startup; a denial, timeout, or missing portal \
                 leaves the node running without capture (never a fake source). Remaining \
                 limits: a compositor that exposes the screen only through the portal fd cannot \
                 be opened by the current PipeWire backend (typed error), returned portal restore \
                 tokens are not persisted, /audio/nodes is still a typed 501, and desktop sink \
                 creation is not implemented. Capture or input failures downgrade capabilities; \
                 the node keeps running rather than claiming a source it did not open."
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

    if config.has_capability("audio") && config.profile != NodeProfile::Desktop {
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
        )
        .await
        {
            error!("Node API exited: {error:#}");
        }
    });

    wait_for_shutdown().await;
    if let Some(controller) = hls_controller.as_ref() {
        if let Err(error) = controller.stop().await {
            warn!(error = %error, "HLS pipeline did not stop cleanly");
        }
    }
    if let Some(control) = webrtc_control.as_ref() {
        control.shutdown().await;
    }
    info!("Ozma node shutting down");
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

/// Open the first registry source for a helper/pipeline.
///
/// This uses the same registry as the VM/hardware profile: a VM node streams
/// its QMP display and a hardware node streams its V4L2/PipeWire source.
fn open_first_capture_source(
    registry: &ozma_capture::CaptureRegistry,
) -> Result<Box<dyn ozma_media::CaptureSource>, String> {
    let sources = registry.enumerate().map_err(|error| {
        format!(
            "capture discovery failed: {} ({})",
            error.message, error.code
        )
    })?;
    let source = sources
        .into_iter()
        .next()
        .ok_or_else(|| "no capture source is available".to_owned())?;
    let id = source.id.clone();
    registry
        .open(&source)
        .map_err(|error| format!("opening capture source `{id}` failed: {}", error.message))
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
            match power.connect().await {
                Ok(()) => {
                    info!(domain, "libvirt power backend connected");
                    Some(shared_power(power))
                }
                Err(error) => {
                    warn!(
                        domain,
                        error = %error,
                        "libvirt power backend unavailable; /power/* will report not-configured"
                    );
                    None
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
    if !command_on_path("pactl") {
        return Some(AudioControl::failed(format!(
            "`pactl` was not found on PATH; {kind} audio routing requires \
             pulseaudio-utils or pipewire-pulse"
        )));
    }
    if let Err(error) = probe_audio_server().await {
        return Some(AudioControl::failed(format!(
            "{kind} audio server is unavailable: {error}"
        )));
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
    Some(AudioControl::configured(
        router,
        config.audio_sink.clone(),
        kind.as_str(),
    ))
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
    let stderr = stderr.trim();
    let detail = if stderr.is_empty() {
        format!("exit status {}", output.status)
    } else {
        format!("exit status {}: {stderr}", output.status)
    };
    Err(format!("`pactl info` failed with {detail}"))
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
fn build_txt(args: &Args, local_ip: &str) -> std::collections::HashMap<String, String> {
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
    if let Some(vm_guest_ip) = args.vm_guest_ip.as_deref().map(str::trim) {
        if !vm_guest_ip.is_empty() {
            txt.insert("vm_guest_ip".into(), vm_guest_ip.to_owned());
        }
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webrtc_is_disabled_by_default() {
        let args = Args::parse_from(["ozma-node"]);
        assert!(
            args.webrtc_helper.is_none(),
            "WebRTC must not default to an implicit Python helper"
        );
        assert!(args.webrtc_helper_script.is_none());
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
        assert!(args.webrtc_stun.is_none());
        assert!(args.webrtc_turn.is_none());
        assert!(args.webrtc_turn_user.is_none());
        assert!(args.webrtc_turn_password.is_none());
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
        let txt = build_txt(&args, "192.0.2.10");

        assert_eq!(txt["capture_device"], "/dev/video0");
        assert_eq!(txt["vm_guest_ip"], "10.0.0.42");
        assert_eq!(txt["audio_sink"], "ozma-node");
        assert_eq!(txt["audio_type"], "pipewire");
        assert_eq!(txt["machine_class"], "workstation");
        assert_eq!(txt["api_port"], "7382");
        assert_eq!(txt["display_outputs"], "[]");
        assert_eq!(txt["pci_devices"], "[]");
        assert_eq!(txt["host"], "192.0.2.10");
    }
}
