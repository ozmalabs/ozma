// SPDX-License-Identifier: AGPL-3.0-only
//! Helpers for building transport descriptors and session offers.
//!
//! The types themselves live in `ozma-protocol`; this module only makes them
//! hard to construct incorrectly. The node builds an offer after path
//! selection and returns it to the client unchanged.

use ozma_protocol::{
    codes, ApiError, ChannelDescriptor, Result, SessionId, SessionOffer, TransportDescriptor,
    TransportMode,
};

use crate::channel::ChannelMap;

/// Builder for one [`TransportDescriptor`].
///
/// ```no_run
/// use ozma_transport::offer::TransportDescriptorBuilder;
/// use ozma_protocol::TransportMode;
///
/// let descriptor = TransportDescriptorBuilder::new(TransportMode::Quic)
///     .endpoint("192.0.2.10:4433")
///     .secure(true)
///     .alpn("ozma/1")
///     .build();
/// assert_eq!(descriptor.mode, TransportMode::Quic);
/// ```
#[derive(Clone, Debug)]
pub struct TransportDescriptorBuilder {
    mode: TransportMode,
    endpoint: Option<String>,
    secure: bool,
    alpn: Option<String>,
    certificate_fingerprint: Option<String>,
    channels: ChannelMap,
}

impl TransportDescriptorBuilder {
    /// Start a descriptor with the default session channel layout.
    pub fn new(mode: TransportMode) -> Self {
        Self {
            mode,
            endpoint: None,
            secure: false,
            alpn: None,
            certificate_fingerprint: None,
            channels: ChannelMap::default_session(),
        }
    }

    /// Start a QUIC descriptor with the standard ALPN and default channels.
    pub fn quic(endpoint: impl Into<String>) -> Self {
        Self::new(TransportMode::Quic)
            .endpoint(endpoint)
            .secure(true)
            .alpn(crate::DEFAULT_ALPN)
    }

    /// Start a raw UDP/RTP descriptor with default channels.
    pub fn udp(endpoint: impl Into<String>) -> Self {
        Self::new(TransportMode::Udp).endpoint(endpoint)
    }

    /// Start an RTSP TCP-interleaved descriptor with default channels.
    pub fn tcp_interleaved(endpoint: impl Into<String>) -> Self {
        Self::new(TransportMode::TcpInterleaved).endpoint(endpoint)
    }

    /// Start a relay descriptor with default channels.
    pub fn relay(endpoint: impl Into<String>) -> Self {
        Self::new(TransportMode::Relay)
            .endpoint(endpoint)
            .secure(true)
    }

    /// Start an in-process loopback descriptor with default channels.
    pub fn loopback() -> Self {
        Self::new(TransportMode::Loopback)
    }

    /// Set the endpoint string (host:port, path, or relay URL).
    pub fn endpoint(mut self, endpoint: impl Into<String>) -> Self {
        self.endpoint = Some(endpoint.into());
        self
    }

    /// Mark whether the transport is authenticated/encrypted.
    pub fn secure(mut self, secure: bool) -> Self {
        self.secure = secure;
        self
    }

    /// Set the ALPN protocol, normally [`crate::DEFAULT_ALPN`].
    pub fn alpn(mut self, alpn: impl Into<String>) -> Self {
        self.alpn = Some(alpn.into());
        self
    }

    /// Pin a certificate fingerprint (hex SHA-256 is the convention).
    pub fn certificate_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.certificate_fingerprint = Some(fingerprint.into());
        self
    }

    /// Replace the channel layout.
    pub fn channels(mut self, channels: ChannelMap) -> Self {
        self.channels = channels;
        self
    }

    /// Replace the channel layout from raw protocol descriptors.
    pub fn channel_descriptors(
        mut self,
        descriptors: impl IntoIterator<Item = ChannelDescriptor>,
    ) -> Result<Self> {
        self.channels = ChannelMap::new(descriptors)?;
        Ok(self)
    }

    /// Finish the descriptor.
    pub fn build(self) -> TransportDescriptor {
        TransportDescriptor {
            mode: self.mode,
            endpoint: self.endpoint,
            secure: self.secure,
            alpn: self.alpn,
            certificate_fingerprint: self.certificate_fingerprint,
            channels: self.channels.descriptors(),
        }
    }
}

/// Builder for a short-lived [`SessionOffer`].
///
/// An offer is only valid with a non-empty token, a non-zero expiry, at least
/// one transport, and valid channels on every transport.
#[derive(Clone, Debug)]
pub struct SessionOfferBuilder {
    session_id: SessionId,
    token: Option<String>,
    expires_at: u64,
    transports: Vec<TransportDescriptor>,
}

impl SessionOfferBuilder {
    /// Start an offer for a session.
    pub fn new(session_id: impl Into<SessionId>) -> Self {
        Self {
            session_id: session_id.into(),
            token: None,
            expires_at: 0,
            transports: Vec::new(),
        }
    }

    /// Set the scoped session token.
    pub fn token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Set the absolute expiry in Unix milliseconds.
    pub fn expires_at(mut self, expires_at: u64) -> Self {
        self.expires_at = expires_at;
        self
    }

    /// Set the expiry as `now_ms + ttl_seconds * 1000`, saturating on overflow.
    pub fn expires_in(mut self, now_ms: u64, ttl_seconds: u64) -> Self {
        self.expires_at = now_ms.saturating_add(ttl_seconds.saturating_mul(1000));
        self
    }

    /// Add a transport the client may use.
    pub fn transport(mut self, descriptor: TransportDescriptor) -> Self {
        self.transports.push(descriptor);
        self
    }

    /// Add every transport from an iterator.
    pub fn transports(
        mut self,
        descriptors: impl IntoIterator<Item = TransportDescriptor>,
    ) -> Self {
        self.transports.extend(descriptors);
        self
    }

    /// Validate and finish the offer.
    pub fn build(self) -> Result<SessionOffer> {
        let token = self
            .token
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                ApiError::new(
                    codes::INVALID_REQUEST,
                    "session offer requires a non-empty token",
                )
            })?;
        if self.expires_at == 0 {
            return Err(ApiError::new(
                codes::INVALID_REQUEST,
                "session offer requires a non-zero expiry",
            ));
        }
        if self.transports.is_empty() {
            return Err(ApiError::new(
                codes::TRANSPORT_UNAVAILABLE,
                "session offer requires at least one transport",
            )
            .with_remediation("Add a direct or relay path before offering the session."));
        }
        for descriptor in &self.transports {
            validate_descriptor(descriptor)?;
        }
        Ok(SessionOffer {
            session_id: self.session_id,
            token,
            transports: self.transports,
            expires_at: self.expires_at,
        })
    }
}

fn validate_descriptor(descriptor: &TransportDescriptor) -> Result<()> {
    if descriptor.channels.is_empty() {
        return Err(ApiError::new(
            codes::INVALID_REQUEST,
            format!("transport {:?} has no channels", descriptor.mode),
        ));
    }
    if descriptor
        .endpoint
        .as_deref()
        .is_some_and(|endpoint| endpoint.trim().is_empty())
    {
        return Err(ApiError::new(
            codes::INVALID_REQUEST,
            "transport endpoint must not be empty",
        ));
    }
    ChannelMap::new(descriptor.channels.iter().cloned())?;
    Ok(())
}

/// Convenience form of [`SessionOfferBuilder`] for the common case.
pub fn session_offer(
    session_id: impl Into<SessionId>,
    token: impl Into<String>,
    expires_at: u64,
    transports: impl IntoIterator<Item = TransportDescriptor>,
) -> Result<SessionOffer> {
    SessionOfferBuilder::new(session_id)
        .token(token)
        .expires_at(expires_at)
        .transports(transports)
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn loopback_descriptor() -> TransportDescriptor {
        TransportDescriptorBuilder::loopback().build()
    }

    #[test]
    fn descriptor_builder_fills_sensible_defaults() {
        let descriptor = TransportDescriptorBuilder::quic("127.0.0.1:4433").build();
        assert_eq!(descriptor.mode, TransportMode::Quic);
        assert_eq!(descriptor.endpoint.as_deref(), Some("127.0.0.1:4433"));
        assert!(descriptor.secure);
        assert_eq!(descriptor.alpn.as_deref(), Some("ozma/1"));
        assert_eq!(descriptor.channels.len(), 9);
        assert!(
            descriptor
                .channels
                .iter()
                .any(|channel| channel.kind == ozma_protocol::ChannelKind::Camera),
            "the default layout carries the browser camera channel"
        );
    }

    #[test]
    fn offer_builder_validates_required_fields() {
        let error = SessionOfferBuilder::new("s1")
            .expires_at(1)
            .transport(loopback_descriptor())
            .build()
            .unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);

        let error = SessionOfferBuilder::new("s1")
            .token("t")
            .expires_at(0)
            .transport(loopback_descriptor())
            .build()
            .unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);

        let error = SessionOfferBuilder::new("s1")
            .token("t")
            .expires_at(1)
            .build()
            .unwrap_err();
        assert_eq!(error.code, codes::TRANSPORT_UNAVAILABLE);
    }

    #[test]
    fn offer_builder_round_trips_protocol_fields() {
        let offer = SessionOfferBuilder::new("session-1")
            .token("short-lived")
            .expires_in(1_700_000_000_000, 60)
            .transport(loopback_descriptor())
            .build()
            .unwrap();
        assert_eq!(offer.session_id, SessionId::new("session-1"));
        assert_eq!(offer.token, "short-lived");
        assert_eq!(offer.expires_at, 1_700_000_060_000);
        assert_eq!(offer.transports.len(), 1);
    }

    #[test]
    fn a_descriptor_without_channels_is_rejected() {
        let mut descriptor = loopback_descriptor();
        descriptor.channels.clear();
        let error = SessionOfferBuilder::new("s1")
            .token("t")
            .expires_at(1)
            .transport(descriptor)
            .build()
            .unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }
}
