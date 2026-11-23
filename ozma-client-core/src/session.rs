// SPDX-License-Identifier: AGPL-3.0-only
//! Platform-neutral session driver.
//!
//! [`ClientDriver`] sits on top of a connected [`ChannelTransport`] and turns
//! the channel byte stream into typed events: control messages, reassembled
//! video access units, raw audio packets, and terminal close/error states. It
//! is the shared core of the native client and the browser client, so it has
//! no sockets, no threads, no timer, and no `Send` requirement.
//!
//! # Handshake contract
//!
//! The driver mirrors the node's `ozma-node/src/transport.rs::read_hello`:
//!
//! 1. The client sends exactly one [`ControlMessage::Hello`] as the first
//!    reliable control frame, carrying `offer.session_id`, `offer.token`, and
//!    optional [`DisplayCapabilities`].
//! 2. The node replies with [`ControlMessage::HelloAck`] (a control payload) or
//!    [`ControlMessage::Error`] and then closes.
//! 3. Video arrives as unreliable datagrams reassembled by [`VideoAssembler`];
//!    audio arrives as raw Opus packets on the audio channel; reliable channels
//!    (control, input, telemetry, bulk) arrive on the shared ordered stream.
//!
//! Channel ids are never hard-coded: they are resolved from the transport's
//! negotiated [`ChannelMap`] by [`ChannelKind`].
//!
//! # Timeouts
//!
//! This module deliberately owns no timer. [`ClientDriver::connect`] awaits the
//! `HelloAck` forever; the caller must bound it with its own runtime (for
//! example `tokio::time::timeout` natively, or an `AbortSignal`-backed future in
//! the browser). Transport adapters should also enforce handshake timeouts
//! internally where the platform provides one.

use std::collections::VecDeque;

use bytes::Bytes;
use ozma_protocol::scene::DisplayCapabilities;
use ozma_protocol::{
    ApiError, ChannelKind, ControlMessage, EndReason, InputEvent, Permission, Session,
    SessionOffer, TrackId, TransportDescriptor, TransportMode,
};
use ozma_transport::{ChannelId, ChannelMap, CloseReason, TransportEvent};

use crate::transport::ChannelTransport;
use crate::video::{VideoAssembler, VideoAssemblyError};

/// Client-side error codes shared with the native client.
///
/// These are stable strings, not protocol codes: a relayed node error keeps its
/// original `ozma_protocol::codes` value.
pub mod codes {
    /// The peer sent control data the driver could not parse.
    pub const CLIENT_PROTOCOL: &str = "client.protocol";
    /// The transport closed while an operation needed it.
    pub const CLIENT_DISCONNECTED: &str = "client.disconnected";
    /// A requested transport or feature is unavailable.
    pub const CLIENT_UNSUPPORTED: &str = "client.unsupported";
    /// Certificate loading, decoding, or pinning failed.
    pub const CLIENT_CERTIFICATE: &str = "client.certificate";
    /// Invalid client configuration, including a bad endpoint.
    pub const CLIENT_CONFIG: &str = "client.config";
}

/// One opaque media payload received from the node.
///
/// Video payloads are complete encoded access units (all fragments joined);
/// audio payloads are raw Opus packets. The driver never decodes media.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MediaPacket {
    /// Wire channel that carried the payload.
    pub channel: ChannelId,
    /// Logical kind from the negotiated channel map.
    pub kind: ChannelKind,
    /// Video track id, when the channel maps to a track.
    pub track_id: Option<TrackId>,
    /// Video access-unit sequence from the fragment header. Audio and other
    /// channels report their client-side arrival sequence here instead.
    pub sequence: u64,
    /// Monotonic client-side arrival counter (never a wire field).
    pub arrival_sequence: u64,
    /// True when the node marked this video access unit as a keyframe.
    pub keyframe: bool,
    /// Encoded bytes exactly as the node produced them.
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

    /// True for a video access unit.
    pub fn is_video(&self) -> bool {
        self.kind == ChannelKind::Video
    }
}

/// A typed event produced by [`ClientDriver::recv`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum ClientEvent {
    /// Video, audio, mic, telemetry, or bulk bytes from the node.
    Media(MediaPacket),
    /// A decoded control message from the reliable control channel.
    ///
    /// This includes `Event`, `SceneCommit`, `Pong`, `InputLatencyAck`, `Bye`,
    /// and a repeated `HelloAck`; it excludes `Error`, which is surfaced as
    /// [`ClientEvent::Error`] and remembered for later input calls.
    Control(ControlMessage),
    /// The transport is closed. Returned again on every later call.
    Closed(CloseReason),
    /// A non-terminal protocol or node error. `Error` control messages are
    /// also retained and returned by later input calls.
    Error(ApiError),
}

impl ClientEvent {
    /// True when this is a video access unit.
    pub fn is_video(&self) -> bool {
        matches!(self, ClientEvent::Media(packet) if packet.is_video())
    }
}

/// Reply fields from [`ControlMessage::InputLatencyAck`].
///
/// The driver forwards the node's raw timestamps; it does not compute a
/// latency and does not claim display/photon latency. Timing policy (clock
/// source, timeout, and which frame to wait for) belongs to the caller.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct InputLatencySample {
    /// Nonce echoed by the node.
    pub nonce: u64,
    /// Client timestamp supplied in the probe, echoed back.
    pub client_sent_micros: u64,
    /// Server wall-clock receive time, in microseconds since the Unix epoch.
    pub server_received_micros: u64,
    /// Highest encoded video frame sequence the node had sent when it received
    /// the probe.
    pub latest_frame_sequence: u64,
}

/// A connected session driven by a [`ChannelTransport`].
pub struct ClientDriver<T: ChannelTransport> {
    transport: T,
    offer: SessionOffer,
    channels: ChannelMap,
    control: ChannelId,
    session: Option<Session>,
    display: Option<ozma_protocol::scene::DisplayProfile>,
    pending: VecDeque<ClientEvent>,
    video: VideoAssembler,
    media_sequence: u64,
    latency_nonce: u64,
    server_error: Option<ApiError>,
    closed: Option<CloseReason>,
}

impl<T: ChannelTransport> std::fmt::Debug for ClientDriver<T> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ClientDriver")
            .field("mode", &self.transport.mode())
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

impl<T: ChannelTransport> ClientDriver<T> {
    /// Send `Hello`, wait for `HelloAck`, and return a connected driver.
    ///
    /// This future has no timeout of its own: wrap it in the caller's timer.
    /// A `ControlMessage::Error` during the handshake is returned as `Err`.
    pub async fn connect(transport: T, offer: &SessionOffer) -> Result<Self, ApiError> {
        Self::connect_with_display(transport, offer, None).await
    }

    /// Like [`ClientDriver::connect`] but advertises display capabilities.
    ///
    /// Providing [`DisplayCapabilities`] lets the node answer with a
    /// [`ozma_protocol::scene::DisplayProfile`] in `HelloAck`.
    pub async fn connect_with_display(
        transport: T,
        offer: &SessionOffer,
        display: Option<DisplayCapabilities>,
    ) -> Result<Self, ApiError> {
        let channels = transport.channel_map().clone();
        let control = channel_for_kind(&channels, ChannelKind::Control)?;

        let mut driver = Self {
            transport,
            offer: offer.clone(),
            channels,
            control,
            session: None,
            display: None,
            pending: VecDeque::new(),
            video: VideoAssembler::default(),
            media_sequence: 0,
            latency_nonce: 0,
            server_error: None,
            closed: None,
        };

        driver
            .send_control(ControlMessage::Hello {
                session_id: offer.session_id.clone(),
                token: offer.token.clone(),
                display,
            })
            .await?;

        let session = driver.await_hello_ack().await?;
        driver.session = Some(session);
        Ok(driver)
    }

    /// The session record from `HelloAck`, updated by later `HelloAck`s.
    pub fn session(&self) -> Option<&Session> {
        self.session.as_ref()
    }

    /// The display profile selected by the node, if any.
    pub fn display(&self) -> Option<&ozma_protocol::scene::DisplayProfile> {
        self.display.as_ref()
    }

    /// The offer used to connect.
    pub fn offer(&self) -> &SessionOffer {
        &self.offer
    }

    /// The negotiated channel map.
    pub fn channel_map(&self) -> &ChannelMap {
        &self.channels
    }

    /// Resolve the first channel of `kind` from the negotiated map.
    ///
    /// Returns `None` when the offer did not negotiate that kind. This is the
    /// driver-owned counterpart of [`channel_for_kind`], for callers that want
    /// to send on an adapter handle without borrowing the whole driver.
    pub fn channel_for(&self, kind: ChannelKind) -> Option<ChannelId> {
        self.channels
            .iter()
            .find(|descriptor| descriptor.kind == kind)
            .map(|descriptor| ChannelId::new(descriptor.id))
    }

    /// The reliable control channel selected during the handshake.
    ///
    /// Connecting requires a control channel, so this is always `Some` for a
    /// constructed driver; the `Option` shape matches [`ClientDriver::channel_for`].
    pub fn control_channel(&self) -> Option<ChannelId> {
        Some(self.control)
    }

    /// Borrow the underlying adapter.
    ///
    /// This lets a platform crate reach adapter-only features (for example the
    /// browser resource-stream reader) without widening the core contract.
    pub fn transport(&self) -> &T {
        &self.transport
    }

    /// Mutably borrow the underlying adapter.
    ///
    /// Adapter-only reads that need `&mut` (such as
    /// `BrowserTransport::next_resource`) cannot be selected against
    /// [`ClientDriver::recv`] on the same driver; surface such streams inside
    /// the adapter's event flow if both must be awaited together.
    pub fn transport_mut(&mut self) -> &mut T {
        &mut self.transport
    }

    /// The adapter kind, for diagnostics.
    pub fn mode(&self) -> TransportMode {
        self.transport.mode()
    }

    /// The last node-reported error, if any.
    pub fn server_error(&self) -> Option<&ApiError> {
        self.server_error.as_ref()
    }

    /// True once the transport has closed or a close was observed.
    pub fn is_closed(&self) -> bool {
        self.closed.is_some()
    }

    /// The next typed event. `Closed` is returned repeatedly after close.
    pub async fn recv(&mut self) -> ClientEvent {
        if let Some(event) = self.pending.pop_front() {
            return event;
        }
        if let Some(reason) = self.closed {
            return ClientEvent::Closed(reason);
        }
        self.receive_transport_event().await
    }

    /// Alias for [`ClientDriver::recv`].
    pub async fn next(&mut self) -> ClientEvent {
        self.recv().await
    }

    /// Read one event from the transport, bypassing the pending queue.
    ///
    /// Callers that must scan the stream (the latency probe) use this directly
    /// so deferred events cannot be replayed in a tight loop.
    async fn receive_transport_event(&mut self) -> ClientEvent {
        loop {
            match self.transport.recv().await {
                TransportEvent::Payload { channel, payload } => {
                    if channel == self.control {
                        match self.handle_control(&payload) {
                            Ok(Some(event)) => return event,
                            Ok(None) => continue,
                            Err(error) => return ClientEvent::Error(error),
                        }
                    }
                    match self.media_event(channel, payload) {
                        Ok(Some(event)) => return event,
                        Ok(None) => continue,
                        Err(error) => return ClientEvent::Error(error),
                    }
                }
                TransportEvent::Datagram { channel, payload } => {
                    match self.media_event(channel, payload) {
                        Ok(Some(event)) => return event,
                        Ok(None) => continue,
                        Err(error) => return ClientEvent::Error(error),
                    }
                }
                TransportEvent::Error { error } => {
                    self.server_error = Some(error.clone());
                    return ClientEvent::Error(error);
                }
                TransportEvent::Closed { reason } => {
                    self.closed = Some(reason);
                    return ClientEvent::Closed(reason);
                }
            }
        }
    }

    /// Send one input event as `ControlMessage::Input`.
    ///
    /// A remembered node error is returned first, and a view-only grant is
    /// rejected locally with `forbidden`, matching the node's enforcement.
    pub async fn send_input(&self, event: &InputEvent) -> Result<(), ApiError> {
        if let Some(error) = &self.server_error {
            return Err(error.clone());
        }
        self.ensure_control_permission()?;
        self.ensure_open()?;
        self.send_control(ControlMessage::Input {
            event: event.clone(),
        })
        .await
    }

    /// Send `ReleaseAll`. Idempotent and a no-op on a closed or view-only
    /// session.
    pub async fn send_release_all(&self) -> Result<(), ApiError> {
        if self.closed.is_some() {
            return Ok(());
        }
        if !self.has_control_permission() {
            return Ok(());
        }
        self.send_control(ControlMessage::Input {
            event: InputEvent::ReleaseAll,
        })
        .await
    }

    /// Send any control message on the reliable control channel.
    pub async fn send_control(&self, message: ControlMessage) -> Result<(), ApiError> {
        let payload = encode_control(&message)?;
        self.transport
            .send(self.control, Bytes::from(payload))
            .await
    }

    /// The next nonce for an input-latency probe.
    pub fn next_latency_nonce(&mut self) -> u64 {
        self.latency_nonce = self.latency_nonce.wrapping_add(1);
        self.latency_nonce
    }

    /// Send an `InputLatency` probe and wait for its `InputLatencyAck`.
    ///
    /// `nonce` and `client_sent_micros` come from the caller so the timing
    /// policy (clock and uniqueness) stays outside this crate. The returned
    /// sample contains only the node's raw reply fields; deriving a latency,
    /// waiting for a specific video frame, and enforcing a timeout are the
    /// caller's responsibility. The measurement never includes decode,
    /// presentation, or display scanout.
    ///
    /// Events received while waiting are queued and delivered by later
    /// [`ClientDriver::recv`] calls.
    pub async fn measure_input_latency(
        &mut self,
        event: &InputEvent,
        nonce: u64,
        client_sent_micros: u64,
    ) -> Result<InputLatencySample, ApiError> {
        if let Some(error) = &self.server_error {
            return Err(error.clone());
        }
        self.ensure_open()?;
        self.send_control(ControlMessage::InputLatency {
            event: event.clone(),
            client_sent_micros,
            nonce,
        })
        .await?;

        // Defer everything that is not the matching ack, but only read from the
        // transport so a deferred event is never replayed. The buffer is put
        // back in front of the pending queue before returning.
        let mut deferred = std::mem::take(&mut self.pending);
        let outcome = loop {
            match self.receive_transport_event().await {
                ClientEvent::Control(ControlMessage::InputLatencyAck {
                    nonce: ack_nonce,
                    client_sent_micros,
                    server_received_micros,
                    latest_frame_sequence,
                }) if ack_nonce == nonce => {
                    break Ok(InputLatencySample {
                        nonce,
                        client_sent_micros,
                        server_received_micros,
                        latest_frame_sequence,
                    });
                }
                ClientEvent::Error(error) => break Err(error),
                ClientEvent::Closed(reason) => break Err(disconnected(reason.as_str())),
                ClientEvent::Control(ControlMessage::Bye { reason }) => {
                    break Err(ApiError::new(
                        codes::CLIENT_DISCONNECTED,
                        format!("The node ended the session during the latency probe: {reason:?}"),
                    ))
                }
                other => deferred.push_back(other),
            }
        };
        for event in deferred.into_iter().rev() {
            self.pending.push_front(event);
        }
        outcome
    }

    /// Release held input, send `Bye`, and close the transport.
    ///
    /// The transport adapter owns any flush grace period: this method does not
    /// sleep.
    pub async fn close(mut self) -> Result<(), ApiError> {
        if self.closed.is_some() {
            return Ok(());
        }
        let _ = self.send_release_all().await;
        let _ = self
            .send_control(ControlMessage::Bye {
                reason: EndReason::ClientDisconnected,
            })
            .await;
        let result = self.transport.close_with_reason(CloseReason::Normal).await;
        self.closed = Some(CloseReason::Normal);
        result
    }

    async fn await_hello_ack(&mut self) -> Result<Session, ApiError> {
        loop {
            match self.transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == self.control => {
                    let message: ControlMessage =
                        serde_json::from_slice(&payload).map_err(|error| {
                            protocol_error(format!("Invalid control JSON from the node: {error}"))
                        })?;
                    match message {
                        ControlMessage::HelloAck { session, display } => {
                            self.display = display;
                            return Ok(session);
                        }
                        ControlMessage::Error { error } => {
                            self.server_error = Some(error.clone());
                            return Err(error);
                        }
                        other => self.pending.push_back(ClientEvent::Control(other)),
                    }
                }
                TransportEvent::Payload { channel, payload }
                | TransportEvent::Datagram { channel, payload } => {
                    if let Some(event) = self.media_event(channel, payload)? {
                        self.pending.push_back(event);
                    }
                }
                TransportEvent::Error { error } => {
                    self.server_error = Some(error.clone());
                    return Err(error);
                }
                TransportEvent::Closed { reason } => {
                    self.closed = Some(reason);
                    return Err(disconnected(reason.as_str()));
                }
            }
        }
    }

    fn handle_control(&mut self, payload: &[u8]) -> Result<Option<ClientEvent>, ApiError> {
        let message: ControlMessage = serde_json::from_slice(payload).map_err(|error| {
            protocol_error(format!("Invalid control JSON from the node: {error}"))
        })?;
        match message {
            ControlMessage::Error { error } => {
                self.server_error = Some(error.clone());
                Ok(Some(ClientEvent::Error(error)))
            }
            ControlMessage::HelloAck { session, display } => {
                self.session = Some(session.clone());
                if display.is_some() {
                    self.display = display.clone();
                }
                Ok(Some(ClientEvent::Control(ControlMessage::HelloAck {
                    session,
                    display,
                })))
            }
            other => Ok(Some(ClientEvent::Control(other))),
        }
    }

    fn media_event(
        &mut self,
        channel: ChannelId,
        payload: Bytes,
    ) -> Result<Option<ClientEvent>, ApiError> {
        let descriptor = self.channels.get(channel).ok_or_else(|| {
            protocol_error(format!(
                "The node sent data on unknown channel {}",
                channel.raw()
            ))
        })?;
        let kind = descriptor.kind;
        let track_id = descriptor.track_id.clone();

        if kind == ChannelKind::Video {
            let frame = match self.video.push(&payload) {
                Ok(frame) => frame,
                Err(error) => {
                    // Drop the other fragments of this broken access unit so a
                    // single corrupt datagram cannot poison later frames.
                    self.video.reset();
                    return Err(video_error(error));
                }
            };
            let Some(frame) = frame else {
                return Ok(None);
            };
            let packet = MediaPacket {
                channel,
                kind,
                track_id,
                sequence: frame.sequence,
                arrival_sequence: self.take_arrival_sequence(),
                keyframe: frame.keyframe,
                payload: Bytes::from(frame.data),
            };
            return Ok(Some(ClientEvent::Media(packet)));
        }

        let packet = MediaPacket {
            channel,
            kind,
            track_id,
            sequence: self.media_sequence,
            arrival_sequence: self.take_arrival_sequence(),
            keyframe: false,
            payload,
        };
        Ok(Some(ClientEvent::Media(packet)))
    }

    fn take_arrival_sequence(&mut self) -> u64 {
        let sequence = self.media_sequence;
        self.media_sequence = self.media_sequence.wrapping_add(1);
        sequence
    }

    fn has_control_permission(&self) -> bool {
        self.session
            .as_ref()
            .map(|session| session.permissions.contains(Permission::Control))
            .unwrap_or(true)
    }

    fn ensure_control_permission(&self) -> Result<(), ApiError> {
        if self.has_control_permission() {
            return Ok(());
        }
        Err(ApiError::new(
            ozma_protocol::codes::FORBIDDEN,
            "The session grant is view-only and does not include control",
        ))
    }

    fn ensure_open(&self) -> Result<(), ApiError> {
        if let Some(reason) = self.closed {
            return Err(disconnected(reason.as_str()));
        }
        Ok(())
    }
}

/// Serialize a control message for the reliable control channel.
///
/// Exposed so a platform facade can send a control frame through a cloned
/// transport handle without borrowing the driver. [`ClientDriver::send_control`]
/// uses the same encoding.
pub fn encode_control(message: &ControlMessage) -> Result<Vec<u8>, ApiError> {
    serde_json::to_vec(message)
        .map_err(|error| protocol_error(format!("Could not encode a control message: {error}")))
}

/// Find the first channel of `kind` in a negotiated map.
pub fn channel_for_kind(map: &ChannelMap, kind: ChannelKind) -> Result<ChannelId, ApiError> {
    map.iter()
        .find(|descriptor| descriptor.kind == kind)
        .map(|descriptor| ChannelId::new(descriptor.id))
        .ok_or_else(|| {
            ApiError::new(
                ozma_protocol::codes::TRANSPORT_UNAVAILABLE,
                format!("The negotiated channel map has no {kind:?} channel"),
            )
            .with_remediation("Check the transport descriptor returned in the session offer.")
        })
}

/// Every channel of `kind` in a negotiated map, in id order.
pub fn channels_for_kind(map: &ChannelMap, kind: ChannelKind) -> Vec<ChannelId> {
    map.iter()
        .filter(|descriptor| descriptor.kind == kind)
        .map(|descriptor| ChannelId::new(descriptor.id))
        .collect()
}

/// The WebTransport-shaped descriptor in an offer.
///
/// The node advertises WebTransport as [`TransportMode::Quic`] with an
/// `https://` endpoint and a certificate fingerprint. This is the descriptor a
/// browser client uses; the native client uses the same mode with a
/// `host:port` endpoint.
pub fn webtransport_descriptor(offer: &SessionOffer) -> Result<&TransportDescriptor, ApiError> {
    offer
        .transports
        .iter()
        .find(|descriptor| {
            descriptor.mode == TransportMode::Quic
                && descriptor
                    .endpoint
                    .as_deref()
                    .is_some_and(|endpoint| endpoint.trim().starts_with("https://"))
        })
        .ok_or_else(|| {
            ApiError::new(
                ozma_protocol::codes::TRANSPORT_UNAVAILABLE,
                "The session offer has no WebTransport (https://) endpoint",
            )
            .with_remediation(
                "Use a node build that serves the WebTransport listener and a browser over HTTPS.",
            )
        })
}

/// Build the channel map for a transport descriptor.
///
/// An empty channel list falls back to the standard single-track layout, which
/// matches the node default and the native client.
pub fn channel_map_for(descriptor: &TransportDescriptor) -> Result<ChannelMap, ApiError> {
    if descriptor.channels.is_empty() {
        return Ok(ChannelMap::default_session());
    }
    let map = ChannelMap::from_descriptor(descriptor)?;
    channel_for_kind(&map, ChannelKind::Control)?;
    Ok(map)
}

fn video_error(error: VideoAssemblyError) -> ApiError {
    protocol_error(format!("Invalid video fragment from the node: {error}"))
}

fn protocol_error(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::CLIENT_PROTOCOL, message)
}

fn disconnected(reason: impl std::fmt::Display) -> ApiError {
    ApiError::new(
        codes::CLIENT_DISCONNECTED,
        format!("The session transport is closed: {reason}"),
    )
    .with_remediation("Reconnect the session and retry.")
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use ozma_protocol::media::VideoFragmentHeader;
    use ozma_protocol::scene::{DisplayCapabilities, DisplayProfile, SCENE_VERSION};
    use ozma_protocol::{
        ClientId, Permission, PermissionSet, SessionEvent, SessionId, SessionSpec, SessionState,
    };
    use ozma_transport::offer::{SessionOfferBuilder, TransportDescriptorBuilder};

    use super::*;

    /// One endpoint of an in-process, unbounded mocked transport. Only the
    /// driver side implements [`ChannelTransport`]; tests drive the other side
    /// with the inherent helpers.
    struct MemoryTransport {
        map: ChannelMap,
        out: tokio::sync::mpsc::UnboundedSender<TransportEvent>,
        input: tokio::sync::mpsc::UnboundedReceiver<TransportEvent>,
        closed: Option<CloseReason>,
    }

    fn pair() -> (MemoryTransport, MemoryTransport) {
        pair_with_map(ChannelMap::default_session())
    }

    fn pair_with_map(map: ChannelMap) -> (MemoryTransport, MemoryTransport) {
        let (a_tx, b_rx) = tokio::sync::mpsc::unbounded_channel();
        let (b_tx, a_rx) = tokio::sync::mpsc::unbounded_channel();
        (
            MemoryTransport {
                map: map.clone(),
                out: a_tx,
                input: a_rx,
                closed: None,
            },
            MemoryTransport {
                map,
                out: b_tx,
                input: b_rx,
                closed: None,
            },
        )
    }

    impl MemoryTransport {
        async fn next_event(&mut self) -> TransportEvent {
            if let Some(reason) = self.closed {
                return TransportEvent::Closed { reason };
            }
            match self.input.recv().await {
                Some(event) => event,
                None => TransportEvent::Closed {
                    reason: CloseReason::PeerClosed,
                },
            }
        }

        async fn recv_control(&mut self) -> ControlMessage {
            loop {
                match self.next_event().await {
                    TransportEvent::Payload { channel, payload }
                        if channel == ChannelId::CONTROL =>
                    {
                        return serde_json::from_slice(&payload).expect("valid control JSON");
                    }
                    TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
                    other => panic!("unexpected transport event: {other:?}"),
                }
            }
        }

        async fn send_control(&self, message: ControlMessage) {
            let payload = serde_json::to_vec(&message).unwrap();
            self.out
                .send(TransportEvent::Payload {
                    channel: ChannelId::CONTROL,
                    payload: Bytes::from(payload),
                })
                .unwrap();
        }

        async fn send_datagram(&self, channel: ChannelId, payload: Vec<u8>) {
            self.out
                .send(TransportEvent::Datagram {
                    channel,
                    payload: Bytes::from(payload),
                })
                .unwrap();
        }

        async fn send_payload(&self, channel: ChannelId, payload: Vec<u8>) {
            self.out
                .send(TransportEvent::Payload {
                    channel,
                    payload: Bytes::from(payload),
                })
                .unwrap();
        }

        async fn close_peer(&mut self, reason: CloseReason) {
            let _ = self.out.send(TransportEvent::Closed { reason });
            self.closed = Some(reason);
        }
    }

    #[async_trait(?Send)]
    impl ChannelTransport for MemoryTransport {
        fn mode(&self) -> TransportMode {
            TransportMode::Loopback
        }

        fn channel_map(&self) -> &ChannelMap {
            &self.map
        }

        async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<(), ApiError> {
            let reliable = self.map.is_reliable(channel)?;
            self.map.check_payload(channel, payload.len())?;
            self.out
                .send(if reliable {
                    TransportEvent::Payload { channel, payload }
                } else {
                    TransportEvent::Datagram { channel, payload }
                })
                .map_err(|_| disconnected("the mock peer is gone"))?;
            Ok(())
        }

        async fn recv(&mut self) -> TransportEvent {
            MemoryTransport::next_event(self).await
        }

        async fn close_with_reason(&self, reason: CloseReason) -> Result<(), ApiError> {
            // The mock has no shared close state; peers observe a dropped
            // sender instead. Tests call the inherent helper to close a side.
            let _ = reason;
            Ok(())
        }
    }

    fn session() -> Session {
        let spec = SessionSpec {
            client_id: ClientId::new("client-test"),
            principal: "alice".into(),
            tracks: Vec::new(),
            audio: Default::default(),
            input: Default::default(),
            privacy: Default::default(),
            transport: Default::default(),
            ttl_seconds: 60,
        };
        let mut session = Session::new(
            SessionId::new("session-test"),
            spec,
            PermissionSet::from_iter([Permission::View, Permission::Control]),
            1,
        );
        session.state = SessionState::Streaming;
        session.started_at = Some(1);
        session.updated_at = 2;
        session
    }

    fn offer() -> SessionOffer {
        SessionOfferBuilder::new("session-test")
            .token("token-test")
            .expires_at(1_700_000_000_000)
            .transport(
                TransportDescriptorBuilder::quic("127.0.0.1:4433")
                    .certificate_fingerprint("00")
                    .build(),
            )
            .build()
            .unwrap()
    }

    fn fragment(keyframe: bool, sequence: u64, index: u16, count: u16, payload: &[u8]) -> Vec<u8> {
        VideoFragmentHeader::new(keyframe, 0, sequence, index, count).encode_into(payload)
    }

    #[tokio::test]
    async fn handshake_sends_hello_and_buffers_early_events() {
        let (client, mut server) = pair();
        let offer = offer();
        let event = SessionEvent::StateChanged {
            session_id: SessionId::new("session-test"),
            state: SessionState::Streaming,
        };

        let (driver, ()) = tokio::join!(ClientDriver::connect(client, &offer), async {
            match server.recv_control().await {
                ControlMessage::Hello {
                    session_id,
                    token,
                    display,
                } => {
                    assert_eq!(session_id, SessionId::new("session-test"));
                    assert_eq!(token, "token-test");
                    assert!(display.is_none());
                }
                other => panic!("expected Hello, got {other:?}"),
            }
            // An event that arrives before HelloAck must not be lost.
            server
                .send_control(ControlMessage::Event {
                    event: event.clone(),
                })
                .await;
            server
                .send_control(ControlMessage::HelloAck {
                    session: session(),
                    display: None,
                })
                .await;
        });

        let mut driver = driver.unwrap();
        assert_eq!(driver.session().unwrap().id, SessionId::new("session-test"));
        assert_eq!(
            driver.recv().await,
            ClientEvent::Control(ControlMessage::Event { event })
        );
    }

    #[tokio::test]
    async fn handshake_reports_a_node_error_as_err() {
        let (client, mut server) = pair();
        let offer = offer();
        let error = ApiError::new(
            ozma_protocol::codes::FORBIDDEN,
            "token is for another session",
        );

        let (result, ()) = tokio::join!(ClientDriver::connect(client, &offer), async {
            let _ = server.recv_control().await;
            server
                .send_control(ControlMessage::Error {
                    error: error.clone(),
                })
                .await;
        });

        assert_eq!(result.unwrap_err(), error);
    }

    #[tokio::test]
    async fn handshake_reports_a_closed_transport_as_err() {
        let (client, mut server) = pair();
        let offer = offer();

        let (result, ()) = tokio::join!(ClientDriver::connect(client, &offer), async {
            let _ = server.recv_control().await;
            server.close_peer(CloseReason::PeerClosed).await;
        });

        let error = result.unwrap_err();
        assert_eq!(error.code, codes::CLIENT_DISCONNECTED);
    }

    #[tokio::test]
    async fn advertises_display_capabilities_and_records_the_profile() {
        let (client, mut server) = pair();
        let offer = offer();
        let capabilities = DisplayCapabilities {
            scene_versions: vec![SCENE_VERSION],
        };

        let (driver, ()) = tokio::join!(
            ClientDriver::connect_with_display(client, &offer, Some(capabilities.clone())),
            async {
                let ControlMessage::Hello { display, .. } = server.recv_control().await else {
                    panic!("expected Hello");
                };
                assert_eq!(display, Some(capabilities));
                server
                    .send_control(ControlMessage::HelloAck {
                        session: session(),
                        display: Some(DisplayProfile::Video),
                    })
                    .await;
            }
        );

        let driver = driver.unwrap();
        assert_eq!(driver.display(), Some(&DisplayProfile::Video));
    }

    async fn connected() -> (ClientDriver<MemoryTransport>, MemoryTransport) {
        let (client, mut server) = pair();
        let offer = offer();
        let (driver, ()) = tokio::join!(ClientDriver::connect(client, &offer), async {
            let _ = server.recv_control().await;
            server
                .send_control(ControlMessage::HelloAck {
                    session: session(),
                    display: None,
                })
                .await;
        });
        (driver.unwrap(), server)
    }

    #[tokio::test]
    async fn reassembles_fragmented_video_and_reports_keyframe_and_sequence() {
        let (mut driver, server) = connected().await;

        server
            .send_datagram(ChannelId::VIDEO, fragment(true, 7, 1, 2, b"world"))
            .await;
        server
            .send_datagram(ChannelId::VIDEO, fragment(true, 7, 0, 2, b"hello "))
            .await;

        let ClientEvent::Media(packet) = driver.recv().await else {
            panic!("expected a media packet");
        };
        assert_eq!(packet.kind, ChannelKind::Video);
        assert_eq!(packet.channel, ChannelId::VIDEO);
        assert_eq!(packet.sequence, 7);
        assert!(packet.keyframe);
        assert_eq!(packet.payload.as_ref(), b"hello world");
        assert_eq!(packet.track_id, Some(TrackId::new("video0")));
        assert_eq!(packet.arrival_sequence, 0);
    }

    #[tokio::test]
    async fn audio_datagrams_are_media_packets_too() {
        let (mut driver, server) = connected().await;
        server
            .send_datagram(ChannelId::AUDIO, b"opus".to_vec())
            .await;

        let ClientEvent::Media(packet) = driver.recv().await else {
            panic!("expected a media packet");
        };
        assert_eq!(packet.kind, ChannelKind::Audio);
        assert!(!packet.keyframe);
        assert_eq!(packet.payload.as_ref(), b"opus");
    }

    #[tokio::test]
    async fn control_events_are_delivered_and_errors_are_remembered() {
        let (mut driver, server) = connected().await;
        let event = SessionEvent::Ended {
            session_id: SessionId::new("session-test"),
            reason: EndReason::ClientDisconnected,
        };
        server
            .send_control(ControlMessage::Event {
                event: event.clone(),
            })
            .await;
        let error = ApiError::new(ozma_protocol::codes::FORBIDDEN, "not the controller");
        server
            .send_control(ControlMessage::Error {
                error: error.clone(),
            })
            .await;

        assert_eq!(
            driver.recv().await,
            ClientEvent::Control(ControlMessage::Event { event })
        );
        assert_eq!(driver.recv().await, ClientEvent::Error(error.clone()));
        assert_eq!(driver.server_error(), Some(&error));
        // A remembered error is returned by later input calls.
        assert_eq!(
            driver
                .send_input(&InputEvent::ReleaseAll)
                .await
                .unwrap_err(),
            error
        );
    }

    #[tokio::test]
    async fn send_input_and_release_all_use_the_control_channel() {
        let (driver, mut server) = connected().await;
        let event = InputEvent::Key {
            code: 0x04,
            down: true,
            modifiers: Default::default(),
        };

        driver.send_input(&event).await.unwrap();
        assert_eq!(
            server.recv_control().await,
            ControlMessage::Input {
                event: event.clone()
            }
        );

        driver.send_release_all().await.unwrap();
        assert_eq!(
            server.recv_control().await,
            ControlMessage::Input {
                event: InputEvent::ReleaseAll
            }
        );
    }

    #[tokio::test]
    async fn view_only_sessions_reject_input_locally() {
        let (client, mut server) = pair();
        let offer = offer();
        let mut view_only = session();
        view_only.permissions = PermissionSet::from_iter([Permission::View]);

        let (driver, ()) = tokio::join!(ClientDriver::connect(client, &offer), async {
            let _ = server.recv_control().await;
            server
                .send_control(ControlMessage::HelloAck {
                    session: view_only,
                    display: None,
                })
                .await;
        });
        let driver = driver.unwrap();

        let error = driver
            .send_input(&InputEvent::ReleaseAll)
            .await
            .unwrap_err();
        assert_eq!(error.code, ozma_protocol::codes::FORBIDDEN);
        // ReleaseAll remains a harmless no-op rather than an error.
        assert!(driver.send_release_all().await.is_ok());
    }

    #[tokio::test]
    async fn close_is_propagated_and_sticky() {
        let (mut driver, mut server) = connected().await;
        server.close_peer(CloseReason::Normal).await;

        assert_eq!(
            driver.recv().await,
            ClientEvent::Closed(CloseReason::Normal)
        );
        assert!(driver.is_closed());
        assert_eq!(
            driver.recv().await,
            ClientEvent::Closed(CloseReason::Normal)
        );
    }

    #[tokio::test]
    async fn stream_and_datagram_order_is_preserved() {
        let (mut driver, server) = connected().await;
        server
            .send_payload(ChannelId::TELEMETRY, b"first".to_vec())
            .await;
        server
            .send_datagram(ChannelId::VIDEO, fragment(false, 1, 0, 1, b"frame"))
            .await;
        server.send_payload(ChannelId::BULK, b"last".to_vec()).await;

        let first = driver.recv().await;
        let second = driver.recv().await;
        let third = driver.recv().await;

        assert!(matches!(
            first,
            ClientEvent::Media(MediaPacket {
                kind: ChannelKind::Telemetry,
                ..
            })
        ));
        assert!(matches!(
            second,
            ClientEvent::Media(MediaPacket {
                kind: ChannelKind::Video,
                ..
            })
        ));
        assert!(matches!(
            third,
            ClientEvent::Media(MediaPacket {
                kind: ChannelKind::Bulk,
                ..
            })
        ));
    }

    #[tokio::test]
    async fn latency_probe_sends_the_protocol_message_and_matches_the_nonce() {
        let (mut driver, mut server) = connected().await;

        let (sample, ()) = tokio::join!(
            driver.measure_input_latency(&InputEvent::ReleaseAll, 5, 1_000),
            async {
                let ControlMessage::InputLatency {
                    event,
                    client_sent_micros,
                    nonce,
                } = server.recv_control().await
                else {
                    panic!("expected InputLatency");
                };
                assert_eq!(event, InputEvent::ReleaseAll);
                assert_eq!(client_sent_micros, 1_000);
                assert_eq!(nonce, 5);
                // An ack for another nonce must be ignored.
                server
                    .send_control(ControlMessage::InputLatencyAck {
                        nonce: 6,
                        client_sent_micros,
                        server_received_micros: 2_000,
                        latest_frame_sequence: 1,
                    })
                    .await;
                server
                    .send_control(ControlMessage::InputLatencyAck {
                        nonce,
                        client_sent_micros,
                        server_received_micros: 2_000,
                        latest_frame_sequence: 42,
                    })
                    .await;
            }
        );

        let sample = sample.unwrap();
        assert_eq!(sample.nonce, 5);
        assert_eq!(sample.server_received_micros, 2_000);
        assert_eq!(sample.latest_frame_sequence, 42);
        // The unmatched ack is queued for the next recv.
        assert!(matches!(
            driver.recv().await,
            ClientEvent::Control(ControlMessage::InputLatencyAck { nonce: 6, .. })
        ));
    }

    #[tokio::test]
    async fn close_sends_release_all_and_bye_then_closes() {
        let (driver, mut server) = connected().await;

        driver.close().await.unwrap();

        assert_eq!(
            server.recv_control().await,
            ControlMessage::Input {
                event: InputEvent::ReleaseAll
            }
        );
        assert_eq!(
            server.recv_control().await,
            ControlMessage::Bye {
                reason: EndReason::ClientDisconnected
            }
        );
    }

    #[test]
    fn channel_lookup_uses_the_negotiated_map() {
        let map = ChannelMap::default_session();
        assert_eq!(
            channel_for_kind(&map, ChannelKind::Control).unwrap(),
            ChannelId::CONTROL
        );
        assert_eq!(
            channel_for_kind(&map, ChannelKind::Video).unwrap(),
            ChannelId::VIDEO
        );
        assert_eq!(
            channels_for_kind(&map, ChannelKind::Video),
            vec![ChannelId::VIDEO]
        );

        let descriptor = TransportDescriptorBuilder::quic("127.0.0.1:1").build();
        assert_eq!(
            channel_map_for(&descriptor).unwrap(),
            ChannelMap::default_session()
        );
    }

    #[tokio::test]
    async fn driver_channel_lookups_use_the_negotiated_map() {
        let (driver, _server) = connected().await;
        assert_eq!(
            driver.channel_for(ChannelKind::Control),
            Some(ChannelId::CONTROL)
        );
        assert_eq!(
            driver.channel_for(ChannelKind::Input),
            Some(ChannelId::INPUT)
        );
        assert_eq!(
            driver.channel_for(ChannelKind::Video),
            Some(ChannelId::VIDEO)
        );
        assert_eq!(driver.channel_for(ChannelKind::Mic), Some(ChannelId::MIC));
        assert!(driver.channel_for(ChannelKind::Camera).is_some());
        assert_eq!(driver.control_channel(), Some(ChannelId::CONTROL));
    }

    #[tokio::test]
    async fn driver_channel_lookup_returns_none_for_unnegotiated_kinds() {
        let map = ChannelMap::for_tracks(&[TrackId::new("video0")], false, false).unwrap();
        let (client, mut server) = pair_with_map(map);
        let offer = offer();
        let (driver, ()) = tokio::join!(ClientDriver::connect(client, &offer), async {
            let _ = server.recv_control().await;
            server
                .send_control(ControlMessage::HelloAck {
                    session: session(),
                    display: None,
                })
                .await;
        });
        let driver = driver.unwrap();
        assert_eq!(driver.channel_for(ChannelKind::Audio), None);
        assert_eq!(driver.channel_for(ChannelKind::Mic), None);
        assert!(driver.channel_for(ChannelKind::Telemetry).is_some());
    }

    #[test]
    fn encode_control_matches_send_control_wire_format() {
        let message = ControlMessage::Ping { nonce: 7 };
        let encoded = encode_control(&message).unwrap();
        let decoded: ControlMessage = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, message);
    }

    #[test]
    fn webtransport_offers_are_selected_by_https_endpoint() {
        let offer = SessionOfferBuilder::new("s1")
            .token("t")
            .expires_at(1)
            .transport(
                TransportDescriptorBuilder::new(TransportMode::Quic)
                    .endpoint("https://node.example:4433/session")
                    .secure(true)
                    .build(),
            )
            .build()
            .unwrap();
        let descriptor = webtransport_descriptor(&offer).unwrap();
        assert_eq!(
            descriptor.endpoint.as_deref(),
            Some("https://node.example:4433/session")
        );
    }
}
