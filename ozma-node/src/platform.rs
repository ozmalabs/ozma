// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Capability detection. The first pass builds on the protocol's deterministic
//! fake capabilities and narrows them according to the node's `--cap` list.
//! Real V4L2/encoder probes plug in here without changing the API.

use ozma_protocol::{
    AudioCapability, CaptureCapability, CaptureKind, Codec, EncodeCapability, EncoderBackend,
    InputCapability, NodeCapabilities, NodeId, ResourceBudget, TransportMode, UsbCapability,
    VirtualDisplayCaps,
};

use crate::config::NodeConfig;

/// Build the capability set advertised by this node.
pub fn detect(config: &NodeConfig) -> NodeCapabilities {
    let mut caps = NodeCapabilities::fake(config.node_id.clone());
    caps.name = config.name.clone();
    caps.platform = config.hw.clone();
    caps.node_id = config.node_id.clone();

    if !config.has_capability("video") {
        caps.capture.clear();
        caps.encode.clear();
        caps.display = VirtualDisplayCaps::none();
    } else {
        caps.capture = vec![CaptureCapability {
            id: "capture0".to_owned(),
            name: "Primary capture".to_owned(),
            kind: CaptureKind::V4l2,
            formats: vec!["mjpeg".to_owned(), "yuyv".to_owned(), "nv12".to_owned()],
            max_width: 1920,
            max_height: 1080,
            max_fps: 60,
            compressed: true,
            hdr: false,
            has_audio: config.has_capability("audio"),
        }];
        caps.encode = vec![
            EncodeCapability {
                codec: Codec::H264,
                backend: EncoderBackend::V4l2M2m,
                hardware: true,
                max_width: 1920,
                max_height: 1080,
                max_fps: 30,
                hdr: false,
                profiles: vec!["baseline".to_owned(), "main".to_owned()],
            },
            EncodeCapability {
                codec: Codec::H264,
                backend: EncoderBackend::Software,
                hardware: false,
                max_width: 1920,
                max_height: 1080,
                max_fps: 30,
                hdr: false,
                profiles: vec!["baseline".to_owned()],
            },
        ];
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
        }
    }

    #[test]
    fn video_capability_adds_capture_and_encoder() {
        let caps = detect(&config("hid,video,audio"));
        assert!(!caps.capture.is_empty());
        assert!(!caps.encode.is_empty());
        assert!(!caps.display.supported);
    }

    #[test]
    fn no_video_means_no_capture_or_display() {
        let caps = detect(&config("hid,audio"));
        assert!(caps.capture.is_empty());
        assert!(caps.encode.is_empty());
        assert!(!caps.display.supported);
    }
}
