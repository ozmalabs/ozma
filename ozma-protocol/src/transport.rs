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
