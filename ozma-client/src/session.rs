// SPDX-License-Identifier: AGPL-3.0-only
//! Native QUIC session client.
//!
//! The client connects directly to a node using the endpoint and certificate
//! from a [`SessionOffer`]. It sends `Hello`, waits for `HelloAck`, then
//! exposes control events, opaque media packets, input, and shutdown.
//!
//! Video, audio, and microphone payloads are never decoded here. Each media
//! packet carries its [`ChannelId`], the channel kind and track from the
//! negotiated channel map, and a client-side arrival sequence number.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::path::Path;
use std::time::Duration;

use bytes::Bytes;
use ozma_protocol::{
    codes, ApiError, ChannelKind, ControlMessage, EndReason, InputEvent, Permission, PermissionSet,
    Session, SessionEvent, SessionOffer, SessionState, TrackId, TransportDescriptor, TransportMode,
};
use ozma_transport::{
    ChannelId, ChannelMap, CloseReason, QuicClient, QuicConfig, QuicTransport, SessionTransport,
    TransportEvent,
};
use serde_json::{json, Value};

use crate::cert;
use crate::error;
use crate::node_api::NodeApiClient;

/// Client-side connection policy.
#[derive(Clone, Debug)]
pub struct NativeClientConfig {
    /// TLS server name used for certificate validation. The node's generated
    /// certificate uses `localhost` by default.
    pub server_name: String,
    /// QUIC handshake and control-stream timeout.
    pub handshake_timeout: Duration,
    /// How long to wait for `HelloAck` after sending `Hello`.
    pub hello_timeout: Duration,
    /// QUIC idle timeout.
    pub idle_timeout: Duration,
    /// Grace period after sending `Bye` before closing the QUIC connection.
    /// This lets queued control frames (including `ReleaseAll`) reach the node.
    pub close_grace: Duration,
}

impl Default for NativeClientConfig {
    fn default() -> Self {
        Self {
            server_name: "localhost".to_string(),
            handshake_timeout: Duration::from_secs(5),
            hello_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            close_grace: Duration::from_millis(50),
        }
    }
}

impl NativeClientConfig {
    /// A configuration with the default server name and timings.
    pub fn new() -> Self {
        Self::default()
    }

    pub fn with_server_name(mut self, server_name: impl Into<String>) -> Self {
        self.server_name = server_name.into();
        self
    }

    pub fn with_hello_timeout(mut self, timeout: Duration) -> Self {
        self.hello_timeout = timeout;
        self
    }

    /// Set how long `close` waits for queued control frames to flush.
    pub fn with_close_grace(mut self, grace: Duration) -> Self {
        self.close_grace = grace;
        self
    }
}

/// One opaque media payload received from the node.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPacket {
    /// Wire channel that carried the payload.
    pub channel: ChannelId,
    /// Logical kind from the negotiated channel map.
    pub kind: ChannelKind,
    /// Video track id, when the channel maps to a track.
    pub track_id: Option<TrackId>,
    /// Client-side arrival sequence (monotonic per client, not a wire field).
    pub sequence: u64,
    /// Encoded bytes exactly as the node sent them.
    pub payload: Bytes,
}

impl MediaPacket {
    /// Payload length in bytes.
    pub fn len(&self) -> usize {
        self.payload.len()
    }

    /// True when the payload has zero bytes.
    pub fn is_empty(&self) -> bool {
        self.payload.is_empty()
    }
}

/// An event received from a native session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// Video, audio, mic, telemetry, or bulk bytes from the node.
    Media(MediaPacket),
    /// A session lifecycle event from the reliable control channel.
    Session(SessionEvent),
    /// The server sent another `HelloAck` (normally only once, during connect).
    SessionUpdated(Session),
    /// Reply to a client `Ping`.
    Pong { nonce: u64 },
    /// A non-terminal error from the node. The same error is retained and
    /// returned by later input calls, which is how view-only/control denial is
    /// surfaced.
    Error { error: ApiError },
    /// The node ended the session.
    Bye { reason: EndReason },
    /// A control message type this client does not know. The session remains
    /// usable, which keeps the client forward-compatible with newer nodes.
    UnknownControl { kind: String },
    /// The transport is closed. This event is returned again on later calls.
    Closed { reason: CloseReason },
}

impl ClientEvent {
    /// True when this is a video media packet.
    pub fn is_video(&self) -> bool {
        matches!(
            self,
            ClientEvent::Media(MediaPacket {
                kind: ChannelKind::Video,
                ..
            })
        )
    }
}

/// A connected native session.
pub struct NativeSessionClient {
    transport: QuicTransport,
    channels: ChannelMap,
    descriptor: TransportDescriptor,
    offer: SessionOffer,
    session: Option<Session>,
    pending: VecDeque<ClientEvent>,
    media_sequence: u64,
    server_error: Option<ApiError>,
    end_reason: Option<EndReason>,
    closed: Option<CloseReason>,
    close_grace: Duration,
}

impl std::fmt::Debug for NativeSessionClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NativeSessionClient")
            .field("session_id", &self.offer.session_id)
            .field("state", &self.session.as_ref().map(|session| session.state))
            .field("channels", &self.channels.len())
            .field("closed", &self.closed)
            .field(
                "server_error",
                &self.server_error.as_ref().map(|error| &error.code),
            )
            .finish()
    }
}

impl NativeSessionClient {
    /// Connect with the default client configuration.
    pub async fn connect(
        offer: &SessionOffer,
        certificate_der: impl AsRef<[u8]>,
    ) -> Result<Self, ApiError> {
        Self::connect_with_config(offer, certificate_der, NativeClientConfig::default()).await
    }

    /// Connect and perform `Hello` / `HelloAck`.
    pub async fn connect_with_config(
        offer: &SessionOffer,
        certificate_der: impl AsRef<[u8]>,
        config: NativeClientConfig,
    ) -> Result<Self, ApiError> {
        let certificate_der = certificate_der.as_ref();
        if certificate_der.is_empty() {
            return Err(error::certificate(
                "A DER certificate is required to pin the node's QUIC connection",
            )
            .with_remediation(
                "Pass --cert, or fetch /api/v1/transport from the node with --node.",
            ));
        }

        let (descriptor, channels) = select_quic_transport(offer)?;
        cert::verify_certificate_fingerprint(
            certificate_der,
            descriptor.certificate_fingerprint.as_deref().unwrap_or(""),
        )?;

        let endpoint = descriptor
            .endpoint
            .as_deref()
            .ok_or_else(|| error::config("The QUIC transport has no endpoint"))?;
        let address = resolve_endpoint(endpoint).await?;

        let mut quic = QuicConfig::default()
            .with_handshake_timeout(config.handshake_timeout)
            .with_idle_timeout(config.idle_timeout);
        if let Some(alpn) = descriptor.alpn.as_deref().filter(|alpn| !alpn.is_empty()) {
            quic = quic.with_alpn(alpn.to_string());
        }

        let transport = QuicClient::connect_with_config(
            address,
            &config.server_name,
            certificate_der,
            quic,
            channels.clone(),
        )
        .await?;

        let mut client = Self {
            transport,
            channels,
            descriptor,
            offer: offer.clone(),
            session: None,
            pending: VecDeque::new(),
            media_sequence: 0,
            server_error: None,
            end_reason: None,
            closed: None,
            close_grace: config.close_grace,
        };

        // Build Hello as JSON rather than via ControlMessage so an optional
        // future field (for example `display`) can be added to the protocol
        // without breaking this client. The node ignores unknown fields.
        let hello = json!({
            "type": "hello",
            "data": {
                "session_id": offer.session_id,
                "token": offer.token,
            }
        });
        client.send_raw_control(&hello).await?;

        let session = tokio::time::timeout(config.hello_timeout, client.await_hello_ack())
            .await
            .map_err(|_| {
                error::timeout("waiting for the session HelloAck", config.hello_timeout)
            })??;
        client.session = Some(session);
        Ok(client)
    }

    /// Connect using a DER/PEM certificate file.
    pub async fn connect_with_cert_file(
        offer: &SessionOffer,
        path: impl AsRef<Path>,
        config: NativeClientConfig,
    ) -> Result<Self, ApiError> {
        let certificate_der = cert::load_certificate(path)?;
        Self::connect_with_config(offer, certificate_der, config).await
    }

    /// Connect by fetching `/api/v1/transport` from the node API.
    ///
    /// This is the headless path: the node returns base64 DER and its
    /// fingerprint, and this method verifies both before pinning QUIC.
    pub async fn connect_with_node_api(
        offer: &SessionOffer,
        node: &NodeApiClient,
        config: NativeClientConfig,
    ) -> Result<Self, ApiError> {
        let transport = node.transport_info().await?;
        Self::connect_with_config(offer, &transport.certificate_der, config).await
    }

    /// The session record from `HelloAck`.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// The session state from `HelloAck`.
    pub fn state(&self) -> Option<SessionState> {
        self.session.as_ref().map(|session| session.state)
    }

    /// The permissions granted in `HelloAck`.
    pub fn permissions(&self) -> PermissionSet {
        self.session
            .as_ref()
            .map(|session| session.permissions.clone())
            .unwrap_or_default()
    }

    /// The offer used to connect.
    pub fn offer(&self) -> &SessionOffer {
        &self.offer
    }

    /// The negotiated channel map.
    pub fn channel_map(&self) -> &ChannelMap {
        &self.channels
    }

    /// The transport descriptor used to connect.
    pub fn transport_descriptor(&self) -> &TransportDescriptor {
        &self.descriptor
    }

    /// The last error reported by the node, if any.
    pub fn server_error(&self) -> Option<&ApiError> {
        self.server_error.as_ref()
    }

    /// The reason from a `Bye`, if the node ended the session.
    pub fn end_reason(&self) -> Option<EndReason> {
        self.end_reason
    }

    /// True once the transport has closed.
    pub fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    /// Receive the next event. `Closed` is returned repeatedly after close.
    pub async fn recv_event(&mut self) -> Result<ClientEvent, ApiError> {
        if let Some(event) = self.pending.pop_front() {
            return Ok(event);
        }
        if let Some(reason) = self.closed {
            return Ok(ClientEvent::Closed { reason });
        }

        match self.transport.recv().await {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                self.handle_control_payload(&payload)
            }
            TransportEvent::Payload { channel, payload }
            | TransportEvent::Datagram { channel, payload } => {
                let packet = self.media_packet(channel, payload)?;
                Ok(ClientEvent::Media(packet))
            }
            TransportEvent::Error { error } => {
                self.server_error = Some(error.clone());
                Ok(ClientEvent::Error { error })
            }
            TransportEvent::Closed { reason } => {
                self.closed = Some(reason);
                Ok(ClientEvent::Closed { reason })
            }
        }
    }

    /// Send one input event on the reliable control channel.
    ///
    /// If the node has already reported an error, that error is returned
    /// instead of sending. A grant without `control` is rejected locally with
    /// `forbidden`, matching the node's view-only enforcement.
    pub async fn send_input(&mut self, event: &InputEvent) -> Result<(), ApiError> {
        if let Some(error) = &self.server_error {
            return Err(error.clone());
        }
        self.ensure_open()?;
        if let Some(session) = &self.session {
            if session.state.is_terminal() {
                return Err(error::disconnected(format!("{:?}", session.state)));
            }
            if !session.permissions.contains(Permission::Control) {
                return Err(ApiError::new(
                    codes::FORBIDDEN,
                    "The session grant is view-only and does not include control",
                )
                .with_details(json!({
                    "session_id": session.id,
                    "permissions": session.permissions,
                })));
            }
        }
        self.send_control(&ControlMessage::Input {
            event: event.clone(),
        })
        .await
    }

    /// Release every held key/button/contact. Idempotent and safe to call on
    /// a view-only session (in which case it does nothing).
    pub async fn release_all(&mut self) -> Result<(), ApiError> {
        if self.closed.is_some() {
            return Ok(());
        }
        if !self
            .session
            .as_ref()
            .is_some_and(|session| session.permissions.contains(Permission::Control))
        {
            return Ok(());
        }
        self.send_control(&ControlMessage::Input {
            event: InputEvent::ReleaseAll,
        })
        .await
    }

    /// Send a protocol ping. The matching `Pong` arrives through `recv_event`.
    pub async fn send_ping(&mut self, nonce: u64) -> Result<(), ApiError> {
        self.ensure_open()?;
        self.send_control(&ControlMessage::Ping { nonce }).await
    }

    /// Send `ReleaseAll`, send `Bye`, and close the QUIC connection.
    pub async fn close(&mut self) -> Result<(), ApiError> {
        if self.closed.is_some() {
            return Ok(());
        }
        let _ = self.release_all().await;
        let _ = self
            .send_control(&ControlMessage::Bye {
                reason: EndReason::ClientDisconnected,
            })
            .await;
        if !self.close_grace.is_zero() {
            // QUIC does not guarantee that stream data is delivered before a
            // CONNECTION_CLOSE. Give the queued `ReleaseAll` and `Bye` a chance
            // to flush so a node can release held input deterministically.
            tokio::time::sleep(self.close_grace).await;
        }
        let result = self.transport.close_with_reason(CloseReason::Normal).await;
        self.closed = Some(CloseReason::Normal);
        result
    }

    fn ensure_open(&self) -> Result<(), ApiError> {
        if let Some(reason) = self.closed {
            return Err(error::disconnected(reason.as_str()));
        }
        Ok(())
    }

    fn media_packet(
        &mut self,
        channel: ChannelId,
        payload: Bytes,
    ) -> Result<MediaPacket, ApiError> {
        let descriptor = self.channels.get(channel).ok_or_else(|| {
            error::protocol(format!(
                "The node sent data on unknown channel {}",
                channel.raw()
            ))
        })?;
        let packet = MediaPacket {
            channel,
            kind: descriptor.kind,
            track_id: descriptor.track_id.clone(),
            sequence: self.media_sequence,
            payload,
        };
        self.media_sequence = self.media_sequence.wrapping_add(1);
        Ok(packet)
    }

    async fn send_control(&self, message: &ControlMessage) -> Result<(), ApiError> {
        let payload = serde_json::to_vec(message).map_err(|error| {
            error::protocol(format!("Could not encode a control message: {error}"))
        })?;
        self.transport
            .send(ChannelId::CONTROL, Bytes::from(payload))
            .await
    }

    async fn send_raw_control(&self, value: &Value) -> Result<(), ApiError> {
        let payload = serde_json::to_vec(value).map_err(|error| {
            error::protocol(format!("Could not encode a control message: {error}"))
        })?;
        self.transport
            .send(ChannelId::CONTROL, Bytes::from(payload))
            .await
    }

    async fn await_hello_ack(&mut self) -> Result<Session, ApiError> {
        loop {
            match self.transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                    let value: Value = serde_json::from_slice(&payload).map_err(|error| {
                        error::protocol(format!("Invalid control JSON from the node: {error}"))
                    })?;
                    match control_kind(&value)? {
                        "hello_ack" => return parse_hello_ack(&value),
                        "error" => {
                            let api = parse_error(&value)?;
                            self.server_error = Some(api.clone());
                            return Err(api);
                        }
                        "event" => {
                            let event = parse_event(&value)?;
                            self.pending.push_back(ClientEvent::Session(event));
                        }
                        "pong" => {
                            if let Some(nonce) = parse_pong(&value) {
                                self.pending.push_back(ClientEvent::Pong { nonce });
                            }
                        }
                        "bye" => {
                            let reason = parse_bye(&value)?;
                            self.end_reason = Some(reason);
                            self.pending.push_back(ClientEvent::Bye { reason });
                        }
                        _ => {}
                    }
                }
                TransportEvent::Payload { channel, payload }
                | TransportEvent::Datagram { channel, payload } => {
                    let packet = self.media_packet(channel, payload)?;
                    self.pending.push_back(ClientEvent::Media(packet));
                }
                TransportEvent::Error { error } => {
                    self.server_error = Some(error.clone());
                    return Err(error);
                }
                TransportEvent::Closed { reason } => {
                    self.closed = Some(reason);
                    return Err(error::disconnected(reason.as_str()));
                }
            }
        }
    }

    fn handle_control_payload(&mut self, payload: &[u8]) -> Result<ClientEvent, ApiError> {
        let value: Value = serde_json::from_slice(payload).map_err(|error| {
            error::protocol(format!("Invalid control JSON from the node: {error}"))
        })?;
        match control_kind(&value)? {
            "hello_ack" => {
                let session = parse_hello_ack(&value)?;
                self.session = Some(session.clone());
                Ok(ClientEvent::SessionUpdated(session))
            }
            "event" => Ok(ClientEvent::Session(parse_event(&value)?)),
            "error" => {
                let error = parse_error(&value)?;
                self.server_error = Some(error.clone());
                Ok(ClientEvent::Error { error })
            }
            "pong" => match parse_pong(&value) {
                Some(nonce) => Ok(ClientEvent::Pong { nonce }),
                None => Ok(ClientEvent::UnknownControl {
                    kind: "pong".to_string(),
                }),
            },
            "bye" => {
                let reason = parse_bye(&value)?;
                self.end_reason = Some(reason);
                Ok(ClientEvent::Bye { reason })
            }
            other => Ok(ClientEvent::UnknownControl {
                kind: other.to_string(),
            }),
        }
    }
}

fn select_quic_transport(
    offer: &SessionOffer,
) -> Result<(TransportDescriptor, ChannelMap), ApiError> {
    let descriptor = offer
        .transports
        .iter()
        .find(|descriptor| {
            descriptor.mode == TransportMode::Quic
                && descriptor
                    .endpoint
                    .as_deref()
                    .is_some_and(|endpoint| !endpoint.trim().is_empty())
        })
        .ok_or_else(|| {
            ApiError::new(
                codes::TRANSPORT_UNAVAILABLE,
                "The session offer has no QUIC transport with an endpoint",
            )
            .with_remediation(
                "Use a node and client that negotiate the native ozma/1 QUIC transport.",
            )
        })?;
    let channels = if descriptor.channels.is_empty() {
        // Hand-written offers (for example a probe fixture) may omit the
        // channel list; the standard single-track layout is the node default.
        ChannelMap::default_session()
    } else {
        ChannelMap::from_descriptor(descriptor)?
    };
    channels.require(ChannelId::CONTROL)?;
    Ok((descriptor.clone(), channels))
}

async fn resolve_endpoint(endpoint: &str) -> Result<SocketAddr, ApiError> {
    let endpoint = endpoint.trim().trim_end_matches('/');
    let endpoint = endpoint
        .strip_prefix("quic://")
        .or_else(|| endpoint.strip_prefix("ozma://"))
        .or_else(|| endpoint.strip_prefix("https://"))
        .or_else(|| endpoint.strip_prefix("http://"))
        .unwrap_or(endpoint);
    let endpoint = endpoint.split('/').next().unwrap_or(endpoint);

    if let Ok(address) = endpoint.parse::<SocketAddr>() {
        return Ok(address);
    }
    let addresses = tokio::net::lookup_host(endpoint)
        .await
        .map_err(|error| {
            error::config(format!(
                "Could not resolve the node QUIC endpoint '{endpoint}': {error}"
            ))
            .with_remediation("Check that the offer endpoint is host:port.")
        })?
        .collect::<Vec<_>>();
    addresses.into_iter().next().ok_or_else(|| {
        error::config(format!(
            "The node QUIC endpoint '{endpoint}' resolved to no addresses"
        ))
    })
}

fn control_kind(value: &Value) -> Result<&str, ApiError> {
    value
        .get("type")
        .and_then(Value::as_str)
        .ok_or_else(|| error::protocol("A control message has no string 'type' field"))
}

fn control_data(value: &Value) -> &Value {
    value.get("data").unwrap_or(&Value::Null)
}

fn parse_hello_ack(value: &Value) -> Result<Session, ApiError> {
    let session = control_data(value).get("session").ok_or_else(|| {
        error::protocol("HelloAck has no 'session' object")
            .with_details(json!({ "message": value }))
    })?;
    serde_json::from_value(session.clone()).map_err(|error| {
        error::protocol(format!("HelloAck contains an invalid session: {error}"))
            .with_details(json!({ "session": session }))
    })
}

fn parse_event(value: &Value) -> Result<SessionEvent, ApiError> {
    let event = control_data(value).get("event").ok_or_else(|| {
        error::protocol("Event control message has no 'event' object")
            .with_details(json!({ "message": value }))
    })?;
    serde_json::from_value(event.clone()).map_err(|error| {
        error::protocol(format!("The node sent an invalid session event: {error}"))
            .with_details(json!({ "event": event }))
    })
}

fn parse_error(value: &Value) -> Result<ApiError, ApiError> {
    let error = control_data(value)
        .get("error")
        .or_else(|| value.get("error"))
        .ok_or_else(|| error::protocol("Error control message has no 'error' object"))?;
    serde_json::from_value(error.clone()).map_err(|parse| {
        error::protocol(format!("The node sent an invalid error object: {parse}"))
            .with_details(json!({ "error": error }))
    })
}

fn parse_pong(value: &Value) -> Option<u64> {
    let nonce = control_data(value).get("nonce")?;
    nonce.as_u64().or_else(|| nonce.as_str()?.parse().ok())
}

fn parse_bye(value: &Value) -> Result<EndReason, ApiError> {
    let reason = control_data(value).get("reason").ok_or_else(|| {
        error::protocol("Bye control message has no 'reason'")
            .with_details(json!({ "message": value }))
    })?;
    serde_json::from_value(reason.clone()).map_err(|error| {
        error::protocol(format!("The node sent an unknown end reason: {error}"))
            .with_details(json!({ "reason": reason }))
    })
}
