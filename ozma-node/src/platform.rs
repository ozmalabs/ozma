// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Capability detection. The first pass builds on the protocol's deterministic
//! fake capabilities and narrows them according to the node's `--cap` list.
//! Real V4L2/encoder probes plug in here without changing the API.

use ozma_protocol::{
    AudioCapability, CaptureKind, Codec, EncodeCapability, EncoderBackend, InputCapability,
    NodeCapabilities, NodeId, PowerCapability, ResourceBudget, TransportMode, UsbCapability,
    VirtualDisplayCaps,
};

use ozma_capture::{
    CaptureRegistry, DBusDisplayConfig, FakeCaptureProvider, LookingGlassProviderConfig,
    QmpDisplayConfig, VncDisplayConfig, DBUS_DEFAULT_ID, QMP_DEFAULT_ID, QMP_DEFAULT_MAX_FPS,
    VNC_DEFAULT_ID,
};
use ozma_input::SinkKind;
use ozma_media::FrameFormat;
use ozma_qmp::SharedQmpClient;

use crate::config::{NodeConfig, NodeProfile};
use crate::hls::HlsConfig;

/// Build the capture registry for this node.
///
/// Hardware keeps V4L2+PipeWire discovery. A VM only ever advertises its own
/// display: either the QMP `screendump` path or the ffmpeg-backed VNC path
/// selected by the provider resolver. Advertising the host's V4L2/PipeWire
/// devices on a guest would claim hardware the VM does not own. A VM with no
/// configured display gets an empty registry.
///
/// This form gives a QMP provider its own connection. The node's `main` uses
/// [`capture_registry_with_qmp_shared`] so QMP capture shares the single
/// control client with power, snapshots, and input.
pub fn capture_registry(config: &NodeConfig) -> CaptureRegistry {
    capture_registry_with_qmp_shared(config, None)
}

/// Build the capture registry, optionally routing QMP `screendump` through the
/// node's single shared control client.
pub fn capture_registry_with_qmp_shared(
    config: &NodeConfig,
    shared: Option<&SharedQmpClient>,
) -> CaptureRegistry {
    match config.profile {
        NodeProfile::Vm => vm_capture_registry(config, shared),
        NodeProfile::Desktop => desktop_capture_registry(config),
        NodeProfile::Hardware => CaptureRegistry::system_with_pipewire(),
    }
}

/// Build the capture registry for a desktop node with a pre-resolved portal
/// stream.
///
/// The portal consent/selection handshake is asynchronous and may prompt the
/// user, so `main` runs it before this point. `None` means the handshake was not
/// requested, or it was denied/timed out/unavailable: the desktop registry then
/// contains no portal provider and the node advertises no capture. It never
/// falls back to X11, a camera, or the synthetic source.
#[cfg(target_os = "linux")]
pub fn capture_registry_with_portal(
    config: &NodeConfig,
    shared: Option<&SharedQmpClient>,
    portal: Option<ozma_capture::PortalStream>,
) -> CaptureRegistry {
    match config.profile {
        NodeProfile::Vm => vm_capture_registry(config, shared),
        NodeProfile::Desktop => desktop_capture_registry_with_portal(config, portal),
        NodeProfile::Hardware => CaptureRegistry::system_with_pipewire(),
    }
}

/// The desktop profile's capture registry.
///
/// Real desktop capture is selected by the same pure resolver used for logs and
/// `/api/v1/connection`:
///
/// * `pipewire` opens `--desktop-capture-target`;
/// * `x11` runs ffmpeg's `x11grab` on an explicitly named `--x11-display`;
/// * `portal` uses the already-resolved ScreenCast stream;
/// * `fake` is the explicit dev/test source;
/// * no provider is an empty registry.
///
/// V4L2 is deliberately absent: a desktop seat's camera is not its screen. A
/// portal denial/timeout leaves an empty registry rather than a fake source.
fn desktop_capture_registry(config: &NodeConfig) -> CaptureRegistry {
    #[cfg(target_os = "linux")]
    {
        desktop_capture_registry_with_portal(config, None)
    }

    #[cfg(not(target_os = "linux"))]
    {
        desktop_capture_registry_common(config)
    }
}

#[cfg(target_os = "linux")]
fn desktop_capture_registry_with_portal(
    config: &NodeConfig,
    portal: Option<ozma_capture::PortalStream>,
) -> CaptureRegistry {
    let selection = match config.desktop_providers() {
        Ok(providers) => providers.capture,
        Err(error) => {
            tracing::warn!(error = %error, "desktop capture: provider selection is invalid");
            return CaptureRegistry::new();
        }
    };
    match selection {
        crate::config::DesktopCaptureSelection::PipeWire(target) => {
            let options = ozma_capture::PipeWireOptions {
                target: stream_target_for(&target),
                remote: config.desktop_capture_remote.clone(),
                width: config.desktop_capture_width,
                height: config.desktop_capture_height,
                fps: config.desktop_capture_fps,
                ..ozma_capture::PipeWireOptions::default()
            };
            CaptureRegistry::new()
                .with(ozma_capture::PipeWireCaptureProvider::with_options(options))
        }
        crate::config::DesktopCaptureSelection::X11(x11) => {
            CaptureRegistry::new().with_x11_display(x11)
        }
        crate::config::DesktopCaptureSelection::Portal(options) => match portal {
            Some(stream) => {
                CaptureRegistry::new().with_portal_capture(portal_capture_config(&options), stream)
            }
            None => CaptureRegistry::new(),
        },
        crate::config::DesktopCaptureSelection::Fake => fake_capture_registry(config),
        crate::config::DesktopCaptureSelection::None => CaptureRegistry::new(),
    }
}

#[cfg(not(target_os = "linux"))]
fn desktop_capture_registry_common(config: &NodeConfig) -> CaptureRegistry {
    let selection = match config.desktop_providers() {
        Ok(providers) => providers.capture,
        Err(_) => return CaptureRegistry::new(),
    };
    match selection {
        crate::config::DesktopCaptureSelection::X11(x11) => {
            CaptureRegistry::new().with_x11_display(x11)
        }
        crate::config::DesktopCaptureSelection::Fake => fake_capture_registry(config),
        // PipeWire/portal are Linux-only; there is no honest non-Linux desktop
        // capture provider in this build.
        _ => CaptureRegistry::new(),
    }
}

#[cfg(target_os = "linux")]
fn stream_target_for(target: &crate::config::DesktopCaptureTarget) -> ozma_capture::StreamTarget {
    match target {
        crate::config::DesktopCaptureTarget::Default => ozma_capture::StreamTarget::Default,
        crate::config::DesktopCaptureTarget::Object(name) => {
            ozma_capture::StreamTarget::Object(name.clone())
        }
        crate::config::DesktopCaptureTarget::NodeId(id) => ozma_capture::StreamTarget::NodeId(*id),
    }
}

/// Translate the node's portal source choice into the capture crate's enum.
#[cfg(target_os = "linux")]
pub fn portal_source_type(kind: crate::config::PortalSourceKind) -> ozma_capture::PortalSourceType {
    match kind {
        crate::config::PortalSourceKind::Monitor => ozma_capture::PortalSourceType::Monitor,
        crate::config::PortalSourceKind::Window => ozma_capture::PortalSourceType::Window,
        crate::config::PortalSourceKind::Virtual => ozma_capture::PortalSourceType::Virtual,
    }
}

/// Translate the node's cursor choice into the capture crate's enum.
#[cfg(target_os = "linux")]
pub fn portal_cursor_mode(kind: crate::config::PortalCursorKind) -> ozma_capture::PortalCursorMode {
    match kind {
        crate::config::PortalCursorKind::Hidden => ozma_capture::PortalCursorMode::Hidden,
        crate::config::PortalCursorKind::Embedded => ozma_capture::PortalCursorMode::Embedded,
        crate::config::PortalCursorKind::Metadata => ozma_capture::PortalCursorMode::Metadata,
    }
}

/// Build the bounded portal consent/selection configuration.
#[cfg(target_os = "linux")]
pub fn portal_screencast_config(
    options: &crate::config::PortalCaptureOptions,
) -> ozma_capture::PortalConfig {
    ozma_capture::PortalConfig {
        source: portal_source_type(options.source),
        multiple: false,
        cursor_mode: portal_cursor_mode(options.cursor),
        restore_token: options.restore_token.clone(),
        response_timeout: options.timeout,
        // The availability probe must not outlive the user's configured bound.
        preflight_timeout: options
            .timeout
            .min(ozma_capture::PORTAL_DEFAULT_PREFLIGHT_TIMEOUT),
    }
}

/// Build the provider config for an already-selected portal stream.
#[cfg(target_os = "linux")]
pub fn portal_capture_config(
    options: &crate::config::PortalCaptureOptions,
) -> ozma_capture::PortalCaptureConfig {
    ozma_capture::PortalCaptureConfig::default()
        .with_identity(
            ozma_capture::PORTAL_DEFAULT_ID,
            ozma_capture::PORTAL_DEFAULT_NAME,
        )
        .with_mode(options.width, options.height, options.fps)
        .with_require_pipewire_fd(true)
}

/// Run the portal handshake and turn a denial/timeout/missing portal into a
/// warned no-capture state.
///
/// This is the only asynchronous capture step in the desktop profile. It runs
/// in `main` before the registry is built, never during enumeration. The
/// returned stream owns the portal D-Bus connection and `OpenPipeWireRemote` fd
/// for the provider's lifetime.
#[cfg(target_os = "linux")]
pub async fn acquire_portal_stream(
    portal: &ozma_capture::PortalScreenCast,
) -> Option<ozma_capture::PortalStream> {
    match portal.request_stream().await {
        Ok(stream) => Some(stream),
        Err(error) => {
            let api = error.to_api_error();
            tracing::warn!(
                code = %api.code,
                error = %error,
                "portal ScreenCast: consent was denied, timed out, or the portal is unavailable. \
                 The node keeps running without capture; no X11, camera, or fake source is \
                 substituted."
            );
            None
        }
    }
}

/// A dev/test-only synthetic RGB24 source.
///
/// This exists so the HLS/hardware-encode path can be validated on a host with
/// no real capture device. It is never a profile default and the hardware
/// profile rejects it in [`NodeConfig::vm_providers`], so it cannot silently
/// replace a missing V4L2/PipeWire/guest source in production.
fn fake_capture_registry(config: &NodeConfig) -> CaptureRegistry {
    let format = FrameFormat::rgb24(config.fake_width, config.fake_height);
    CaptureRegistry::new().with(
        FakeCaptureProvider::new()
            .with_identity(
                "fake0",
                format!("{} synthetic capture (dev/test)", config.name),
            )
            .with_format(format)
            .with_blocks()
            .with_fps(config.fake_fps),
    )
}

fn vm_capture_registry(config: &NodeConfig, shared: Option<&SharedQmpClient>) -> CaptureRegistry {
    let capture = config
        .vm_providers()
        .ok()
        .and_then(|providers| providers.capture);

    match capture.as_deref() {
        Some("fake") => fake_capture_registry(config),
        Some("vnc") => match (config.vnc_host.as_deref(), config.vnc_port) {
            (Some(host), Some(port)) => CaptureRegistry::new().with_vnc_display(
                VncDisplayConfig::new(host, port)
                    .with_identity(VNC_DEFAULT_ID, format!("{} VNC display", config.name))
                    .with_dimensions(config.vnc_width, config.vnc_height)
                    .with_fps(config.vnc_fps)
                    .with_ffmpeg_bin(config.ffmpeg_bin.clone()),
            ),
            // `NodeConfig::validate` and the resolver reject a partial
            // endpoint; never invent one here.
            _ => CaptureRegistry::new(),
        },
        Some("qmp") => match &config.qmp_socket {
            Some(socket) => {
                let display = QmpDisplayConfig::new(socket.clone())
                    .with_identity(QMP_DEFAULT_ID, format!("{} VM display", config.name))
                    .with_max_fps(QMP_DEFAULT_MAX_FPS);
                match shared {
                    Some(shared) => {
                        CaptureRegistry::new().with_qmp_display_shared(display, shared.clone())
                    }
                    None => CaptureRegistry::new().with_qmp_display(display),
                }
            }
            None => CaptureRegistry::new(),
        },
        Some("dbus") => match &config.qmp_socket {
            Some(socket) => {
                let display = DBusDisplayConfig::new(socket.clone())
                    .with_console_index(config.dbus_console)
                    .with_identity(DBUS_DEFAULT_ID, format!("{} D-Bus display", config.name))
                    .with_limits(config.dbus_width, config.dbus_height)
                    .with_fps(config.dbus_fps);
                match shared {
                    Some(shared) => {
                        CaptureRegistry::new().with_dbus_display_shared(display, shared.clone())
                    }
                    None => CaptureRegistry::new().with_dbus_display(display),
                }
            }
            None => CaptureRegistry::new(),
        },
        // GPU passthrough: QEMU publishes the guest framebuffer through an
        // IVSHMEM region instead of an emulated display. Discovery reads only
        // the KVMFR header; a missing file, foreign magic, or a region without
        // a published frame yields no source (never a synthetic frame). The
        // provider is registered explicitly here, not in `system*`.
        Some("looking-glass") => match &config.looking_glass_shm {
            Some(shm) => CaptureRegistry::new().with_looking_glass(
                LookingGlassProviderConfig::new(shm.clone())
                    .with_identity(
                        config.looking_glass_id.clone(),
                        config.looking_glass_name.clone(),
                    )
                    .with_fps(config.looking_glass_fps),
            ),
            None => CaptureRegistry::new(),
        },
        _ => CaptureRegistry::new(),
    }
}

/// Largest encode width this node advertises from a synthetic or real source.
///
/// The Gate 2 live report measured 4K (3840x2160) software, VAAPI, and NVENC
/// HLS encodes. Larger sources are real capture input, but not part of the
/// probed encode envelope.
pub const MAX_ENCODE_WIDTH: u32 = 3_840;
/// Largest encode height this node advertises.
pub const MAX_ENCODE_HEIGHT: u32 = 2_160;
/// Highest encode frame rate this node advertises.
pub const MAX_ENCODE_FPS: u32 = 60;

/// The real capture envelope an encoder may be advertised against.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct EncodeEnvelope {
    /// Widest source, capped at [`MAX_ENCODE_WIDTH`].
    pub width: u32,
    /// Tallest source, capped at [`MAX_ENCODE_HEIGHT`].
    pub height: u32,
    /// Highest source frame rate, capped at [`MAX_ENCODE_FPS`].
    pub fps: u32,
}

impl EncodeEnvelope {
    /// Conservative fallback when a source has not reported a mode yet.
    pub const fn fallback() -> Self {
        Self {
            width: 1_920,
            height: 1_080,
            fps: 30,
        }
    }
}

/// Derive the advertised encode envelope from capture capabilities.
///
/// A 4K synthetic source becomes a 4K encode envelope; a 1080p camera stays
/// 1080p. This is deliberately a source bound, not a hard-coded resolution.
pub fn capture_envelope(captures: &[ozma_protocol::CaptureCapability]) -> EncodeEnvelope {
    let mut envelope = EncodeEnvelope {
        width: 0,
        height: 0,
        fps: 0,
    };
    for capture in captures {
        let area = u64::from(capture.max_width) * u64::from(capture.max_height);
        let current_area = u64::from(envelope.width) * u64::from(envelope.height);
        if area > current_area {
            envelope.width = capture.max_width;
            envelope.height = capture.max_height;
        }
        envelope.fps = envelope.fps.max(capture.max_fps);
    }
    if envelope.width == 0 || envelope.height == 0 {
        return EncodeEnvelope::fallback();
    }
    EncodeEnvelope {
        width: envelope.width.clamp(1, MAX_ENCODE_WIDTH),
        height: envelope.height.clamp(1, MAX_ENCODE_HEIGHT),
        fps: envelope.fps.clamp(1, MAX_ENCODE_FPS),
    }
}

/// Build the capability for the encoder HLS is configured to run.
pub fn hls_encode_capability(config: &HlsConfig, envelope: EncodeEnvelope) -> EncodeCapability {
    let encoder = config.video_encoder;
    let (codec, backend) = match encoder {
        crate::hls::HlsEncoder::H264 => (Codec::H264, EncoderBackend::Software),
        crate::hls::HlsEncoder::Hevc => (Codec::Hevc, EncoderBackend::Software),
        crate::hls::HlsEncoder::Av1Svt | crate::hls::HlsEncoder::Av1Aom => {
            (Codec::Av1, EncoderBackend::Software)
        }
        crate::hls::HlsEncoder::Mjpeg => (Codec::Mjpeg, EncoderBackend::Software),
        crate::hls::HlsEncoder::VaapiH264 => (Codec::H264, EncoderBackend::Vaapi),
        crate::hls::HlsEncoder::VaapiHevc => (Codec::Hevc, EncoderBackend::Vaapi),
        crate::hls::HlsEncoder::NvencH264 => (Codec::H264, EncoderBackend::Nvenc),
        crate::hls::HlsEncoder::NvencHevc => (Codec::Hevc, EncoderBackend::Nvenc),
        crate::hls::HlsEncoder::QsvH264 => (Codec::H264, EncoderBackend::Qsv),
        crate::hls::HlsEncoder::QsvHevc => (Codec::Hevc, EncoderBackend::Qsv),
        crate::hls::HlsEncoder::V4l2m2mH264 => (Codec::H264, EncoderBackend::V4l2M2m),
        crate::hls::HlsEncoder::V4l2m2mHevc => (Codec::Hevc, EncoderBackend::V4l2M2m),
    };
    let ten_bit = config.pixel_format.is_10bit();
    let hdr = config.hdr && ten_bit;
    let profiles = match codec {
        Codec::Hevc if ten_bit => vec!["main10".to_owned()],
        Codec::Hevc => vec!["main".to_owned()],
        Codec::Av1 if ten_bit => vec!["main".to_owned(), "main10".to_owned()],
        Codec::Av1 => vec!["main".to_owned()],
        Codec::H264 => vec!["high".to_owned()],
        Codec::Mjpeg => vec!["baseline".to_owned()],
        _ => Vec::new(),
    };
    EncodeCapability {
        codec,
        backend,
        hardware: encoder.is_hardware(),
        max_width: envelope.width,
        max_height: envelope.height,
        max_fps: envelope.fps.min(config.fps.max(1)),
        hdr,
        profiles,
    }
}

/// Fit advertised encoder capabilities to the real source envelope.
///
/// Software, VAAPI, and NVENC have live 4K evidence, so their source bound is
/// raised to the capture envelope (up to 4K). QSV/V4L2M2M keep their earlier
/// bound because this tree has no 4K evidence for them. HDR is only set for
/// the configured HLS codec family, and only after `HlsConfig::validate`
/// accepted a Main10 encoder/format combination.
pub fn fit_encode_capabilities(
    capabilities: &mut [EncodeCapability],
    envelope: EncodeEnvelope,
    hdr_codec: Option<Codec>,
) {
    for capability in capabilities {
        let proven_4k = matches!(
            capability.backend,
            EncoderBackend::Software | EncoderBackend::Vaapi | EncoderBackend::Nvenc
        );
        if proven_4k {
            capability.max_width = envelope.width;
            capability.max_height = envelope.height;
        } else {
            capability.max_width = capability.max_width.min(envelope.width);
            capability.max_height = capability.max_height.min(envelope.height);
        }
        capability.max_fps = if capability.max_fps == 0 {
            envelope.fps
        } else {
            capability.max_fps.min(envelope.fps)
        };
        if hdr_codec == Some(capability.codec) && proven_4k {
            capability.hdr = true;
            if !capability
                .profiles
                .iter()
                .any(|profile| profile == "main10")
            {
                capability.profiles.push("main10".to_owned());
            }
        }
    }
}

/// Replace/add the configured HLS encoder and fit the whole advertised list to
/// the real capture envelope.
pub fn apply_hls_encode_capability(caps: &mut NodeCapabilities, config: &HlsConfig) {
    let envelope = capture_envelope(&caps.capture);
    let hls_capability = hls_encode_capability(config, envelope);
    caps.encode.retain(|capability| {
        capability.codec != hls_capability.codec || capability.backend != hls_capability.backend
    });
    caps.encode.push(hls_capability.clone());
    let hdr_codec = (config.hdr && config.pixel_format.is_10bit()).then_some(hls_capability.codec);
    fit_encode_capabilities(&mut caps.encode, envelope, hdr_codec);
}

/// Build the capability set advertised by this node, probing the host.
pub fn detect(config: &NodeConfig) -> NodeCapabilities {
    detect_with_registry(config, &capture_registry(config))
}

/// Build capabilities with an injected capture registry, for tests and
/// unusual deployments.
///
/// Input is reported separately (see [`detect_with_registry_and_input`]) because
/// only the node knows which sink actually opened. This form reports no
/// keyboard/pointer injection rather than claiming the protocol's fake input
/// set.
pub fn detect_with_registry(config: &NodeConfig, capture: &CaptureRegistry) -> NodeCapabilities {
    let mut caps = detect_base(config, capture);
    caps.input = InputCapability::default();
    caps
}

/// Build capabilities and report the input path that actually opened.
///
/// `sink` is the selected sink: uinput/QMP/HID gadget advertise only the
/// keyboard and pointer modes that sink implements, and the recording-only
/// fallback advertises no injection at all.
pub fn detect_with_registry_and_input(
    config: &NodeConfig,
    capture: &CaptureRegistry,
    sink: Option<SinkKind>,
) -> NodeCapabilities {
    let mut caps = detect_base(config, capture);
    caps.input = input_capability_for(sink);
    caps
}

/// Truthful input capabilities for a selected sink.
pub fn input_capability_for(sink: Option<SinkKind>) -> InputCapability {
    match sink {
        Some(SinkKind::Uinput) => InputCapability {
            keyboard: true,
            // `UinputSink::open` uses the default absolute pointer device; it
            // rejects relative events, so do not advertise them.
            mouse_absolute: true,
            mouse_relative: false,
            release_all: true,
            ..InputCapability::default()
        },
        Some(SinkKind::Qmp) | Some(SinkKind::HidGadget) => InputCapability {
            keyboard: true,
            mouse_absolute: true,
            mouse_relative: true,
            release_all: true,
            ..InputCapability::default()
        },
        // A recording sink validates events but injects nothing.
        Some(SinkKind::Recording) | None => InputCapability::default(),
    }
}

fn detect_base(config: &NodeConfig, capture: &CaptureRegistry) -> NodeCapabilities {
    let mut caps = NodeCapabilities::fake(config.node_id.clone());
    caps.name = config.name.clone();
    caps.platform = config.hw.clone();
    caps.node_id = config.node_id.clone();

    if !config.has_capability("video") {
        caps.capture.clear();
        caps.encode.clear();
        caps.display = VirtualDisplayCaps::none();
    } else {
        let discovered = capture.enumerate().unwrap_or_default();
        caps.capture = discovered
            .into_iter()
            .map(|mut source| {
                // Discovery reads sysfs, which does not expose modes. Advertise
                // a conservative ceiling until the source is opened, rather
                // than pretending a 0x0 mode is usable. Looking Glass is the
                // exception: a placeholder KVMFR region has no published frame
                // yet, so its unknown geometry stays 0x0 instead of becoming an
                // invented 1080p mode.
                if source.kind != CaptureKind::LookingGlass {
                    if source.max_width == 0 {
                        source.max_width = 1920;
                    }
                    if source.max_height == 0 {
                        source.max_height = 1080;
                    }
                }
                if source.max_fps == 0 {
                    source.max_fps = 30;
                }
                source.has_audio = config.has_capability("audio");
                source
            })
            .collect();
        if caps.capture.is_empty() {
            caps.encode.clear();
            caps.display = VirtualDisplayCaps::none();
        } else {
            // Advertise only the encoder this build can actually run. A
            // hardware adapter must be added here after its probe passes,
            // never assumed from the platform name. Fit its envelope to the
            // discovered source instead of a hard-coded 1080p ceiling.
            caps.encode = vec![ozma_encode::H264SoftwareEncoder::capability()];
            let envelope = capture_envelope(&caps.capture);
            fit_encode_capabilities(&mut caps.encode, envelope, None);
        }
        // The real Linux EVDI/KMS provider is still gated on the M2 spike.
        // Reporting the fake provider here would claim a monitor the node
        // cannot actually create, so advertise no virtual display until a
        // platform provider passes its probe.
        caps.display = VirtualDisplayCaps::none();
    }

    if !config.has_capability("audio") {
        caps.audio = AudioCapability::default();
    }

    if !config.has_capability("hid") {
        caps.input = InputCapability::default();
    }

    // The `usb` capability string is only configuration intent. The node
    // advertises USB after it actually creates and binds a gadget in `main`;
    // a bare `cap` entry must never claim ConfigFS support that failed to
    // initialise.
    caps.usb = UsbCapability::default();

    if config
        .vm_providers()
        .map(|providers| providers.power.is_some())
        .unwrap_or(false)
    {
        caps.power = PowerCapability {
            reset: true,
            power_off: true,
            ..PowerCapability::default()
        };
    }

    caps.transport = vec![
        TransportMode::Loopback,
        TransportMode::Quic,
        TransportMode::TcpInterleaved,
    ];
    caps.budget = ResourceBudget {
        max_sessions: 1,
        max_tracks: 1,
        max_bitrate_kbps: 8_000,
        // A multi-console VM advertises every head it knows about. This is a
        // layout/selection count, not a claim that each head has a live capture
        // source (only the selected console does).
        max_remote_displays: u16::try_from(config.display_output_count().max(1))
            .unwrap_or(u16::MAX),
    };
    caps
}

/// Build a deterministic test capability set without touching hardware.
pub fn fake() -> NodeCapabilities {
    NodeCapabilities::fake(NodeId::new("test-node"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_capture::FakeCaptureProvider;

    fn config(caps: &str) -> NodeConfig {
        NodeConfig {
            name: "node".into(),
            node_id: NodeId::new("node._ozma._udp.local."),
            hid_udp_port: 7331,
            http_port: 7382,
            rtsp_port: 0,
            capabilities: caps.split(',').map(str::to_owned).collect(),
            hw: "test".into(),
            fw: "0.1.0".into(),
            register_url: None,
            register_host: None,
            heartbeat_interval_secs: 30,
            role: "compute".into(),
            machine_class: ozma_auth::MachineClass::Server,
            workstation_consent: false,
            api_token: None,
            profile: crate::config::NodeProfile::Hardware,
            providers: crate::config::ProviderSelection::for_profile(
                crate::config::NodeProfile::Hardware,
            ),
            provider_overrides: crate::config::ProviderOverrides::default(),
            capture_device: None,
            qmp_socket: None,
            qmp_input_socket: None,
            libvirt_domain: None,
            libvirt_uri: None,
            qmp_connect_timeout: std::time::Duration::from_millis(3_000),
            qmp_command_timeout: std::time::Duration::from_millis(5_000),
            vnc_host: None,
            vnc_port: None,
            vnc_width: ozma_capture::VNC_DEFAULT_WIDTH,
            vnc_height: ozma_capture::VNC_DEFAULT_HEIGHT,
            vnc_fps: ozma_capture::VNC_DEFAULT_FPS,
            vnc_server: false,
            vnc_bind: None,
            vnc_server_port: 0,
            vnc_password_file: None,
            vnc_allow_unauthenticated: false,
            vnc_max_clients: crate::vnc_server::DEFAULT_MAX_CLIENTS,
            vnc_idle_timeout_seconds: 0,
            dbus_console: 0,
            dbus_width: ozma_capture::DBUS_DEFAULT_WIDTH,
            dbus_height: ozma_capture::DBUS_DEFAULT_HEIGHT,
            dbus_fps: ozma_capture::DBUS_DEFAULT_FPS,
            dbus_requested: false,
            capture_console: None,
            display_output_specs: Vec::new(),
            display_outputs: Vec::new(),
            looking_glass_shm: None,
            looking_glass_fps: ozma_capture::LOOKING_GLASS_DEFAULT_FPS,
            looking_glass_id: ozma_capture::LOOKING_GLASS_DEFAULT_ID.to_owned(),
            looking_glass_name: ozma_capture::LOOKING_GLASS_DEFAULT_NAME.to_owned(),
            fake_width: crate::config::DEFAULT_FAKE_WIDTH,
            fake_height: crate::config::DEFAULT_FAKE_HEIGHT,
            fake_fps: crate::config::DEFAULT_FAKE_FPS,
            desktop_capture_width: crate::config::DEFAULT_FAKE_WIDTH,
            desktop_capture_height: crate::config::DEFAULT_FAKE_HEIGHT,
            desktop_capture_fps: crate::config::DEFAULT_FAKE_FPS,
            desktop_capture_target: crate::config::DesktopCaptureTarget::Default,
            desktop_capture_remote: None,
            x11_display: None,
            x11_x: 0,
            x11_y: 0,
            x11_width: ozma_capture::X11_DEFAULT_WIDTH,
            x11_height: ozma_capture::X11_DEFAULT_HEIGHT,
            x11_fps: ozma_capture::X11_DEFAULT_FPS,
            x11_requested: false,
            portal_source_type: crate::config::PortalSourceKind::Monitor,
            portal_cursor: crate::config::PortalCursorKind::Hidden,
            portal_restore_token: None,
            portal_timeout: ozma_capture::PORTAL_DEFAULT_RESPONSE_TIMEOUT,
            portal_requested: false,
            audio_capture_target: ozma_audio::AudioTarget::Default,
            audio_router: crate::config::AudioRouterKind::None,
            audio_track_root: None,
            ffmpeg_bin: std::path::PathBuf::from("ffmpeg"),
            vaapi_device: std::path::PathBuf::from("/dev/dri/renderD128"),
            nvenc_gpu: None,
            audio_sink: None,
            webrtc_audio_source: None,
            webrtc_audio_backend: crate::webrtc::WebRtcAudioBackend::Auto,
            vm_guest_ip: None,
            display_outputs_json: "[]".into(),
            pci_devices_json: "[]".into(),
            usb_gadget: false,
            usb_configfs_root: std::path::PathBuf::from(crate::config::DEFAULT_USB_CONFIGFS_ROOT),
            usb_udc: None,
            usb_media: None,
            usb_media_ro: false,
            usb_media_cdrom: false,
            usb_media_export: None,
            usb_media_export_format: "raw".to_owned(),
            usb_media_export_read_write: false,
            usb_media_export_persist: None,
            usb_media_export_persist_on_flush: true,
            usb_media_export_persist_on_stop: true,
            usb_media_export_reload: false,
            usb_media_export_timeout_ms: None,
            usb_media_export_max_transfer_bytes: None,
            usb_media_restart_max_attempts: ozma_usb::USB_MEDIA_RESTART_DEFAULT_MAX_ATTEMPTS,
            usb_media_restart_backoff_ms: ozma_usb::USB_MEDIA_RESTART_DEFAULT_BACKOFF_MS,
            usb_media_restart_max_backoff_ms: ozma_usb::USB_MEDIA_RESTART_DEFAULT_MAX_BACKOFF_MS,
            usb_media_restart_reload_overlay: true,
            usb_uac2: false,
            usb_vendor: false,
            usb_vendor_daemon: None,
            usb_functionfs_mount: None,
            usbip: false,
            usbip_host: "127.0.0.1".to_owned(),
            usbip_port: ozma_usb::DEFAULT_USBIP_PORT,
            usbip_linux_adapters: false,
            usbip_sysfs_root: None,
            usbip_program: None,
            usb_uac2_bridge: false,
            usb_uac2_sink_device: "default".to_owned(),
            usb_uac2_source_device: "default".to_owned(),
        }
    }

    #[test]
    fn usb_is_not_advertised_from_the_capability_string_alone() {
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new());
        let caps = detect_with_registry(&config("hid,video,audio,usb"), &registry);
        // main.rs only sets this after a gadget is created and bound. A bare
        // `cap` entry (or platform profile) must not claim USB support.
        assert!(!caps.usb.gadget);
        assert!(!caps.usb.virtual_media);
        assert!(!caps.usb.usbip);
    }

    #[test]
    fn video_capability_adds_capture_and_encoder() {
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new());
        let caps = detect_with_registry(&config("hid,video,audio"), &registry);
        assert!(!caps.capture.is_empty());
        assert!(!caps.encode.is_empty());
        assert!(!caps.display.supported);
        // A 320x240 fake source must not be advertised as 1080p or 4K.
        assert!(caps.encode.iter().all(|capability| {
            capability.max_width == caps.capture[0].max_width
                && capability.max_height == caps.capture[0].max_height
                && capability.max_width < MAX_ENCODE_WIDTH
        }));
    }

    #[test]
    fn no_video_means_no_capture_or_display() {
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new());
        let caps = detect_with_registry(&config("hid,audio"), &registry);
        assert!(caps.capture.is_empty());
        assert!(caps.encode.is_empty());
        assert!(!caps.display.supported);
    }

    #[test]
    fn vm_profile_uses_only_the_qmp_capture_source() {
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;

        // No QMP socket: the VM must not advertise host hardware capture.
        let registry = capture_registry(&vm);
        assert_eq!(registry.provider_count(), 0);
        let caps = detect_with_registry(&vm, &registry);
        assert!(caps.capture.is_empty());
        assert!(caps.encode.is_empty());
        assert!(!caps.display.supported);

        // With a QMP socket the registry owns exactly one provider.
        vm.qmp_socket = Some(std::path::PathBuf::from("/run/ozma/ctrl.qmp"));
        let registry = capture_registry(&vm);
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].kind, ozma_protocol::CaptureKind::Qmp);
    }

    #[test]
    fn configured_power_backend_advertises_power_capability() {
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;
        vm.qmp_socket = Some(std::path::PathBuf::from("/run/ozma/ctrl.qmp"));

        let caps = detect_with_registry(&vm, &CaptureRegistry::new());
        assert!(caps.power.reset);
        assert!(caps.power.power_off);

        vm.qmp_socket = None;
        let caps = detect_with_registry(&vm, &CaptureRegistry::new());
        assert!(!caps.power.reset);
        assert!(!caps.power.power_off);
    }

    #[test]
    fn vm_profile_uses_only_the_vnc_capture_source_when_selected() {
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;
        vm.vnc_host = Some("127.0.0.1".into());
        vm.vnc_port = Some(5901);
        vm.vnc_width = 1280;
        vm.vnc_height = 720;
        vm.vnc_fps = 15;

        let registry = capture_registry(&vm);
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].kind, ozma_protocol::CaptureKind::Vnc);
        assert_eq!(scans[0].capabilities.len(), 1);
        let capability = &scans[0].capabilities[0];
        assert_eq!(capability.id, ozma_capture::VNC_DEFAULT_ID);
        assert_eq!((capability.max_width, capability.max_height), (1280, 720));
        assert_eq!(capability.max_fps, 15);
        assert_eq!(capability.formats, vec!["rgb24".to_owned()]);

        // The advertised capability reaches NodeCapabilities unchanged.
        let caps = detect_with_registry(&vm, &registry);
        assert_eq!(caps.capture.len(), 1);
        assert_eq!(caps.capture[0].kind, ozma_protocol::CaptureKind::Vnc);

        // An incomplete endpoint never becomes a registry source.
        vm.vnc_port = None;
        assert_eq!(capture_registry(&vm).provider_count(), 0);
    }

    #[test]
    fn vm_profile_uses_only_the_dbus_capture_source_when_selected() {
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;
        vm.qmp_socket = Some(std::path::PathBuf::from("/run/ozma/ctrl.qmp"));
        vm.dbus_requested = true;
        vm.dbus_console = 2;
        vm.dbus_width = 1280;
        vm.dbus_height = 720;
        vm.dbus_fps = 15;

        let socket = std::env::temp_dir().join(format!(
            "ozma-dbus-registry-{}-{}.qmp",
            std::process::id(),
            "platform"
        ));
        std::fs::write(&socket, b"").unwrap();
        vm.qmp_socket = Some(socket.clone());

        let registry = capture_registry(&vm);
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].kind, ozma_protocol::CaptureKind::Dbus);
        assert_eq!(scans[0].capabilities.len(), 1);
        let capability = &scans[0].capabilities[0];
        assert_eq!(capability.id, ozma_capture::DBUS_DEFAULT_ID);
        assert_eq!((capability.max_width, capability.max_height), (1280, 720));
        assert_eq!(capability.max_fps, 15);
        assert_eq!(capability.formats, vec!["bgra".to_owned()]);

        let caps = detect_with_registry(&vm, &registry);
        assert_eq!(caps.capture.len(), 1);
        assert_eq!(caps.capture[0].kind, ozma_protocol::CaptureKind::Dbus);

        let _ = std::fs::remove_file(socket);
    }

    fn kvmfr_fixture(magic: bool) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "ozma-kvmfr-{}-{}.shm",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let mut region = vec![0u8; ozma_capture::lookingglass::FRAME_HEADER_OFFSET + 128];
        if magic {
            region[..8].copy_from_slice(b"KVMFR---");
            region[8..12].copy_from_slice(&ozma_capture::lookingglass::KVMFR_VERSION.to_le_bytes());
            region[12..16].copy_from_slice(&0u32.to_le_bytes());
        }
        std::fs::write(&path, &region).unwrap();
        path
    }

    #[test]
    fn vm_profile_registers_looking_glass_only_for_a_valid_kvmfr_region() {
        let path = kvmfr_fixture(true);
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;
        vm.looking_glass_shm = Some(path.clone());

        let registry = capture_registry(&vm);
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].kind, ozma_protocol::CaptureKind::LookingGlass);
        assert!(scans[0].error.is_none(), "{:?}", scans[0].error);
        assert_eq!(scans[0].capabilities.len(), 1);
        let capability = &scans[0].capabilities[0];
        assert_eq!(capability.id, ozma_capture::LOOKING_GLASS_DEFAULT_ID);
        // A placeholder KVMFR region has no published frame, so geometry stays
        // unknown instead of being guessed.
        assert!((capability.max_width, capability.max_height) == (0, 0));
        assert!(capability.formats.is_empty());

        let caps = detect_with_registry(&vm, &registry);
        assert_eq!(caps.capture.len(), 1);
        assert_eq!(
            caps.capture[0].kind,
            ozma_protocol::CaptureKind::LookingGlass
        );
        // The placeholder geometry is not upgraded to a guessed 1080p mode.
        assert_eq!(
            (caps.capture[0].max_width, caps.capture[0].max_height),
            (0, 0)
        );

        // An absent region is "no source", not a hardware claim.
        vm.looking_glass_shm = Some(path.with_extension("absent"));
        let scans = capture_registry(&vm).scan();
        assert_eq!(scans.len(), 1);
        assert!(scans[0].capabilities.is_empty());
        assert!(scans[0].error.is_none());
        let caps = detect_with_registry(&vm, &CaptureRegistry::new());
        assert!(caps.capture.is_empty());
        assert!(caps.encode.is_empty());

        // A foreign magic is also ignored rather than advertised.
        let foreign = kvmfr_fixture(false);
        vm.looking_glass_shm = Some(foreign.clone());
        let scans = capture_registry(&vm).scan();
        assert_eq!(scans.len(), 1);
        assert!(scans[0].capabilities.is_empty());

        let _ = std::fs::remove_file(path);
        let _ = std::fs::remove_file(foreign);
    }

    #[test]
    fn multi_console_display_outputs_raise_the_remote_display_budget() {
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;
        vm.qmp_socket = Some(std::path::PathBuf::from("/run/ozma/ctrl.qmp"));
        vm.display_output_specs = vec![
            "0:Console_0:1920x1080:primary:qmp".into(),
            "1:Console_1:1280x1024:qmp".into(),
        ];
        let selection = vm.resolve_display_outputs().unwrap();
        let json = selection.to_json();
        vm.display_outputs = selection.outputs;
        vm.display_outputs_json = json;

        let caps = detect_with_registry(&vm, &CaptureRegistry::new());
        assert_eq!(caps.budget.max_remote_displays, 2);
    }

    #[test]
    fn vm_fake_capture_advertises_the_requested_dev_geometry() {
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;
        vm.provider_overrides = crate::config::ProviderOverrides::from_cli("fake", "", "", "", "");
        vm.providers = crate::config::ProviderSelection::for_profile(vm.profile)
            .with_overrides(&vm.provider_overrides);
        vm.fake_width = 3_840;
        vm.fake_height = 2_160;
        vm.fake_fps = 30;

        let registry = capture_registry(&vm);
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].kind, ozma_protocol::CaptureKind::Fake);
        assert_eq!(scans[0].capabilities.len(), 1);
        let capability = &scans[0].capabilities[0];
        assert_eq!(capability.id, "fake0");
        assert!(capability.name.contains("dev/test"));
        assert_eq!(
            (capability.max_width, capability.max_height),
            (3_840, 2_160)
        );
        assert_eq!(capability.max_fps, 30);
        assert_eq!(capability.formats, vec!["rgb24".to_owned()]);
        assert_eq!(capability.kind, ozma_protocol::CaptureKind::Fake);

        // The synthetic geometry reaches NodeCapabilities, and the fake source
        // opens with exactly the advertised frame format.
        let caps = detect_with_registry(&vm, &registry);
        assert_eq!(caps.capture.len(), 1);
        assert_eq!(caps.capture[0].max_width, 3_840);
        // The encode envelope follows the real source instead of hard-coding
        // 1080p, but a 4K size alone must not claim HDR.
        assert!(caps.encode.iter().all(|capability| {
            capability.max_width == 3_840 && capability.max_height == 2_160 && !capability.hdr
        }));
        let source = registry
            .open(&caps.capture[0])
            .expect("fake source must open");
        assert_eq!(
            source.format(),
            ozma_media::FrameFormat::rgb24(3_840, 2_160)
        );
        assert_eq!(source.id(), "fake0");
    }

    #[test]
    fn hls_capability_reflects_configured_ten_bit_hdr_only() {
        let mut vm = config("hid,video,audio");
        vm.profile = crate::config::NodeProfile::Vm;
        vm.provider_overrides = crate::config::ProviderOverrides::from_cli("fake", "", "", "", "");
        vm.providers = crate::config::ProviderSelection::for_profile(vm.profile)
            .with_overrides(&vm.provider_overrides);
        vm.fake_width = 3_840;
        vm.fake_height = 2_160;
        let registry = capture_registry(&vm);
        let mut caps = detect_with_registry(&vm, &registry);
        assert!(caps.encode.iter().all(|capability| !capability.hdr));

        let sdr = HlsConfig {
            video_encoder: crate::hls::HlsEncoder::Hevc,
            ..HlsConfig::default()
        };
        apply_hls_encode_capability(&mut caps, &sdr);
        let hevc = caps
            .encode
            .iter()
            .find(|capability| capability.codec == Codec::Hevc)
            .expect("configured HEVC HLS capability");
        assert_eq!((hevc.max_width, hevc.max_height), (3_840, 2_160));
        assert!(!hevc.hdr);
        assert!(hevc.profiles.iter().any(|profile| profile == "main"));

        let hdr = HlsConfig {
            video_encoder: crate::hls::HlsEncoder::Hevc,
            pixel_format: crate::hls::HlsPixelFormat::P010,
            hdr: true,
            ..HlsConfig::default()
        };
        apply_hls_encode_capability(&mut caps, &hdr);
        let hevc = caps
            .encode
            .iter()
            .find(|capability| capability.codec == Codec::Hevc)
            .expect("configured HEVC HLS capability");
        assert!(hevc.hdr);
        assert!(hevc.profiles.iter().any(|profile| profile == "main10"));
        // H.264 has no Main10 mapping, so it must stay non-HDR.
        assert!(caps
            .encode
            .iter()
            .filter(|capability| capability.codec == Codec::H264)
            .all(|capability| !capability.hdr));
    }

    #[test]
    fn encode_envelope_is_capped_to_the_probed_four_k_maximum() {
        let envelope = capture_envelope(&[ozma_protocol::CaptureCapability {
            id: "fake0".to_owned(),
            name: "8K synthetic".to_owned(),
            kind: ozma_protocol::CaptureKind::Fake,
            formats: vec!["rgb24".to_owned()],
            max_width: 8_192,
            max_height: 4_320,
            max_fps: 240,
            compressed: false,
            hdr: false,
            has_audio: false,
        }]);
        assert_eq!(
            envelope,
            EncodeEnvelope {
                width: MAX_ENCODE_WIDTH,
                height: MAX_ENCODE_HEIGHT,
                fps: MAX_ENCODE_FPS,
            }
        );
    }

    #[test]
    fn hardware_profile_never_advertises_vnc() {
        let hardware = config("hid,video,audio");
        let registry = capture_registry(&hardware);
        assert!(registry
            .scan()
            .iter()
            .all(|scan| scan.kind != ozma_protocol::CaptureKind::Vnc));
    }

    #[test]
    fn desktop_profile_registers_pipewire_only() {
        let desktop = config("hid,video,audio");
        let mut desktop = desktop;
        desktop.profile = crate::config::NodeProfile::Desktop;
        desktop.providers = crate::config::ProviderSelection::for_profile(desktop.profile);

        let registry = capture_registry(&desktop);
        #[cfg(target_os = "linux")]
        assert_eq!(registry.provider_count(), 1);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(registry.provider_count(), 0);

        // The explicit dev/test source still works and stays labelled Fake.
        desktop.provider_overrides =
            crate::config::ProviderOverrides::from_cli("fake", "", "", "", "");
        desktop.providers = crate::config::ProviderSelection::for_profile(desktop.profile)
            .with_overrides(&desktop.provider_overrides);
        let registry = capture_registry(&desktop);
        assert_eq!(registry.provider_count(), 1);
        assert_eq!(registry.scan()[0].kind, ozma_protocol::CaptureKind::Fake);
    }

    #[test]
    fn desktop_capabilities_follow_the_selected_input_sink() {
        let mut desktop = config("hid,video,audio");
        desktop.profile = crate::config::NodeProfile::Desktop;
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new());

        let uinput = detect_with_registry_and_input(&desktop, &registry, Some(SinkKind::Uinput));
        assert!(uinput.input.keyboard);
        assert!(uinput.input.mouse_absolute);
        assert!(!uinput.input.mouse_relative);
        assert!(uinput.input.release_all);
        assert!(!uinput.input.clipboard);

        let recording =
            detect_with_registry_and_input(&desktop, &registry, Some(SinkKind::Recording));
        assert_eq!(recording.input, InputCapability::default());
        assert!(!recording.input.keyboard);

        // Without a selected sink, detect_with_registry must not claim the
        // protocol's fake input set.
        let unknown = detect_with_registry(&desktop, &registry);
        assert_eq!(unknown.input, InputCapability::default());
    }

    #[test]
    fn desktop_without_capture_reports_no_video_or_display() {
        let mut desktop = config("hid,video,audio");
        desktop.profile = crate::config::NodeProfile::Desktop;
        let caps = detect_with_registry(&desktop, &CaptureRegistry::new());
        assert!(caps.capture.is_empty());
        assert!(caps.encode.is_empty());
        assert!(!caps.display.supported);
    }

    fn desktop_with_provider(provider: &str) -> NodeConfig {
        let mut desktop = config("hid,video,audio");
        desktop.profile = crate::config::NodeProfile::Desktop;
        desktop.provider_overrides =
            crate::config::ProviderOverrides::from_cli(provider, "", "", "", "");
        desktop.providers = crate::config::ProviderSelection::for_profile(desktop.profile)
            .with_overrides(&desktop.provider_overrides);
        desktop
    }

    #[test]
    fn desktop_x11_registry_advertises_x11_kind_and_geometry() {
        let mut desktop = desktop_with_provider("x11");
        desktop.x11_display = Some(":0".into());
        desktop.x11_requested = true;
        desktop.x11_x = 4;
        desktop.x11_y = 8;
        desktop.x11_width = 320;
        desktop.x11_height = 240;
        desktop.x11_fps = 12;

        let registry = capture_registry(&desktop);
        assert_eq!(registry.provider_count(), 1);
        let scans = registry.scan();
        assert_eq!(scans.len(), 1);
        assert_eq!(scans[0].kind, ozma_protocol::CaptureKind::X11);
        assert!(scans[0].error.is_none());
        assert_eq!(scans[0].capabilities.len(), 1);
        let capability = &scans[0].capabilities[0];
        assert_eq!(capability.id, ozma_capture::X11_DEFAULT_ID);
        assert_eq!(capability.kind, ozma_protocol::CaptureKind::X11);
        assert_eq!((capability.max_width, capability.max_height), (320, 240));
        assert_eq!(capability.max_fps, 12);
        assert_eq!(capability.formats, vec!["rgb24".to_owned()]);

        // The advertised capability is a real desktop capture source.
        let caps = detect_with_registry(&desktop, &registry);
        assert_eq!(caps.capture.len(), 1);
        assert!(!caps.encode.is_empty());
    }

    #[test]
    fn desktop_x11_missing_display_is_a_configuration_error_not_a_registry_claim() {
        let desktop = desktop_with_provider("x11");
        let error = desktop.desktop_providers().unwrap_err();
        assert!(error.contains("--x11-display"), "{error}");
        assert!(desktop.validate().is_err());

        // Even if a caller builds the registry without validating first, it
        // advertises nothing rather than an ambient or fake X display.
        let registry = capture_registry(&desktop);
        assert_eq!(registry.provider_count(), 0);
    }

    #[tokio::test]
    async fn desktop_x11_missing_ffmpeg_is_a_typed_capture_failure() {
        let mut desktop = desktop_with_provider("x11");
        desktop.x11_display = Some(":0".into());
        desktop.x11_requested = true;
        desktop.ffmpeg_bin = std::path::PathBuf::from("/nonexistent/ozma-ffmpeg");
        let registry = capture_registry(&desktop);
        let capability = registry.scan()[0].capabilities[0].clone();
        let mut source = registry.open(&capability).expect("source opens lazily");
        let error = source
            .next_frame()
            .await
            .expect_err("a missing ffmpeg must not produce a frame");
        let message = error.to_string();
        assert!(message.contains("could not start ffmpeg"), "{message}");
        assert!(message.contains("/nonexistent/ozma-ffmpeg"), "{message}");
    }

    #[cfg(target_os = "linux")]
    mod portal_tests {
        use super::*;
        use std::os::fd::OwnedFd;
        use std::sync::Arc;
        use std::time::Duration;

        use async_trait::async_trait;
        use ozma_capture::{
            PortalConfig, PortalError, PortalResults, PortalScreenCast, PortalSourceType,
            PortalStream, PortalStreamInfo, PortalTransport,
        };

        #[derive(Clone, Copy)]
        enum Failure {
            Denied,
            Timeout,
            Missing,
        }

        struct FailureTransport {
            failure: Failure,
        }

        #[async_trait]
        impl PortalTransport for FailureTransport {
            async fn name_owner(&self, _name: &str) -> Result<Option<String>, PortalError> {
                match self.failure {
                    Failure::Missing => Ok(None),
                    Failure::Timeout => {
                        tokio::time::sleep(Duration::from_millis(200)).await;
                        Ok(Some(":1.42".into()))
                    }
                    Failure::Denied => Ok(Some(":1.42".into())),
                }
            }

            async fn screencast_version(&self) -> Result<u32, PortalError> {
                Ok(2)
            }

            async fn create_session(
                &self,
                _config: &PortalConfig,
                _handle_token: &str,
                _session_handle_token: &str,
            ) -> Result<PortalResults, PortalError> {
                match self.failure {
                    Failure::Denied => Err(PortalError::Denied {
                        stage: "CreateSession".into(),
                    }),
                    _ => panic!("preflight should have failed before CreateSession"),
                }
            }

            async fn select_sources(
                &self,
                _session: &str,
                _config: &PortalConfig,
                _handle_token: &str,
            ) -> Result<PortalResults, PortalError> {
                Err(PortalError::Denied {
                    stage: "SelectSources".into(),
                })
            }

            async fn start(
                &self,
                _session: &str,
                _config: &PortalConfig,
                _handle_token: &str,
            ) -> Result<PortalResults, PortalError> {
                Err(PortalError::Denied {
                    stage: "Start".into(),
                })
            }

            async fn open_pipewire_remote(&self, _session: &str) -> Result<OwnedFd, PortalError> {
                Err(PortalError::PipeWireFdUnavailable {
                    message: "no fd in failure transport".into(),
                })
            }

            async fn close_session(&self, _session: &str) {}
        }

        fn portal_client(failure: Failure, preflight: Duration) -> PortalScreenCast {
            PortalScreenCast::with_transport(
                PortalConfig {
                    source: PortalSourceType::Monitor,
                    preflight_timeout: preflight,
                    response_timeout: Duration::from_millis(500),
                    ..PortalConfig::default()
                },
                Arc::new(FailureTransport { failure }),
            )
            .expect("test portal config")
        }

        fn fake_stream() -> PortalStream {
            let fd = std::fs::File::open("/dev/null").expect("/dev/null");
            PortalStream {
                session_handle: "/org/freedesktop/portal/desktop/session/1_1/ozma".into(),
                streams: vec![PortalStreamInfo {
                    node_id: 7,
                    position: None,
                    size: Some((1920, 1080)),
                    source_type: Some(1),
                    mapping_id: None,
                }],
                pipewire_fd: Some(fd.into()),
                pipewire_fd_error: None,
                restore_token: Some("restore-1".into()),
                keepalive: None,
            }
        }

        fn portal_desktop() -> NodeConfig {
            let mut desktop = desktop_with_provider("portal");
            desktop.portal_source_type = crate::config::PortalSourceKind::Monitor;
            desktop.portal_requested = true;
            desktop
        }

        #[test]
        fn portal_config_maps_source_cursor_timeout_and_mode() {
            let mut desktop = portal_desktop();
            desktop.portal_source_type = crate::config::PortalSourceKind::Virtual;
            desktop.portal_cursor = crate::config::PortalCursorKind::Metadata;
            desktop.portal_restore_token = Some("token-xyz".into());
            desktop.portal_timeout = Duration::from_secs(7);
            desktop.desktop_capture_width = 1024;
            desktop.desktop_capture_height = 768;
            desktop.desktop_capture_fps = 20;

            let options = desktop.portal_capture_options();
            let config = portal_screencast_config(&options);
            assert_eq!(config.source, PortalSourceType::Virtual);
            assert_eq!(config.cursor_mode, ozma_capture::PortalCursorMode::Metadata);
            assert_eq!(config.restore_token.as_deref(), Some("token-xyz"));
            assert_eq!(config.response_timeout, Duration::from_secs(7));
            assert!(config.preflight_timeout <= config.response_timeout);

            let provider = portal_capture_config(&options);
            assert_eq!(provider.id, ozma_capture::PORTAL_DEFAULT_ID);
            assert!(provider.require_pipewire_fd);
            assert_eq!(
                (
                    provider.pipewire.width,
                    provider.pipewire.height,
                    provider.pipewire.fps
                ),
                (1024, 768, 20)
            );
        }

        #[tokio::test]
        async fn portal_success_registers_one_pipewire_source() {
            let desktop = portal_desktop();
            let registry = capture_registry_with_portal(&desktop, None, Some(fake_stream()));
            assert_eq!(registry.provider_count(), 1);
            let scans = registry.scan();
            assert_eq!(scans.len(), 1);
            assert_eq!(scans[0].kind, ozma_protocol::CaptureKind::PipeWire);
            assert!(scans[0].error.is_none(), "{:?}", scans[0].error);
            assert_eq!(scans[0].capabilities.len(), 1);
            assert_eq!(scans[0].capabilities[0].id, "portal0");

            let caps = detect_with_registry(&desktop, &registry);
            assert_eq!(caps.capture.len(), 1);
            assert_eq!(caps.capture[0].id, "portal0");
            assert!(!caps.encode.is_empty());
        }

        #[tokio::test]
        async fn portal_denial_timeout_and_missing_service_are_warned_no_capture() {
            for (failure, preflight) in [
                (Failure::Denied, Duration::from_millis(50)),
                (Failure::Timeout, Duration::from_millis(10)),
                (Failure::Missing, Duration::from_millis(50)),
            ] {
                let portal = portal_client(failure, preflight);
                assert!(
                    acquire_portal_stream(&portal).await.is_none(),
                    "failure must not yield a stream"
                );

                // The node keeps running with an empty desktop registry: no
                // X11, camera, or fake fallback, and no encode/display claim.
                let desktop = portal_desktop();
                let registry = capture_registry_with_portal(&desktop, None, None);
                assert_eq!(registry.provider_count(), 0);
                let caps = detect_with_registry(&desktop, &registry);
                assert!(caps.capture.is_empty());
                assert!(caps.encode.is_empty());
                assert!(!caps.display.supported);
            }
        }

        #[test]
        fn portal_missing_fd_is_a_typed_unsupported_source() {
            let desktop = portal_desktop();
            let mut stream = fake_stream();
            stream.pipewire_fd = None;
            stream.pipewire_fd_error = Some("OpenPipeWireRemote is not implemented".into());
            let registry = capture_registry_with_portal(&desktop, None, Some(stream));
            let scans = registry.scan();
            let error = scans[0]
                .error
                .as_ref()
                .expect("a missing required fd is an error");
            assert_eq!(error.code, ozma_protocol::codes::UNSUPPORTED);
            assert!(
                error.message.contains("PipeWire remote fd"),
                "{}",
                error.message
            );
        }
    }
}
