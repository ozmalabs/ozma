// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Capability detection. The first pass builds on the protocol's deterministic
//! fake capabilities and narrows them according to the node's `--cap` list.
//! Real V4L2/encoder probes plug in here without changing the API.

use ozma_protocol::{
    AudioCapability, InputCapability, NodeCapabilities, NodeId, ResourceBudget, TransportMode,
    UsbCapability, VirtualDisplayCaps,
};

use ozma_capture::CaptureRegistry;

use crate::config::NodeConfig;

/// Build the capability set advertised by this node, probing the host.
pub fn detect(config: &NodeConfig) -> NodeCapabilities {
    detect_with_registry(config, &CaptureRegistry::system())
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
}
