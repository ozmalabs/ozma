// SPDX-License-Identifier: AGPL-3.0-only
//! Session Description Protocol generation for one H.264 track.
//!
//! The output is deliberately small and standard: `packetization-mode=1`
//! advertises FU-A/STAP-A, which is exactly what [`ozma_rtp::H264Packetizer`]
//! emits. Multi-track SDP is an M3 task in `docs/node-hermes-parity.md` §15.

use std::net::IpAddr;

/// Inputs for one-track SDP generation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdpConfig {
    /// Session identifier used in the `o=` line.
    pub session_id: String,
    /// RTP payload type, normally 96.
    pub payload_type: u8,
    /// Advertised frame rate; `None` omits `a=framerate`.
    pub fps: Option<u32>,
    /// Track control attribute, relative to the base URL.
    pub track_control: String,
    /// Optional `profile-level-id` value for `a=fmtp`.
    pub profile_level_id: Option<String>,
    /// Address for the `c=` line. `None` uses `0.0.0.0`.
    pub connection_address: Option<IpAddr>,
}

impl SdpConfig {
    /// Construct a config for one H.264 track.
    pub fn h264(session_id: impl Into<String>, payload_type: u8) -> Self {
        Self {
            session_id: session_id.into(),
            payload_type,
            fps: Some(30),
            track_control: "trackID=0".to_owned(),
            profile_level_id: Some("42C01F".to_owned()),
            connection_address: None,
        }
    }
}

/// Render SDP for one H.264 video track.
///
/// Required interoperability lines:
///
/// ```text
/// m=video 0 RTP/AVP 96
/// a=rtpmap:96 H264/90000
/// a=fmtp:96 packetization-mode=1
/// a=control:trackID=0
/// ```
pub fn h264_sdp(config: &SdpConfig) -> String {
    let payload_type = config.payload_type;
    let (family, address) = match config.connection_address {
        Some(IpAddr::V6(address)) => ("IP6", address.to_string()),
        Some(IpAddr::V4(address)) => ("IP4", address.to_string()),
        None => ("IP4", "0.0.0.0".to_owned()),
    };
    let mut lines = vec![
        "v=0".to_owned(),
        format!(
            "o=- {} 1 IN {} {}",
            sanitize(&config.session_id),
            family,
            address
        ),
        "s=Ozma H.264 Stream".to_owned(),
        format!("c=IN {family} {address}"),
        "t=0 0".to_owned(),
        "a=control:*".to_owned(),
        "a=range:npt=0-".to_owned(),
        format!("m=video 0 RTP/AVP {payload_type}"),
        format!("a=rtpmap:{payload_type} H264/90000"),
    ];
    let fmtp = match &config.profile_level_id {
        Some(profile) if !profile.is_empty() => {
            format!("a=fmtp:{payload_type} packetization-mode=1;profile-level-id={profile}")
        }
        _ => format!("a=fmtp:{payload_type} packetization-mode=1"),
    };
    lines.push(fmtp);
    lines.push(format!("a=control:{}", config.track_control));
    lines.push("a=sendonly".to_owned());
    if let Some(fps) = config.fps.filter(|fps| *fps > 0) {
        lines.push(format!("a=framerate:{fps}"));
    }

    let mut sdp = lines.join("\r\n");
    sdp.push_str("\r\n");
    sdp
}

fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '-' | '_' | '.') {
                ch
            } else {
                '-'
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sdp_has_required_h264_lines() {
        let sdp = h264_sdp(&SdpConfig::h264("session-1", 96));
        assert!(sdp.starts_with("v=0\r\n"));
        assert!(sdp.contains("\r\nm=video 0 RTP/AVP 96\r\n"));
        assert!(sdp.contains("\r\na=rtpmap:96 H264/90000\r\n"));
        assert!(sdp.contains("\r\na=fmtp:96 packetization-mode=1"));
        assert!(sdp.contains("\r\na=control:trackID=0\r\n"));
        assert!(sdp.ends_with("\r\n"));
    }

    #[test]
    fn dynamic_payload_type_is_consistent() {
        let mut config = SdpConfig::h264("x", 98);
        config.fps = None;
        config.profile_level_id = None;
        let sdp = h264_sdp(&config);
        assert!(sdp.contains("m=video 0 RTP/AVP 98"));
        assert!(sdp.contains("a=rtpmap:98 H264/90000"));
        assert!(sdp.contains("a=fmtp:98 packetization-mode=1\r\n"));
        assert!(!sdp.contains("a=framerate"));
        assert!(!sdp.contains("profile-level-id"));
    }
}
