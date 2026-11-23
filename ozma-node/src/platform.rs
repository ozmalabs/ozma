// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Capability detection. The first pass builds on the protocol's deterministic
//! fake capabilities and narrows them according to the node's `--cap` list.
//! Real V4L2/encoder probes plug in here without changing the API.

use ozma_protocol::{
    AudioCapability, InputCapability, NodeCapabilities, NodeId, ResourceBudget, TransportMode,
    UsbCapability, VirtualDisplayCaps,
};

use ozma_capture::{
    CaptureRegistry, DBusDisplayConfig, QmpDisplayConfig, VncDisplayConfig, DBUS_DEFAULT_ID,
    QMP_DEFAULT_ID, QMP_DEFAULT_MAX_FPS, VNC_DEFAULT_ID,
};

use crate::config::{NodeConfig, NodeProfile};

/// Build the capture registry for this node.
///
/// Hardware keeps V4L2+PipeWire discovery. A VM only ever advertises its own
/// display: either the QMP `screendump` path or the ffmpeg-backed VNC path
/// selected by the provider resolver. Advertising the host's V4L2/PipeWire
/// devices on a guest would claim hardware the VM does not own. A VM with no
/// configured display gets an empty registry.
pub fn capture_registry(config: &NodeConfig) -> CaptureRegistry {
    match config.profile {
        NodeProfile::Vm => vm_capture_registry(config),
        NodeProfile::Hardware | NodeProfile::Desktop => CaptureRegistry::system_with_pipewire(),
    }
}

fn vm_capture_registry(config: &NodeConfig) -> CaptureRegistry {
    let capture = config
        .vm_providers()
        .ok()
        .and_then(|providers| providers.capture);

    match capture.as_deref() {
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
            Some(socket) => CaptureRegistry::new().with_qmp_display(
                QmpDisplayConfig::new(socket.clone())
                    .with_identity(QMP_DEFAULT_ID, format!("{} VM display", config.name))
                    .with_max_fps(QMP_DEFAULT_MAX_FPS),
            ),
            None => CaptureRegistry::new(),
        },
        Some("dbus") => match &config.qmp_socket {
            Some(socket) => CaptureRegistry::new().with_dbus_display(
                DBusDisplayConfig::new(socket.clone())
                    .with_console_index(config.dbus_console)
                    .with_identity(DBUS_DEFAULT_ID, format!("{} D-Bus display", config.name))
                    .with_limits(config.dbus_width, config.dbus_height)
                    .with_fps(config.dbus_fps),
            ),
            None => CaptureRegistry::new(),
        },
        _ => CaptureRegistry::new(),
    }
}

/// Build the capability set advertised by this node, probing the host.
pub fn detect(config: &NodeConfig) -> NodeCapabilities {
    detect_with_registry(config, &capture_registry(config))
}

/// Build capabilities with an injected capture registry, for tests and
/// unusual deployments.
pub fn detect_with_registry(config: &NodeConfig, capture: &CaptureRegistry) -> NodeCapabilities {
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
                // than pretending a 0x0 mode is usable.
                if source.max_width == 0 {
                    source.max_width = 1920;
                }
                if source.max_height == 0 {
                    source.max_height = 1080;
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
            // never assumed from the platform name.
            caps.encode = vec![ozma_encode::H264SoftwareEncoder::capability()];
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

    if config.has_capability("usb") {
        caps.usb = UsbCapability {
            gadget: true,
            usbip: false,
            virtual_media: false,
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
        max_remote_displays: 1,
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
            qmp_connect_timeout: std::time::Duration::from_millis(3_000),
            qmp_command_timeout: std::time::Duration::from_millis(5_000),
            vnc_host: None,
            vnc_port: None,
            vnc_width: ozma_capture::VNC_DEFAULT_WIDTH,
            vnc_height: ozma_capture::VNC_DEFAULT_HEIGHT,
            vnc_fps: ozma_capture::VNC_DEFAULT_FPS,
            dbus_console: 0,
            dbus_width: ozma_capture::DBUS_DEFAULT_WIDTH,
            dbus_height: ozma_capture::DBUS_DEFAULT_HEIGHT,
            dbus_fps: ozma_capture::DBUS_DEFAULT_FPS,
            dbus_requested: false,
            audio_router: crate::config::AudioRouterKind::None,
            audio_track_root: None,
            ffmpeg_bin: std::path::PathBuf::from("ffmpeg"),
            audio_sink: None,
            webrtc_audio_source: None,
            webrtc_audio_backend: crate::webrtc::WebRtcAudioBackend::Auto,
            vm_guest_ip: None,
            display_outputs_json: "[]".into(),
            pci_devices_json: "[]".into(),
        }
    }

    #[test]
    fn video_capability_adds_capture_and_encoder() {
        let registry = CaptureRegistry::new().with(FakeCaptureProvider::new());
        let caps = detect_with_registry(&config("hid,video,audio"), &registry);
        assert!(!caps.capture.is_empty());
        assert!(!caps.encode.is_empty());
        assert!(!caps.display.supported);
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

    #[test]
    fn hardware_profile_never_advertises_vnc() {
        let hardware = config("hid,video,audio");
        let registry = capture_registry(&hardware);
        assert!(registry
            .scan()
            .iter()
            .all(|scan| scan.kind != ozma_protocol::CaptureKind::Vnc));
    }
}
