// SPDX-License-Identifier: AGPL-3.0-only
//! Transport and channel vocabulary. A session has typed channels; a
//! transport is an adapter that carries them.

use serde::{Deserialize, Serialize};

use crate::ids::{SessionId, TrackId};

/// A way to reach a node or carry a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TransportMode {
    /// Native QUIC/WebTransport on one UDP port.
    Quic,
    /// Raw RTP/UDP, including the Moonlight-compatible path.
    Udp,
    /// RTSP over TCP with `$` interleaved framing.
    TcpInterleaved,
    /// Local Unix domain socket plus a stdio proxy.
    UnixSocket,
    /// SSH subsystem or port-forwarded stream.
    Ssh,
    /// HLS/LL-HLS/MJPEG fallback.
    Hls,
    /// WireGuard relay when direct connectivity fails.
    Relay,
    /// In-process fake for tests and local bring-up.
    Loopback,
}

/// Logical channel carried inside a session.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelKind {
    Control,
    Input,
    Video,
    Audio,
    Mic,
    Telemetry,
    Bulk,
    /// Client -> node encoded camera video. Carries H.264 access units using
    /// the same fragmentation header as the node -> client video channels.
    Camera,
    /// Reliable, ordered display traffic: definitions, definition patches,
    /// scene commits, data updates, and action descriptors/invocations.
    Display,
}

/// Encoding selected for the rest of a session after negotiation.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WireFormat {
    /// Legacy JSON control path and REST/debug compatibility.
    Json,
    /// Canonical protobuf encoding.
    Protobuf,
}

impl Default for WireFormat {
    /// JSON is the compatibility default, so a peer that predates wire-format
    /// negotiation and omits the field is understood as JSON.
    fn default() -> Self {
        Self::Json
    }
}

impl WireFormat {
    /// Stable string used on the wire and in diagnostics.
    pub const fn wire_name(self) -> &'static str {
        match self {
            Self::Json => "json",
            Self::Protobuf => "protobuf",
        }
    }

    /// Parse a stable wire name produced by [`WireFormat::wire_name`].
    pub fn from_wire_name(name: &str) -> Option<Self> {
        match name {
            "json" => Some(Self::Json),
            "protobuf" => Some(Self::Protobuf),
            _ => None,
        }
    }

    /// Deterministically select the encoding both peers support.
    ///
    /// Protobuf is preferred over JSON regardless of the order of either
    /// list, so negotiation is symmetric. Returns `None` when the peers have
    /// no format in common.
    pub fn negotiate(local: &[WireFormat], remote: &[WireFormat]) -> Option<WireFormat> {
        [WireFormat::Protobuf, WireFormat::Json]
            .into_iter()
            .find(|candidate| local.contains(candidate) && remote.contains(candidate))
    }
}

/// Scheduling priority. Realtime channels must never be starved by video.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChannelPriority {
    Realtime,
    High,
    Normal,
    Low,
    Bulk,
}

/// One channel in a transport descriptor.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChannelDescriptor {
    pub id: u8,
    pub kind: ChannelKind,
    pub priority: ChannelPriority,
    pub reliable: bool,
    #[serde(default)]
    pub track_id: Option<TrackId>,
    #[serde(default = "default_max_payload")]
    pub max_payload: u16,
}

fn default_max_payload() -> u16 {
    1200
}

/// A reachable transport endpoint for a prepared session.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportDescriptor {
    pub mode: TransportMode,
    #[serde(default)]
    pub endpoint: Option<String>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub alpn: Option<String>,
    #[serde(default)]
    pub certificate_fingerprint: Option<String>,
    #[serde(default)]
    pub channels: Vec<ChannelDescriptor>,
}

/// How a client obtains the native transport endpoint and the certificate it
/// must pin. The certificate is base64 DER so the response is plain JSON.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TransportInfo {
    pub endpoint: String,
    pub fingerprint: String,
    pub certificate_der_base64: String,
}

/// What a client receives after creating a session and before it connects
/// media. The token is scoped to the session and short-lived.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionOffer {
    pub session_id: SessionId,
    pub token: String,
    pub transports: Vec<TransportDescriptor>,
    pub expires_at: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn camera_channel_has_a_stable_wire_name() {
        assert_eq!(
            serde_json::to_string(&ChannelKind::Camera).unwrap(),
            "\"camera\""
        );
        assert_eq!(
            serde_json::from_str::<ChannelKind>("\"mic\"").unwrap(),
            ChannelKind::Mic
        );
    }

    #[test]
    fn display_channel_has_a_stable_wire_name() {
        assert_eq!(
            serde_json::to_string(&ChannelKind::Display).unwrap(),
            "\"display\""
        );
        assert_eq!(
            serde_json::from_str::<ChannelKind>("\"display\"").unwrap(),
            ChannelKind::Display
        );
    }

    #[test]
    fn wire_format_has_stable_wire_names() {
        assert_eq!(WireFormat::Json.wire_name(), "json");
        assert_eq!(WireFormat::Protobuf.wire_name(), "protobuf");
        assert_eq!(
            serde_json::to_string(&WireFormat::Json).unwrap(),
            "\"json\""
        );
        assert_eq!(
            serde_json::to_string(&WireFormat::Protobuf).unwrap(),
            "\"protobuf\""
        );
        assert_eq!(
            serde_json::from_str::<WireFormat>("\"protobuf\"").unwrap(),
            WireFormat::Protobuf
        );
        assert_eq!(WireFormat::from_wire_name("json"), Some(WireFormat::Json));
        assert_eq!(WireFormat::from_wire_name("cbor"), None);
    }

    #[test]
    fn wire_format_defaults_to_json_for_legacy_peers() {
        assert_eq!(WireFormat::default(), WireFormat::Json);
    }

    #[test]
    fn negotiation_prefers_protobuf_then_json() {
        let both = [WireFormat::Json, WireFormat::Protobuf];
        assert_eq!(
            WireFormat::negotiate(&both, &both),
            Some(WireFormat::Protobuf)
        );
        // Local order does not matter.
        assert_eq!(
            WireFormat::negotiate(
                &[WireFormat::Protobuf, WireFormat::Json],
                &[WireFormat::Json, WireFormat::Protobuf]
            ),
            Some(WireFormat::Protobuf)
        );
        // JSON is used when it is the only shared format.
        assert_eq!(
            WireFormat::negotiate(
                &[WireFormat::Protobuf, WireFormat::Json],
                &[WireFormat::Json]
            ),
            Some(WireFormat::Json)
        );
    }

    #[test]
    fn negotiation_is_symmetric_already() {
        let local = [WireFormat::Protobuf, WireFormat::Json];
        let remote = [WireFormat::Json];
        let forward = WireFormat::negotiate(&local, &remote);
        let reverse = WireFormat::negotiate(&remote, &local);
        assert_eq!(forward, reverse);
        assert_eq!(forward, Some(WireFormat::Json));

        let only_local = [WireFormat::Protobuf];
        let only_remote = [WireFormat::Json];
        assert_eq!(
            WireFormat::negotiate(&only_local, &only_remote),
            WireFormat::negotiate(&only_remote, &only_local)
        );
        assert_eq!(WireFormat::negotiate(&only_local, &only_remote), None);
    }

    #[test]
    fn negotiation_returns_none_without_overlap() {
        assert_eq!(WireFormat::negotiate(&[WireFormat::Json], &[]), None);
        assert_eq!(WireFormat::negotiate(&[], &[WireFormat::Protobuf]), None);
        assert_eq!(WireFormat::negotiate(&[], &[]), None);
        assert_eq!(
            WireFormat::negotiate(&[WireFormat::Protobuf], &[WireFormat::Json]),
            None
        );
    }

    #[test]
    fn channel_descriptor_roundtrips() {
        let descriptor = ChannelDescriptor {
            id: 1,
            kind: ChannelKind::Video,
            priority: ChannelPriority::Normal,
            reliable: false,
            track_id: Some(TrackId::new("video0")),
            max_payload: 1200,
        };
        let json = serde_json::to_string(&descriptor).unwrap();
        let back: ChannelDescriptor = serde_json::from_str(&json).unwrap();
        assert_eq!(back, descriptor);
    }
}
