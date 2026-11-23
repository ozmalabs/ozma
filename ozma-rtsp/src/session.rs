// SPDX-License-Identifier: AGPL-3.0-only
//! RTSP session state and `Transport:` negotiation.

use crate::error::RtspError;

/// Session lifecycle used by the server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// Created by DESCRIBE or SETUP but not yet ready.
    Init,
    /// SDP was delivered; no transport is selected.
    Described,
    /// SETUP completed; transport is ready but media is stopped.
    Ready,
    /// PLAY is active; media is flowing.
    Playing,
}

impl SessionState {
    /// Stable lowercase label for logs and diagnostics.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Init => "init",
            Self::Described => "described",
            Self::Ready => "ready",
            Self::Playing => "playing",
        }
    }
}

/// The client's requested transport, before server ports are allocated.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum TransportRequest {
    /// RTP interleaved in the RTSP TCP connection.
    Tcp {
        /// Interleaved RTP channel.
        rtp_channel: u8,
        /// Interleaved RTCP channel.
        rtcp_channel: u8,
    },
    /// RTP over UDP.
    Udp {
        /// Client RTP port.
        client_rtp: u16,
        /// Client RTCP port.
        client_rtcp: u16,
    },
}

/// Parse a `Transport:` header.
///
/// Supported:
///
/// * `RTP/AVP/TCP;unicast;interleaved=0-1`
/// * `RTP/AVP;unicast;client_port=5000-5001`
/// * `RTP/AVP/UDP;unicast;client_port=5000`
///
/// Multicast, SRTP, and missing `client_port` for UDP are rejected with
/// [`RtspError::UnsupportedTransport`], which the server answers with 461.
pub fn parse_transport(value: &str) -> Result<TransportRequest, RtspError> {
    let mut parts = value
        .split(';')
        .map(str::trim)
        .filter(|part| !part.is_empty());
    let protocol = parts
        .next()
        .ok_or_else(|| RtspError::UnsupportedTransport("empty Transport header".to_owned()))?;

    let tcp = protocol.eq_ignore_ascii_case("RTP/AVP/TCP")
        || protocol.eq_ignore_ascii_case("RTP/AVP/TCP/AVP");
    let udp =
        protocol.eq_ignore_ascii_case("RTP/AVP") || protocol.eq_ignore_ascii_case("RTP/AVP/UDP");
    if !tcp && !udp {
        return Err(RtspError::UnsupportedTransport(format!(
            "protocol `{protocol}` (expected RTP/AVP or RTP/AVP/TCP)"
        )));
    }

    let mut interleaved: Option<(u8, u8)> = None;
    let mut client_port: Option<(u16, u16)> = None;
    let mut multicast = false;

    for part in parts {
        let (key, value) = match part.split_once('=') {
            Some((key, value)) => (key.trim(), value.trim()),
            None => (part, ""),
        };
        if key.eq_ignore_ascii_case("multicast") {
            multicast = true;
            continue;
        }
        if key.eq_ignore_ascii_case("interleaved") {
            let (first, second) = parse_pair(value, "interleaved")?;
            if first > u8::MAX as u16 || second > u8::MAX as u16 {
                return Err(RtspError::MalformedRequest(
                    "interleaved channel is larger than 255".to_owned(),
                ));
            }
            interleaved = Some((first as u8, second as u8));
        } else if key.eq_ignore_ascii_case("client_port") {
            client_port = Some(parse_pair(value, "client_port")?);
        }
        // `mode=play`, `ttl=...`, `ssrc=...`, and unknown parameters are
        // tolerated and ignored.
    }

    if multicast {
        return Err(RtspError::UnsupportedTransport(
            "multicast is not implemented".to_owned(),
        ));
    }

    if tcp {
        let (rtp_channel, rtcp_channel) = interleaved.unwrap_or((0, 1));
        if rtp_channel == rtcp_channel {
            return Err(RtspError::MalformedRequest(
                "interleaved RTP and RTCP channels must differ".to_owned(),
            ));
        }
        return Ok(TransportRequest::Tcp {
            rtp_channel,
            rtcp_channel,
        });
    }

    let (client_rtp, client_rtcp) = client_port.ok_or_else(|| {
        RtspError::UnsupportedTransport("UDP SETUP requires client_port=N-M".to_owned())
    })?;
    if client_rtp == 0 || client_rtcp == 0 {
        return Err(RtspError::MalformedRequest(
            "client_port must be non-zero".to_owned(),
        ));
    }
    Ok(TransportRequest::Udp {
        client_rtp,
        client_rtcp,
    })
}

fn parse_pair(value: &str, name: &str) -> Result<(u16, u16), RtspError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(RtspError::MalformedRequest(format!("{name} has no value")));
    }
    let (first, second) = match value.split_once('-') {
        Some((first, second)) => (first.trim(), second.trim()),
        None => (value, ""),
    };
    let first = first
        .parse::<u16>()
        .map_err(|_| RtspError::MalformedRequest(format!("invalid {name} value `{value}`")))?;
    if second.is_empty() {
        let second = first.checked_add(1).ok_or_else(|| {
            RtspError::MalformedRequest(format!("{name} cannot allocate a second port"))
        })?;
        return Ok((first, second));
    }
    let second = second
        .parse::<u16>()
        .map_err(|_| RtspError::MalformedRequest(format!("invalid {name} value `{value}`")))?;
    Ok((first, second))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_tcp_interleaved() {
        assert_eq!(
            parse_transport("RTP/AVP/TCP;unicast;interleaved=0-1").unwrap(),
            TransportRequest::Tcp {
                rtp_channel: 0,
                rtcp_channel: 1
            }
        );
        assert_eq!(
            parse_transport("RTP/AVP/TCP;unicast;interleaved=4-5;mode=play").unwrap(),
            TransportRequest::Tcp {
                rtp_channel: 4,
                rtcp_channel: 5
            }
        );
    }

    #[test]
    fn parses_udp_client_ports() {
        assert_eq!(
            parse_transport("RTP/AVP;unicast;client_port=5000-5001").unwrap(),
            TransportRequest::Udp {
                client_rtp: 5000,
                client_rtcp: 5001
            }
        );
        assert_eq!(
            parse_transport("RTP/AVP/UDP;unicast;client_port=5000").unwrap(),
            TransportRequest::Udp {
                client_rtp: 5000,
                client_rtcp: 5001
            }
        );
    }

    #[test]
    fn rejects_unsupported_transports() {
        assert!(parse_transport("RTP/AVP;multicast;client_port=5000-5001").is_err());
        assert!(parse_transport("RTP/AVP;unicast").is_err());
        assert!(parse_transport("RTP/SAVP;unicast;client_port=1-2").is_err());
        assert!(parse_transport("RTP/AVP/TCP;interleaved=1-1").is_err());
        assert!(parse_transport("").is_err());
    }

    #[test]
    fn defaults_tcp_channels_when_omitted() {
        assert_eq!(
            parse_transport("RTP/AVP/TCP;unicast").unwrap(),
            TransportRequest::Tcp {
                rtp_channel: 0,
                rtcp_channel: 1
            }
        );
    }
}
