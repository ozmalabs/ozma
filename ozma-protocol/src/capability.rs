// SPDX-License-Identifier: AGPL-3.0-only
//! Node capabilities. A controller or client chooses a session profile from
//! this, rather than asking a user to pick a codec or port.

use serde::{Deserialize, Serialize};

use crate::display::VirtualDisplayCaps;
use crate::ids::NodeId;
use crate::media::Codec;
use crate::transport::TransportMode;
use crate::version::{ProtocolVersion, CURRENT_PROTOCOL_VERSION};

/// How a capture source is reached.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CaptureKind {
    V4l2,
    Kms,
    PipeWire,
    /// QEMU display captured through QMP `screendump`.
    Qmp,
    /// Looking Glass KVMFR shared-memory framebuffer.
    LookingGlass,
    /// VM display read through the VNC/RFB protocol (ffmpeg transition path).
    Vnc,
    X11,
    Dxgi,
    HdmiSink,
    Fake,
}

/// A single capturable source.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureCapability {
    pub id: String,
    pub name: String,
    pub kind: CaptureKind,
    #[serde(default)]
    pub formats: Vec<String>,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    #[serde(default)]
    pub compressed: bool,
    #[serde(default)]
    pub hdr: bool,
    #[serde(default)]
    pub has_audio: bool,
}

/// Hardware or software encoder backend.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EncoderBackend {
    Vaapi,
    Nvenc,
    Qsv,
    V4l2M2m,
    Software,
    Fake,
}

/// A codec/backend combination the node can actually run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EncodeCapability {
    pub codec: Codec,
    pub backend: EncoderBackend,
    pub hardware: bool,
    pub max_width: u32,
    pub max_height: u32,
    pub max_fps: u32,
    #[serde(default)]
    pub hdr: bool,
    #[serde(default)]
    pub profiles: Vec<String>,
}

/// Audio capture/playback capability.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioCapability {
    pub capture: bool,
    pub playback: bool,
    pub mic: bool,
    #[serde(default)]
    pub codecs: Vec<Codec>,
    #[serde(default)]
    pub sample_rates: Vec<u32>,
    #[serde(default)]
    pub channels: u8,
}

/// Input paths the node can expose.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputCapability {
    pub keyboard: bool,
    pub mouse_absolute: bool,
    pub mouse_relative: bool,
    pub gamepad: bool,
    pub touch: bool,
    pub pen: bool,
    pub clipboard: bool,
    pub release_all: bool,
}

/// USB gadget and USB/IP support.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbCapability {
    pub gadget: bool,
    pub usbip: bool,
    pub virtual_media: bool,
}

/// Power and physical controls.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct PowerCapability {
    pub wake_on_lan: bool,
    pub reset: bool,
    pub power_off: bool,
    pub relay: bool,
    pub current_sense: bool,
    pub rgb: bool,
}

/// Resource envelope advertised to the controller.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceBudget {
    pub max_sessions: u16,
    pub max_tracks: u16,
    pub max_bitrate_kbps: u32,
    pub max_remote_displays: u16,
}

/// Everything a controller or client needs to plan a session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCapabilities {
    pub protocol: ProtocolVersion,
    pub node_id: NodeId,
    pub name: String,
    pub platform: String,
    #[serde(default)]
    pub capture: Vec<CaptureCapability>,
    #[serde(default)]
    pub encode: Vec<EncodeCapability>,
    #[serde(default)]
    pub audio: AudioCapability,
    #[serde(default)]
    pub input: InputCapability,
    #[serde(default)]
    pub transport: Vec<TransportMode>,
    #[serde(default)]
    pub display: VirtualDisplayCaps,
    #[serde(default)]
    pub usb: UsbCapability,
    #[serde(default)]
    pub power: PowerCapability,
    #[serde(default)]
    pub budget: ResourceBudget,
}

impl NodeCapabilities {
    /// Deterministic capabilities for CI and the onboarding harness.
    pub fn fake(node_id: impl Into<NodeId>) -> Self {
        let codec = Codec::H264;
        Self {
            protocol: CURRENT_PROTOCOL_VERSION,
            node_id: node_id.into(),
            name: "Ozma test node".to_owned(),
            platform: "fake".to_owned(),
            capture: vec![CaptureCapability {
                id: "fake0".to_owned(),
                name: "Fake capture".to_owned(),
                kind: CaptureKind::Fake,
                formats: vec!["rgb24".to_owned()],
                max_width: 1920,
                max_height: 1080,
                max_fps: 30,
                compressed: false,
                hdr: false,
                has_audio: true,
            }],
            encode: vec![EncodeCapability {
                codec,
                backend: EncoderBackend::Fake,
                hardware: false,
                max_width: 1920,
                max_height: 1080,
                max_fps: 30,
                hdr: false,
                profiles: vec!["baseline".to_owned()],
            }],
            audio: AudioCapability {
                capture: true,
                playback: true,
                mic: true,
                codecs: vec![Codec::Opus],
                sample_rates: vec![48_000],
                channels: 2,
            },
            input: InputCapability {
                keyboard: true,
                mouse_absolute: true,
                mouse_relative: true,
                gamepad: true,
                touch: true,
                pen: true,
                clipboard: true,
                release_all: true,
            },
            transport: vec![
                TransportMode::Loopback,
                TransportMode::Quic,
                TransportMode::TcpInterleaved,
            ],
            display: VirtualDisplayCaps::fake(),
            usb: UsbCapability {
                gadget: false,
                usbip: false,
                virtual_media: false,
            },
            power: PowerCapability::default(),
            budget: ResourceBudget {
                max_sessions: 2,
                max_tracks: 2,
                max_bitrate_kbps: 20_000,
                max_remote_displays: 2,
            },
        }
    }

    /// Best capture source by area, used when a client asks for "any".
    pub fn best_capture(&self) -> Option<&CaptureCapability> {
        self.capture
            .iter()
            .max_by_key(|capture| capture.max_width * capture.max_height)
    }

    /// Best encoder for a codec, preferring hardware.
    pub fn best_encoder(&self, codec: Codec) -> Option<&EncodeCapability> {
        self.encode
            .iter()
            .filter(|encoder| encoder.codec == codec)
            .max_by_key(|encoder| (encoder.hardware, encoder.max_fps, encoder.max_width))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_kinds_serialize_with_stable_names() {
        assert_eq!(
            serde_json::to_string(&CaptureKind::Vnc).unwrap(),
            r#""vnc""#
        );
        assert_eq!(
            serde_json::from_str::<CaptureKind>(r#""vnc""#).unwrap(),
            CaptureKind::Vnc
        );
        assert_eq!(
            serde_json::to_string(&CaptureKind::LookingGlass).unwrap(),
            r#""looking_glass""#
        );
    }

    #[test]
    fn fake_capabilities_are_consistent() {
        let caps = NodeCapabilities::fake("test-node");
        assert!(caps.display.supported);
        assert_eq!(caps.best_capture().unwrap().id, "fake0");
        assert_eq!(
            caps.best_encoder(Codec::H264).unwrap().backend,
            EncoderBackend::Fake
        );
    }
}
