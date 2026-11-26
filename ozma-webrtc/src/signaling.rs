// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Newline-delimited JSON signaling contract.
//!
//! Input shapes match `scripts/webrtc/gst_webrtc_worker.py`:
//!
//! * `{"type":"offer","sdp":"..."}` -> `{"type":"answer","sdp":"...","session":"..."}`
//! * `{"type":"add-ice-candidate","session":"...","candidate":"...","sdpMLineIndex":0}`
//!   -> `{"ok":true,...}`
//! * `{"type":"bitrate","bitrate":<bps>}` -> `{"ok":true,"bitrate":<clamped bps>}`
//!
//! Failures are either `{"type":"error","error":"..."}` (offer) or
//! `{"ok":false,"error":"..."}` (candidate/bitrate), matching what the node's
//! response parsers accept.

use serde_json::{json, Value};

use crate::encoder::{bps_to_kbps, clamp_bitrate_bps};

/// One parsed client message.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientMessage {
    /// A peer's SDP offer.
    Offer { sdp: String },
    /// One trickled remote ICE candidate.
    AddIceCandidate {
        session: Option<String>,
        candidate: String,
        sdp_mline_index: u32,
    },
    /// A requested video bitrate in bits per second.
    Bitrate { bitrate_bps: u64 },
}

/// A signaling line could not be parsed or was invalid.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("{0}")]
pub struct SignalError(pub String);

fn error(message: impl Into<String>) -> SignalError {
    SignalError(message.into())
}

/// Parse one newline-delimited JSON message.
pub fn parse_message(line: &str) -> Result<ClientMessage, SignalError> {
    let value: Value =
        serde_json::from_str(line.trim()).map_err(|e| error(format!("invalid JSON: {e}")))?;
    let object = value
        .as_object()
        .ok_or_else(|| error("signaling message is not a JSON object"))?;
    let kind = object
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| error("signaling message has no string `type`"))?;

    match kind {
        "offer" => {
            let sdp = object
                .get("sdp")
                .and_then(Value::as_str)
                .ok_or_else(|| error("offer has no string `sdp`"))?;
            if sdp.trim().is_empty() {
                return Err(error("offer `sdp` must not be empty"));
            }
            Ok(ClientMessage::Offer {
                sdp: sdp.to_owned(),
            })
        }
        "add-ice-candidate" | "candidate" => {
            let candidate = object
                .get("candidate")
                .and_then(Value::as_str)
                .ok_or_else(|| error("candidate message has no string `candidate`"))?;
            let candidate = validate_candidate(candidate)?;
            let session = object
                .get("session")
                .and_then(Value::as_str)
                .map(str::trim)
                .filter(|session| !session.is_empty())
                .map(str::to_owned);
            let sdp_mline_index = match object.get("sdpMLineIndex") {
                None | Some(Value::Null) => 0,
                Some(value) => parse_u32(value, "sdpMLineIndex")?,
            };
            Ok(ClientMessage::AddIceCandidate {
                session,
                candidate,
                sdp_mline_index,
            })
        }
        "bitrate" => {
            let value = object
                .get("bitrate")
                .ok_or_else(|| error("bitrate message has no `bitrate`"))?;
            let bitrate_bps = parse_u64(value, "bitrate")?;
            Ok(ClientMessage::Bitrate { bitrate_bps })
        }
        other => Err(error(format!("unsupported signaling type `{other}`"))),
    }
}

fn parse_u64(value: &Value, field: &str) -> Result<u64, SignalError> {
    match value {
        Value::Number(number) => number
            .as_u64()
            .ok_or_else(|| error(format!("`{field}` must be a non-negative integer"))),
        Value::String(text) => text
            .trim()
            .parse::<u64>()
            .map_err(|_| error(format!("`{field}` must be a non-negative integer"))),
        _ => Err(error(format!("`{field}` must be a non-negative integer"))),
    }
}

fn parse_u32(value: &Value, field: &str) -> Result<u32, SignalError> {
    let value = parse_u64(value, field)?;
    u32::try_from(value).map_err(|_| error(format!("`{field}` is too large")))
}

/// Validate and normalise a trickled candidate.
///
/// Accepts an optional `a=` prefix and requires a `candidate:` prefix plus a
/// recognised `typ` token, exactly like the Python worker. Control characters
/// are rejected rather than forwarded to the ICE agent.
pub fn validate_candidate(candidate: &str) -> Result<String, SignalError> {
    let value = candidate.trim();
    if value.is_empty() {
        return Err(error("candidate must not be empty"));
    }
    if value.chars().any(char::is_control) {
        return Err(error("candidate contains control characters"));
    }
    let value = value.strip_prefix("a=").unwrap_or(value);
    if !value.starts_with("candidate:") {
        return Err(error("candidate must start with `candidate:`"));
    }
    if crate::sdp::candidate_type(value).is_none() {
        return Err(error("candidate must carry a recognised `typ` token"));
    }
    Ok(value.to_owned())
}

/// Clamp and convert a requested bps bitrate to the encoder's kbps setting.
pub fn bitrate_kbps_for(requested_bps: u64) -> (u32, u32) {
    let clamped = clamp_bitrate_bps(requested_bps);
    (clamped, bps_to_kbps(clamped))
}

/// Bounds accepted over signaling, for documentation and tests.
#[cfg(test)]
pub fn bitrate_bounds() -> (u32, u32) {
    (
        crate::encoder::MIN_BITRATE_BPS,
        crate::encoder::MAX_BITRATE_BPS,
    )
}

// ---------------------------------------------------------------------------
// Response shapes
// ---------------------------------------------------------------------------

pub fn answer_response(sdp: &str, session: &str) -> String {
    json!({"type": "answer", "sdp": sdp, "session": session}).to_string()
}

pub fn error_response(message: &str) -> String {
    json!({"type": "error", "error": message}).to_string()
}

pub fn candidate_ack(session: Option<&str>, remote_candidates: usize) -> String {
    json!({
        "ok": true,
        "session": session,
        "remote_candidates": remote_candidates,
    })
    .to_string()
}

pub fn error_ack(message: &str) -> String {
    json!({"ok": false, "error": message}).to_string()
}

pub fn bitrate_ack(clamped_bps: u32) -> String {
    json!({"ok": true, "bitrate": clamped_bps}).to_string()
}

// ---------------------------------------------------------------------------
// ICE server normalisation
// ---------------------------------------------------------------------------

/// Which ICE server class a flag describes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum IceKind {
    Stun,
    Turn,
}

impl IceKind {
    fn schemes(self) -> (&'static [&'static str], &'static str, u16) {
        match self {
            Self::Stun => (&["stun", "stuns"], "stun", 3478),
            Self::Turn => (&["turn", "turns"], "turn", 3478),
        }
    }
}

/// Normalise a STUN/TURN address to `scheme:host:port`.
///
/// Accepted forms are `stun://host:port`, `stun:host:port`, and a bare
/// `host:port` (or `host`, with the class default port). Embedded credentials
/// are rejected; TURN credentials travel in separate flags.
pub fn normalize_ice_server(value: &str, kind: IceKind) -> Result<String, SignalError> {
    let value = value.trim();
    if value.is_empty() {
        return Err(error("ICE server address must not be empty"));
    }
    if value.chars().any(char::is_whitespace) {
        return Err(error("ICE server address must not contain whitespace"));
    }
    if value.contains('@') {
        return Err(error(
            "ICE server address must not embed credentials; use --turn-user/--turn-password",
        ));
    }

    let (allowed, default_scheme, _default_port) = kind.schemes();
    let (scheme, rest) = if let Some((head, tail)) = value.split_once("://") {
        (head.to_ascii_lowercase(), tail)
    } else if let Some((head, tail)) = value.split_once(':') {
        let head = head.to_ascii_lowercase();
        if allowed.contains(&head.as_str()) {
            (head, tail)
        } else if ["stun", "stuns", "turn", "turns"].contains(&head.as_str()) {
            return Err(error(format!(
                "unsupported ICE scheme `{head}` for this flag"
            )));
        } else {
            (default_scheme.to_owned(), value)
        }
    } else {
        (default_scheme.to_owned(), value)
    };

    if !allowed.contains(&scheme.as_str()) {
        return Err(error(format!("unsupported ICE scheme `{scheme}`")));
    }
    let rest = rest.trim_start_matches('/');
    if rest.is_empty() {
        return Err(error("ICE server address is missing a host"));
    }

    let default_port = match scheme.as_str() {
        "stuns" | "turns" => 5349,
        _ => 3478,
    };
    let rest = if has_port(rest) {
        rest.to_owned()
    } else {
        format!("{rest}:{default_port}")
    };
    validate_host_port(&rest)?;
    Ok(format!("{scheme}:{rest}"))
}

fn has_port(rest: &str) -> bool {
    if let Some(close) = rest.rfind(']') {
        // Bracketed IPv6: a port must follow the closing bracket.
        rest[close + 1..].starts_with(':')
    } else {
        rest.contains(':')
    }
}

fn validate_host_port(rest: &str) -> Result<(), SignalError> {
    let port_text = if let Some(close) = rest.rfind(']') {
        rest[close + 1..].strip_prefix(':')
    } else if let Some((_, port)) = rest.rsplit_once(':') {
        Some(port)
    } else {
        None
    };
    let port_text = port_text.ok_or_else(|| error("ICE server address is missing a port"))?;
    let port: u16 = port_text
        .parse()
        .map_err(|_| error(format!("ICE server port `{port_text}` is not a valid u16")))?;
    if port == 0 {
        return Err(error("ICE server port must be greater than zero"));
    }
    let host = rest.split_once(':').map(|(host, _)| host).unwrap_or(rest);
    if host.trim_matches(['[', ']']).is_empty() {
        return Err(error("ICE server address is missing a host"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_offer() {
        let message =
            parse_message(r#"{"type":"offer","sdp":"v=0\r\nm=video 9 x 96\r\n"}"#).expect("parse");
        assert!(matches!(message, ClientMessage::Offer { .. }));
    }

    #[test]
    fn rejects_empty_offer_and_missing_type() {
        assert!(parse_message(r#"{"type":"offer","sdp":"  "}"#).is_err());
        assert!(parse_message(r#"{"sdp":"x"}"#).is_err());
        assert!(parse_message("not json").is_err());
        assert!(parse_message("[]").is_err());
    }

    #[test]
    fn parses_candidate_alias_and_defaults_mline_to_zero() {
        let message = parse_message(
            r#"{"type":"candidate","candidate":"candidate:1 1 UDP 1 192.0.2.1 5 typ host"}"#,
        )
        .expect("parse");
        match message {
            ClientMessage::AddIceCandidate {
                session,
                candidate,
                sdp_mline_index,
            } => {
                assert_eq!(session, None);
                assert!(candidate.starts_with("candidate:"));
                assert_eq!(sdp_mline_index, 0);
            }
            other => panic!("unexpected {other:?}"),
        }
    }

    #[test]
    fn parses_numeric_string_mline_and_rejects_negative() {
        let ok = parse_message(
            r#"{"type":"add-ice-candidate","candidate":"candidate:1 1 UDP 1 1.2.3.4 5 typ host","sdpMLineIndex":"1"}"#,
        )
        .expect("numeric string");
        assert!(matches!(
            ok,
            ClientMessage::AddIceCandidate {
                sdp_mline_index: 1,
                ..
            }
        ));
        assert!(parse_message(
            r#"{"type":"add-ice-candidate","candidate":"candidate:1 1 UDP 1 1.2.3.4 5 typ host","sdpMLineIndex":-1}"#,
        )
        .is_err());
    }

    #[test]
    fn rejects_bad_candidates() {
        assert!(validate_candidate("").is_err());
        assert!(validate_candidate("hello").is_err());
        assert!(validate_candidate("candidate:1 1 UDP 1 1.2.3.4 5").is_err());
        assert!(validate_candidate("candidate:1 1 UDP 1 1.2.3.4 5 typ host\0").is_err());
        assert_eq!(
            validate_candidate("a=candidate:1 1 UDP 1 1.2.3.4 5 typ host").unwrap(),
            "candidate:1 1 UDP 1 1.2.3.4 5 typ host"
        );
    }

    #[test]
    fn parses_bitrate_number_and_string_and_clamps() {
        assert_eq!(
            parse_message(r#"{"type":"bitrate","bitrate":2500000}"#).unwrap(),
            ClientMessage::Bitrate {
                bitrate_bps: 2_500_000
            }
        );
        assert_eq!(
            parse_message(r#"{"type":"bitrate","bitrate":"2500000"}"#).unwrap(),
            ClientMessage::Bitrate {
                bitrate_bps: 2_500_000
            }
        );
        assert!(parse_message(r#"{"type":"bitrate","bitrate":-5}"#).is_err());
        assert_eq!(bitrate_kbps_for(100_000), (500_000, 500));
        assert_eq!(bitrate_kbps_for(60_000_000), (50_000_000, 50_000));
        assert_eq!(bitrate_kbps_for(4_000_000), (4_000_000, 4_000));
        assert_eq!(bitrate_bounds(), (500_000, 50_000_000));
    }

    #[test]
    fn response_shapes_match_the_node_contract() {
        let answer = answer_response("v=0\r\n", "abc12345");
        let value: Value = serde_json::from_str(&answer).unwrap();
        assert_eq!(value["type"], "answer");
        assert_eq!(value["sdp"], "v=0\r\n");
        assert_eq!(value["session"], "abc12345");

        let ack: Value = serde_json::from_str(&candidate_ack(Some("s"), 2)).unwrap();
        assert_eq!(ack["ok"], true);
        assert_eq!(ack["remote_candidates"], 2);

        let bitrate: Value = serde_json::from_str(&bitrate_ack(4_000_000)).unwrap();
        assert_eq!(bitrate["ok"], true);
        assert_eq!(bitrate["bitrate"], 4_000_000u64);

        let failure: Value = serde_json::from_str(&error_ack("bad")).unwrap();
        assert_eq!(failure["ok"], false);
        assert_eq!(failure["error"], "bad");

        let offer_error: Value = serde_json::from_str(&error_response("bad")).unwrap();
        assert_eq!(offer_error["type"], "error");
    }

    #[test]
    fn normalises_stun_forms() {
        assert_eq!(
            normalize_ice_server("stun:stun.example.org:19302", IceKind::Stun).unwrap(),
            "stun:stun.example.org:19302"
        );
        assert_eq!(
            normalize_ice_server("stun://stun.example.org:19302", IceKind::Stun).unwrap(),
            "stun:stun.example.org:19302"
        );
        assert_eq!(
            normalize_ice_server("stun.example.org", IceKind::Stun).unwrap(),
            "stun:stun.example.org:3478"
        );
        assert_eq!(
            normalize_ice_server("192.0.2.1:19302", IceKind::Stun).unwrap(),
            "stun:192.0.2.1:19302"
        );
    }

    #[test]
    fn normalises_turn_and_rejects_bad_forms() {
        assert_eq!(
            normalize_ice_server("turn://turn.example.org:3478", IceKind::Turn).unwrap(),
            "turn:turn.example.org:3478"
        );
        assert_eq!(
            normalize_ice_server("turn.example.org", IceKind::Turn).unwrap(),
            "turn:turn.example.org:3478"
        );
        assert_eq!(
            normalize_ice_server("turns://turn.example.org", IceKind::Turn).unwrap(),
            "turns:turn.example.org:5349"
        );
        assert!(normalize_ice_server("stun:host:1", IceKind::Turn).is_err());
        assert!(normalize_ice_server("turn:host:1", IceKind::Stun).is_err());
        assert!(normalize_ice_server("user:pass@host:3478", IceKind::Turn).is_err());
        assert!(normalize_ice_server("turn:host:notaport", IceKind::Turn).is_err());
        assert!(normalize_ice_server("turn:host:0", IceKind::Turn).is_err());
        assert!(normalize_ice_server("turn:   ", IceKind::Turn).is_err());
    }
}
