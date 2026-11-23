// SPDX-License-Identifier: AGPL-3.0-only
//! Browser WebTransport adapter for Ozma sessions.
//!
//! The node exposes a WebTransport endpoint that carries the same channel
//! framing as the native QUIC adapter: one reliable bidirectional stream for
//! ordered channels, datagrams for unreliable channels, and unidirectional
//! streams for resources. The wasm-only part of this crate wraps the browser's
//! `WebTransport` object and implements
//! [`ozma_client_core::ChannelTransport`].
//!
//! # Handshake
//!
//! [`BrowserTransport`] is a byte pipe only. The caller layers
//! [`ozma_client_core::ClientDriver`] on top, which sends
//! `ControlMessage::Hello` as the first reliable control frame and waits for
//! `HelloAck`, exactly like `ozma-node/src/transport.rs::read_hello`.
//!
//! # Cloned senders
//!
//! [`BrowserTransport::sender`] returns a [`BrowserSender`] that shares the
//! reliable writer queue, the datagram writer, the WebTransport handle, and the
//! closed flag with the transport. A facade can keep a sender outside the
//! media loop, so input frames and uplink camera/microphone datagrams do not
//! wait for a pending [`ozma_client_core::ChannelTransport::recv`].
//!
//! # Certificate pinning
//!
//! Self-signed node certificates must be pinned with
//! [`BrowserConnectOptions::certificate_sha256`]. Browsers require the pinned
//! certificate to be ECDSA with a validity of at most 14 days; a mismatch
//! surfaces as [`WebTransportError::Certificate`].
//!
//! # Building
//!
//! `WebTransport`, `WebTransportOptions`, `WebTransportHash`, and the
//! `VideoDecoder`/`AudioDecoder` APIs are marked unstable by `web-sys`, so the
//! wasm check/build must enable them explicitly:
//!
//! ```text
//! RUSTFLAGS="--cfg=web_sys_unstable_apis" \
//!     cargo check -p ozma-transport-web --target wasm32-unknown-unknown
//! ```
//!
//! Do not put this in a workspace-wide `.cargo/config.toml`: it would affect
//! native crates. The flag belongs on the wasm build command (or in the
//! browser client's build script).
//!
//! # Certificate hashing
//!
//! [`certificate_sha256_der`] and [`certificate_sha256_from_base64`] convert
//! the node's `/api/v1/transport` `certificate_der_base64` into the 32-byte
//! pin the browser needs. Keeping the helper here means `ozma-web` does not
//! need its own `sha2`/`base64` dependency.
//!
//! # Timeouts and the event loop
//!
//! Every network operation runs in a `spawn_local` task and reports through
//! typed events, so [`ozma_client_core::ChannelTransport::recv`] never blocks
//! the browser event loop. The adapter owns no timer; connect/handshake
//! timeouts belong to the caller (`AbortSignal`, `setTimeout`, or a
//! `wasm-bindgen-futures` select).

#![forbid(unsafe_code)]

use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use ozma_client_core::session::codes as client_codes;
use ozma_protocol::{ApiError, TransportDescriptor, TransportMode};
use ozma_transport::ChannelMap;
use sha2::{Digest, Sha256};

mod queue;

#[cfg(target_arch = "wasm32")]
mod transport;

pub use queue::{FrameWriter, SendQueue, SendQueueError};
#[cfg(target_arch = "wasm32")]
pub use transport::{BrowserSender, BrowserTransport};

/// Default cap for one reliable frame payload, matching the protocol's
/// `u16::MAX` channel limit.
pub const DEFAULT_MAX_PAYLOAD: usize = u16::MAX as usize;

/// Everything [`BrowserTransport::connect`] needs.
#[derive(Clone, Debug)]
pub struct BrowserConnectOptions {
    /// `https://` WebTransport URL from the session offer.
    pub url: String,
    /// SHA-256 of the node's DER certificate, for `serverCertificateHashes`.
    pub certificate_sha256: [u8; 32],
    /// Channels negotiated in the offer's transport descriptor.
    pub channel_map: ChannelMap,
    /// Maximum reliable frame payload accepted or sent. Datagrams are also
    /// capped by the browser's `maxDatagramSize`.
    pub max_payload: usize,
}

impl BrowserConnectOptions {
    /// Build options for an `https://` URL and a pinned certificate hash.
    pub fn new(url: impl Into<String>, certificate_sha256: [u8; 32]) -> Self {
        Self {
            url: url.into(),
            certificate_sha256,
            channel_map: ChannelMap::default_session(),
            max_payload: DEFAULT_MAX_PAYLOAD,
        }
    }

    /// Use the offer's negotiated channel map.
    pub fn with_channel_map(mut self, channel_map: ChannelMap) -> Self {
        self.channel_map = channel_map;
        self
    }

    /// Override the reliable frame payload limit.
    pub fn with_max_payload(mut self, max_payload: usize) -> Self {
        self.max_payload = max_payload.max(1);
        self
    }

    /// Build options directly from the offer's WebTransport descriptor.
    ///
    /// The descriptor must be [`TransportMode::Quic`] with an `https://`
    /// endpoint and a hex SHA-256 `certificate_fingerprint`. An empty channel
    /// list falls back to the standard session layout.
    pub fn from_descriptor(descriptor: &TransportDescriptor) -> Result<Self, WebTransportError> {
        let url = descriptor
            .endpoint
            .as_deref()
            .filter(|endpoint| endpoint.starts_with("https://"))
            .ok_or_else(|| {
                WebTransportError::Unsupported(
                    "the transport descriptor has no https:// endpoint".into(),
                )
            })?;
        let fingerprint = descriptor
            .certificate_fingerprint
            .as_deref()
            .ok_or_else(|| {
                WebTransportError::Certificate(
                    "the transport descriptor has no certificate fingerprint".into(),
                )
            })?;
        let certificate_sha256 = certificate_sha256_from_fingerprint(fingerprint)?;
        let channel_map = ozma_client_core::channel_map_for(descriptor)
            .map_err(|error| WebTransportError::Protocol(error.message))?;
        Ok(Self {
            url: url.to_string(),
            certificate_sha256,
            channel_map,
            max_payload: DEFAULT_MAX_PAYLOAD,
        })
    }

    /// True when this descriptor looks like a browser WebTransport path.
    pub fn matches(descriptor: &TransportDescriptor) -> bool {
        descriptor.mode == TransportMode::Quic
            && descriptor
                .endpoint
                .as_deref()
                .is_some_and(|endpoint| endpoint.starts_with("https://"))
    }
}

/// A typed browser transport failure.
#[derive(Debug, Clone, thiserror::Error)]
pub enum WebTransportError {
    /// WebTransport is unavailable, the URL is not `https://`, or the
    /// connection cannot carry datagrams.
    #[error("WebTransport is not available: {0}")]
    Unsupported(String),
    /// The connection or handshake failed.
    #[error("WebTransport connection failed: {0}")]
    Connect(String),
    /// Certificate pinning or validation failed.
    #[error("WebTransport certificate pinning failed: {0}")]
    Certificate(String),
    /// The transport is closed.
    #[error("WebTransport is closed: {0}")]
    Closed(String),
    /// A stream or datagram operation failed.
    #[error("WebTransport I/O error: {0}")]
    Io(String),
    /// Framing or channel negotiation was violated.
    #[error("WebTransport protocol error: {0}")]
    Protocol(String),
    /// A payload exceeded the negotiated or browser limit.
    #[error("payload of {len} bytes exceeds the {max}-byte limit on channel {channel}")]
    PayloadTooLarge { channel: u8, len: usize, max: usize },
}

impl WebTransportError {
    /// Convert to the shared client error vocabulary.
    pub fn into_api_error(self) -> ApiError {
        match self {
            Self::Unsupported(message) => ApiError::new(client_codes::CLIENT_UNSUPPORTED, message)
                .with_remediation(
                    "Use a browser with WebTransport datagram support and an https:// node URL.",
                ),
            Self::Certificate(message) => ApiError::new(client_codes::CLIENT_CERTIFICATE, message)
                .with_remediation(
                    "Fetch /api/v1/transport and pin the certificate SHA-256 the node advertises.",
                ),
            Self::Connect(message) | Self::Io(message) => {
                ApiError::new(ozma_protocol::codes::TRANSPORT_UNAVAILABLE, message).retryable()
            }
            Self::Closed(message) => ApiError::new(client_codes::CLIENT_DISCONNECTED, message)
                .with_remediation("Reconnect the session and retry."),
            Self::Protocol(message) => ApiError::new(client_codes::CLIENT_PROTOCOL, message),
            Self::PayloadTooLarge { channel, len, max } => {
                ozma_transport::payload_too_large(ozma_transport::ChannelId::new(channel), len, max)
            }
        }
    }
}

impl From<WebTransportError> for ApiError {
    fn from(error: WebTransportError) -> Self {
        error.into_api_error()
    }
}

/// SHA-256 of a DER certificate, ready for `serverCertificateHashes`.
pub fn certificate_sha256_der(der: &[u8]) -> [u8; 32] {
    let digest = Sha256::digest(der);
    let mut hash = [0u8; 32];
    hash.copy_from_slice(&digest);
    hash
}

/// SHA-256 of base64 DER, as served by `/api/v1/transport`.
pub fn certificate_sha256_from_base64(
    certificate_der_base64: &str,
) -> Result<[u8; 32], WebTransportError> {
    let der = STANDARD
        .decode(certificate_der_base64.trim())
        .or_else(|_| URL_SAFE_NO_PAD.decode(certificate_der_base64.trim()))
        .map_err(|error| {
            WebTransportError::Certificate(format!(
                "the certificate_der_base64 value is not valid base64: {error}"
            ))
        })?;
    if der.is_empty() {
        return Err(WebTransportError::Certificate(
            "the certificate_der_base64 value is empty".into(),
        ));
    }
    Ok(certificate_sha256_der(&der))
}

/// Parse a hex SHA-256 fingerprint (`aa:bb:...`, `sha-256:...`, or plain hex).
pub fn certificate_sha256_from_fingerprint(
    fingerprint: &str,
) -> Result<[u8; 32], WebTransportError> {
    let trimmed = fingerprint.trim();
    let without_prefix = trimmed
        .strip_prefix("sha-256:")
        .or_else(|| trimmed.strip_prefix("sha256:"))
        .unwrap_or(trimmed);
    let digits: String = without_prefix
        .chars()
        .filter(|character| *character != ':' && *character != ' ' && *character != '-')
        .collect();
    if digits.len() != 64 {
        return Err(WebTransportError::Certificate(format!(
            "a SHA-256 fingerprint must be 64 hex digits, got {}",
            digits.len()
        )));
    }
    let mut hash = [0u8; 32];
    for (index, slot) in hash.iter_mut().enumerate() {
        let pair = &digits[index * 2..index * 2 + 2];
        *slot = u8::from_str_radix(pair, 16).map_err(|_| {
            WebTransportError::Certificate(format!(
                "the certificate fingerprint contains non-hex characters: {pair}"
            ))
        })?;
    }
    Ok(hash)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base64_and_hex_helpers_agree() {
        let der = b"not a real certificate";
        let hash = certificate_sha256_der(der);
        let encoded = STANDARD.encode(der);
        assert_eq!(certificate_sha256_from_base64(&encoded).unwrap(), hash);
        let fingerprint = hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<String>();
        assert_eq!(
            certificate_sha256_from_fingerprint(&fingerprint).unwrap(),
            hash
        );
        let colonized = hash
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect::<Vec<_>>()
            .join(":");
        assert_eq!(
            certificate_sha256_from_fingerprint(&colonized).unwrap(),
            hash
        );
    }

    #[test]
    fn bad_fingerprints_are_typed() {
        assert!(matches!(
            certificate_sha256_from_fingerprint("abcd").unwrap_err(),
            WebTransportError::Certificate(_)
        ));
        assert!(matches!(
            certificate_sha256_from_base64("!!!!").unwrap_err(),
            WebTransportError::Certificate(_)
        ));
    }

    #[test]
    fn payload_too_large_maps_to_a_channel_error() {
        let error = WebTransportError::PayloadTooLarge {
            channel: 2,
            len: 2000,
            max: 1200,
        }
        .into_api_error();
        assert_eq!(error.code, ozma_protocol::codes::INVALID_REQUEST);
    }

    #[test]
    fn descriptor_matching_requires_https_quic() {
        use ozma_protocol::TransportMode;
        let mut descriptor = TransportDescriptor {
            mode: TransportMode::Quic,
            endpoint: Some("https://node.example:4433".into()),
            secure: true,
            alpn: Some("ozma/1".into()),
            certificate_fingerprint: Some("00".repeat(32)),
            channels: Vec::new(),
        };
        assert!(BrowserConnectOptions::matches(&descriptor));
        let options = BrowserConnectOptions::from_descriptor(&descriptor).unwrap();
        assert_eq!(options.url, "https://node.example:4433");
        assert_eq!(options.channel_map, ChannelMap::default_session());

        descriptor.endpoint = Some("192.0.2.1:4433".into());
        assert!(!BrowserConnectOptions::matches(&descriptor));
    }
}
