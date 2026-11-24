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

use std::sync::Arc;

use clap::Parser;
use ozma_audio::{
    AudioRouter, PipeWireAudioRouter, PulseAudioAudioRouter, RouterConfig, TokioCommandRunner,
};
use ozma_input::SinkKind;
use ozma_power::{LibvirtPower, PowerBackend, QmpPower};
use ozma_protocol::NodeId;
use ozma_qmp::SharedQmpClient;
use tracing::{error, info, warn};

use ozma_node::{
    api,
    config::{
        AudioRouterKind, NodeConfig, NodeProfile, PowerProvider, ProviderOverrides,
        ProviderSelection, VmProviderConfig, DEFAULT_FAKE_FPS, DEFAULT_FAKE_HEIGHT,
        DEFAULT_FAKE_WIDTH, DEFAULT_QMP_COMMAND_TIMEOUT_MS, DEFAULT_QMP_CONNECT_TIMEOUT_MS,
    },
    controller_compat::{self, AudioControl, CodecControl},
    display_compat::SnapshotControl,
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
    /// sockets/domain are configured. Local HLS is opt-in via `--hls`; WebRTC
    /// runs only through an external helper when `--webrtc-helper` is set. The
    /// per-VM supervisor remains a later phase.
    #[arg(long, env = "OZMA_PROFILE", value_enum, default_value_t = NodeProfile::Hardware)]
    pub profile: NodeProfile,

    /// Override the profile's video capture provider (e.g. `v4l2`, `qmp`).
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

    /// JSON-encoded display output list sent to the controller.
    #[arg(long, default_value = "[]")]
    pub display_outputs_json: String,

    /// JSON-encoded PCI passthrough device list sent to the controller.
    #[arg(long, default_value = "[]")]
    pub pci_devices_json: String,

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
            .dbus_console
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
        fake_width: args.fake_width.unwrap_or(DEFAULT_FAKE_WIDTH),
        fake_height: args.fake_height.unwrap_or(DEFAULT_FAKE_HEIGHT),
        fake_fps: args.fake_fps.unwrap_or(DEFAULT_FAKE_FPS),
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

    if let Err(error) = config.validate() {
        error!("Invalid node configuration: {error}");
        std::process::exit(2);
    }

    let vm_providers = match config.vm_providers() {
        Ok(providers) => providers,
        Err(error) => {
            error!("Invalid provider configuration: {error}");
            std::process::exit(2);
        }
    };

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
    let capture_registry = platform::capture_registry_with_qmp_shared(&config, shared_qmp.as_ref());
    if config.profile == NodeProfile::Vm && !vm_providers.has_capture() {
        warn!(
            "vm profile has no --qmp-socket, --vnc-host/--vnc-port, or D-Bus display flags: \
             no capture source is configured. Hardware capture is not advertised on a VM."
        );
    }
    let mut capabilities = platform::detect_with_registry(&config, &capture_registry);
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
            let registry = capture_registry.clone();
            let source_factory: HlsSourceFactory = std::sync::Arc::new(move || {
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
            .with_webrtc(webrtc_control.clone())
            .with_snapshot(Some(SnapshotControl::new(capture_registry.clone())));
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

    if config.profile != NodeProfile::Hardware {
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
            "Profile '{}' is PARTIALLY WIRED: QMP/D-Bus/VNC capture, uinput/QMP input \
             injection, QMP/libvirt power control, and optional PipeWire/PulseAudio routing \
             start when their flags are configured. Local HLS is opt-in via `--hls` and feeds \
             `/stream/*` from the first capture source. WebRTC runs only through an external \
             helper and only when `--webrtc-helper` is set. POST /codec validates an override and, \
             when HLS is running, restarts the pipeline with the mapped encoder; with HLS off it \
             is stored with pending_restart. Still not \
             implemented: native WebRTC, live D-Bus/VNC validation in CI, per-VM provisioning \
             (ozma-softnode supervisor), and WebRTC audio. Do not treat WebRTC as functional \
             without a configured helper.",
            config.profile
        );
    }

    info!(
        name = %config.name,
        node_id = %node_id,
        profile = %config.profile,
        capture_provider = %vm_providers.capture.as_deref().unwrap_or("none"),
        input_provider = ?vm_providers.input_chain_names(),
        audio_provider = %config.providers.audio,
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

    if config.has_capability("audio") {
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
