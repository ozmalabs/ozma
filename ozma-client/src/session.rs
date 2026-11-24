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
use std::time::{Duration, Instant};

use bytes::Bytes;
use ozma_display_protocol::{
    scene_convert, CodecError, DisplayEvent, DisplayMessage, DisplaySession,
};
use ozma_protocol::scene::{
    CommitStatus, DisplayCapabilities, DisplayProfile, SceneReceiver, SCENE_VERSION,
};
use ozma_protocol::{
    codes, ApiError, ChannelKind, ControlMessage, EndReason, InputEvent, Permission, PermissionSet,
    Session, SessionEvent, SessionOffer, SessionState, TrackId, TransportDescriptor, TransportMode,
    WireFormat,
};
use ozma_transport::{
    ChannelId, ChannelMap, CloseReason, QuicClient, QuicConfig, QuicTransport, SessionTransport,
    TransportEvent,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::capture::{
    mic_channel_missing, ClientCapture, ClientCaptureOptions, ClientCaptureState,
};
use crate::cert;
use crate::error;
use crate::node_api::NodeApiClient;
use crate::video::VideoAssembler;

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
    /// Enable scene negotiation with this encoded-image cache budget.
    pub scene_cache_bytes: Option<usize>,
}

impl Default for NativeClientConfig {
    fn default() -> Self {
        Self {
            server_name: "localhost".to_string(),
            handshake_timeout: Duration::from_secs(5),
            hello_timeout: Duration::from_secs(10),
            idle_timeout: Duration::from_secs(30),
            close_grace: Duration::from_millis(50),
            scene_cache_bytes: None,
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

/// Result of [`NativeSessionClient::measure_input_latency`].
///
/// The timings cover the client's send call through arrival of the first
/// complete **encoded** access unit at or after the node's ack sequence. They
/// deliberately do not include decode, compositor presentation, display
/// scanout, or monitor latency, so they are not a photon-latency SLO.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct InputLatencyReport {
    /// Nonce echoed by the matching ack.
    pub nonce: u64,
    /// True when the node accepted and applied the probe event. An unauthorized
    /// probe is reported as an error instead of a successful report.
    pub event_accepted: bool,
    /// Local round trip from sending `InputLatency` to receiving its ack.
    pub ack_rtt_ms: f64,
    /// Ack's server-receive time to local frame arrival, when both wall clocks
    /// are close enough for the difference to be meaningful. `None` when the
    /// clocks are skewed or the timestamps are not ordered.
    pub server_to_frame_ms: Option<f64>,
    /// Local input send to local arrival of a complete encoded access unit.
    /// This is the primary measurement and is independent of clock sync.
    pub input_to_frame_ms: f64,
    /// Highest encoded frame sequence the node had sent when it received the
    /// probe.
    pub latest_frame_sequence: u64,
    /// Sequence of the first complete frame at or above `latest_frame_sequence`.
    pub frame_sequence: u64,
    /// Raw client send timestamp from the ack, for diagnostics.
    pub client_sent_micros: u64,
    /// Raw server receive timestamp from the ack, for diagnostics.
    pub server_received_micros: u64,
}

/// An event received from a native session.
///
/// The `Eq` bound is not derived because Display events embed generated
/// protobuf message types, which only implement `PartialEq`.
#[derive(Clone, Debug, PartialEq)]
pub enum ClientEvent {
    /// Video, audio, mic, telemetry, or bulk bytes from the node.
    Media(MediaPacket),
    /// A session lifecycle event from the reliable control channel.
    Session(SessionEvent),
    /// The server sent another `HelloAck` (normally only once, during connect).
    SessionUpdated(Session),
    /// The current overlay revision is ready for presentation.
    ScenePresented { revision: u64 },
    /// A scene commit arrived without changing the current presentation.
    ScenePending,
    /// Reply to a client `Ping`.
    Pong { nonce: u64 },
    /// Reply to a client `InputLatency` probe.
    InputLatencyAck {
        nonce: u64,
        client_sent_micros: u64,
        server_received_micros: u64,
        latest_frame_sequence: u64,
    },
    /// A non-terminal error from the node. The same error is retained and
    /// returned by later input calls, which is how view-only/control denial is
    /// surfaced.
    Error { error: ApiError },
    /// The node ended the session.
    Bye { reason: EndReason },
    /// A control message type this client does not know. The session remains
    /// usable, which keeps the client forward-compatible with newer nodes.
    UnknownControl { kind: String },
    /// A decoded Display-channel message was applied to the session state
    /// machine or forwarded to the application.
    ///
    /// Scene snapshots that feed the compositor still produce
    /// [`ClientEvent::ScenePresented`] and [`ClientEvent::ScenePending`];
    /// definition, data, action, and interaction messages appear here.
    Display(DisplayEvent),
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
    display_channel: Option<ChannelId>,
    wire_format: WireFormat,
    descriptor: TransportDescriptor,
    offer: SessionOffer,
    session: Option<Session>,
    pending: VecDeque<ClientEvent>,
    media_sequence: u64,
    latency_nonce: u64,
    server_error: Option<ApiError>,
    end_reason: Option<EndReason>,
    closed: Option<CloseReason>,
    close_grace: Duration,
    scene: Option<SceneReceiver>,
    display: Option<DisplaySession>,
    resources: Option<tokio::sync::mpsc::Receiver<Result<Bytes, ApiError>>>,
    capture: Option<ClientCapture>,
    captured: Option<ClientCaptureState>,
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

        let display_channel = channels.channel_for_kind(ChannelKind::Display);
        let mut client = Self {
            transport,
            channels,
            display_channel,
            wire_format: WireFormat::Json,
            descriptor,
            offer: offer.clone(),
            session: None,
            pending: VecDeque::new(),
            media_sequence: 0,
            latency_nonce: 0,
            server_error: None,
            end_reason: None,
            closed: None,
            close_grace: config.close_grace,
            scene: None,
            display: display_channel.map(|_| DisplaySession::default()),
            resources: None,
            capture: None,
            captured: None,
        };

        let display = config.scene_cache_bytes.map(|_| DisplayCapabilities {
            scene_versions: vec![SCENE_VERSION],
            ..Default::default()
        });
        client
            .send_control(&ControlMessage::Hello {
                session_id: offer.session_id.clone(),
                token: offer.token.clone(),
                wire_formats: vec![WireFormat::Protobuf, WireFormat::Json],
                display,
            })
            .await?;

        let session = tokio::time::timeout(
            config.hello_timeout,
            client.await_hello_ack(config.scene_cache_bytes),
        )
        .await
        .map_err(|_| error::timeout("waiting for the session HelloAck", config.hello_timeout))??;
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

    /// Current scene and encoded image cache for a compositor.
    pub fn scene(&self) -> Option<&SceneReceiver> {
        self.scene.as_ref()
    }

    /// The Display-channel state machine, when the transport negotiated a
    /// reliable `Display` channel.
    ///
    /// This caches the current definition, decoded field values, and the latest
    /// scene revision. Events that change it are also delivered through
    /// [`ClientEvent::Display`].
    pub fn display_session(&self) -> Option<&DisplaySession> {
        self.display.as_ref()
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

    /// The `Display` channel selected by the negotiated map, if one exists.
    ///
    /// The node may omit the channel for transports built before display
    /// traffic existed, in which case scene commits stay on the control channel.
    pub fn display_channel(&self) -> Option<ChannelId> {
        self.display_channel
    }

    /// The wire format selected by the node in `HelloAck`.
    ///
    /// Defaults to [`WireFormat::Json`] for older nodes that omit the field.
    pub fn wire_format(&self) -> WireFormat {
        self.wire_format
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

        let event = loop {
            let Some(resources) = self.resources.as_mut() else {
                break self.transport.recv().await;
            };
            tokio::select! {
                event = self.transport.recv() => break event,
                resource = resources.recv() => {
                    match resource {
                        Some(Ok(bytes)) => {
                            let scene = self.scene.as_mut().expect("resource reader requires a scene");
                            if scene.receive_resource(&bytes).map_err(|error| {
                                error::protocol(format!("Scene resource rejected: {error:?}"))
                            })? {
                                return Ok(ClientEvent::ScenePresented { revision: scene.presented().revision });
                            }
                        }
                        Some(Err(error)) => {
                            self.resources = None;
                            return Err(error);
                        }
                        None => self.resources = None,
                    }
                }
            }
        };
        match event {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                self.handle_control_payload(&payload)
            }
            TransportEvent::Payload { channel, payload }
                if Some(channel) == self.display_channel =>
            {
                self.handle_display_payload(&payload)
            }
            TransportEvent::Payload { channel, payload } => {
                let packet = self.media_packet(channel, payload)?;
                Ok(ClientEvent::Media(packet))
            }
            TransportEvent::Datagram { channel, payload } => {
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

    /// Send one decoded Display message on the negotiated reliable `Display`
    /// channel.
    ///
    /// This is the device-to-node direction for `ActionInvoke` and
    /// `InteractionEvent`; it can carry any [`DisplayMessage`] variant. The
    /// message is encoded with the stable one-byte tag framing used by
    /// [`DisplayMessage::encode`].
    ///
    /// # Errors
    ///
    /// Returns `client.unsupported` when the negotiated channel map has no
    /// `Display` channel, `client.disconnected` when the session is closed or
    /// terminal, and `client.protocol` when the message cannot be encoded.
    pub async fn send_display_message(&mut self, message: DisplayMessage) -> Result<(), ApiError> {
        self.ensure_open()?;
        if let Some(session) = &self.session {
            if session.state.is_terminal() {
                return Err(error::disconnected(format!("{:?}", session.state)));
            }
        }

        let channel = self.display_channel.ok_or_else(|| {
            ApiError::new(
                error::codes::CLIENT_UNSUPPORTED,
                "The negotiated session has no reliable Display channel",
            )
            .with_remediation("Connect to a node that advertises the Display channel, then retry.")
        })?;

        let frame = message.encode().map_err(|error| {
            error::protocol(format!("Could not encode a Display message: {error}"))
        })?;
        self.transport.send(channel, Bytes::from(frame)).await
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

    /// Measure send-to-client-visible-encoded-frame latency for one input event.
    ///
    /// The event is sent as `InputLatency`, which the node authorizes and
    /// applies through the same session manager and input sink as ordinary
    /// input. This method then waits for the matching `InputLatencyAck`, and
    /// reassembles video until the first complete access unit with a sequence
    /// at or above the ack's `latest_frame_sequence` arrives.
    ///
    /// `input_to_frame_ms` measures client send to **encoded frame arrival**.
    /// It does not include decode, presentation, display scanout, or panel
    /// latency, so it must not be reported as input-to-photon latency or used
    /// to claim a display-latency SLO. `server_to_frame_ms` additionally
    /// depends on server/client wall clocks being close and is `None` when they
    /// are not.
    ///
    /// Use a harmless event such as [`InputEvent::ReleaseAll`] for a probe. An
    /// unauthorized event returns the node's structured error instead of a
    /// report.
    pub async fn measure_input_latency(
        &mut self,
        event: &InputEvent,
        timeout: Duration,
    ) -> Result<InputLatencyReport, ApiError> {
        if let Some(error) = &self.server_error {
            return Err(error.clone());
        }
        self.ensure_open()?;
        if let Some(session) = &self.session {
            if session.state.is_terminal() {
                return Err(error::disconnected(format!("{:?}", session.state)));
            }
        }

        self.latency_nonce = self.latency_nonce.wrapping_add(1);
        let nonce = self.latency_nonce;
        let started = Instant::now();
        let client_sent_micros = now_micros();
        self.send_control(&ControlMessage::InputLatency {
            event: event.clone(),
            client_sent_micros,
            nonce,
        })
        .await?;

        let mut assembler = VideoAssembler::default();
        let mut ack: Option<AckSample> = None;
        let mut last_completed: Option<CompletedFrame> = None;

        let report = tokio::time::timeout(timeout, async {
            loop {
                match self.recv_event().await? {
                    ClientEvent::Media(packet) if packet.kind == ChannelKind::Video => {
                        let frame =
                            assembler
                                .push_or_complete(&packet.payload)
                                .map_err(|error| {
                                    ApiError::new(
                                        error::codes::CLIENT_PROTOCOL,
                                        format!(
                                        "invalid video fragment during the latency probe: {error}"
                                    ),
                                    )
                                })?;
                        let Some(frame) = frame else { continue };
                        let at = Instant::now();
                        let at_micros = now_micros();
                        if let Some(sample) = ack.as_ref() {
                            if frame.sequence >= sample.latest_frame_sequence {
                                return Ok(latency_report(
                                    nonce,
                                    started,
                                    sample,
                                    frame.sequence,
                                    at,
                                    at_micros,
                                ));
                            }
                        }
                        last_completed = Some(CompletedFrame {
                            sequence: frame.sequence,
                            at,
                            at_micros,
                        });
                    }
                    ClientEvent::InputLatencyAck {
                        nonce: ack_nonce,
                        client_sent_micros,
                        server_received_micros,
                        latest_frame_sequence,
                    } if ack_nonce == nonce => {
                        let sample = AckSample {
                            client_sent_micros,
                            server_received_micros,
                            latest_frame_sequence,
                            received: Instant::now(),
                        };
                        if let Some(frame) = last_completed.as_ref() {
                            if frame.sequence >= latest_frame_sequence {
                                return Ok(latency_report(
                                    nonce,
                                    started,
                                    &sample,
                                    frame.sequence,
                                    frame.at,
                                    frame.at_micros,
                                ));
                            }
                        }
                        ack = Some(sample);
                    }
                    ClientEvent::Error { error } => return Err(error),
                    ClientEvent::Closed { reason } => {
                        return Err(error::disconnected(reason.as_str()))
                    }
                    ClientEvent::Bye { reason } => {
                        return Err(ApiError::new(
                            error::codes::CLIENT_DISCONNECTED,
                            format!(
                            "The node ended the session while measuring input latency ({reason:?})"
                        ),
                        ))
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(|_| {
            ApiError::new(
                error::codes::CLIENT_TIMEOUT,
                format!(
                    "Timed out measuring input latency after {} ms",
                    timeout.as_millis()
                ),
            )
            .with_details(json!({ "nonce": nonce }))
            .with_remediation(
                "Check that the session is streaming and that the node supports InputLatency.",
            )
            .retryable()
        })?;

        report
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

    /// Start publishing this client's camera (and optionally microphone).
    ///
    /// `CaptureStart` is sent before the media pipeline starts so the node has
    /// created its decode workers. A negotiated `CaptureState` with the chosen
    /// codec and size follows, or an error state when startup fails.
    ///
    /// [`ClientCaptureOptions::camera`] is always opened; `None` means the first
    /// available capture source. Audio is enabled only when
    /// [`ClientCaptureOptions::mic`] is `Some`. A microphone request without a
    /// negotiated `Mic` channel returns `client.config` and starts nothing.
    pub async fn start_capture(&mut self, options: ClientCaptureOptions) -> Result<(), ApiError> {
        self.start_capture_with_media(options, None, None).await
    }

    /// Start capture with injected media components.
    ///
    /// Passing `video`/`audio` as `None` uses the real capture path for that
    /// half only when both are `None`; otherwise a `None` half is disabled.
    /// This is the integration-test seam and is not part of the stable API.
    #[doc(hidden)]
    pub async fn start_capture_with_media(
        &mut self,
        options: ClientCaptureOptions,
        video: Option<(
            Box<dyn ozma_media::CaptureSource>,
            Box<dyn ozma_media::Encoder>,
        )>,
        audio: Option<(
            Box<dyn ozma_audio::AudioSource>,
            Box<dyn ozma_audio::AudioEncoder>,
        )>,
    ) -> Result<(), ApiError> {
        self.ensure_open()?;
        if let Some(error) = &self.server_error {
            return Err(error.clone());
        }
        if let Some(session) = &self.session {
            if session.state.is_terminal() {
                return Err(error::disconnected(format!("{:?}", session.state)));
            }
        }
        self.stop_capture().await?;

        let use_real_media = video.is_none() && audio.is_none();
        let wants_video = use_real_media || video.is_some();
        let wants_audio = options.mic.is_some() || audio.is_some();

        if !use_real_media && options.mic.is_some() && audio.is_none() {
            let error =
                error::config("Microphone capture was requested but no audio source was supplied");
            let _ = self
                .send_capture_state(false, false, None, None, None, Some(error.clone()))
                .await;
            return Err(error);
        }

        if wants_audio && self.channels.channel_for_kind(ChannelKind::Mic).is_none() {
            let error = mic_channel_missing();
            let _ = self
                .send_capture_state(false, false, None, None, None, Some(error.clone()))
                .await;
            return Err(error);
        }

        self.send_control(&ControlMessage::CaptureStart {
            video: wants_video,
            audio: wants_audio,
        })
        .await?;

        let started = if use_real_media {
            ClientCapture::start(self.transport.sender(), options).await
        } else {
            ClientCapture::start_with_media(self.transport.sender(), options, video, audio).await
        };

        match started {
            Ok(capture) => {
                let state = capture.state();
                self.send_capture_state(
                    state.video,
                    state.audio,
                    state.video_codec.clone(),
                    state.width,
                    state.height,
                    None,
                )
                .await?;
                self.captured = Some(state);
                self.capture = Some(capture);
                Ok(())
            }
            Err(error) => {
                let _ = self
                    .send_capture_state(false, false, None, None, None, Some(error.clone()))
                    .await;
                Err(error)
            }
        }
    }

    /// Stop the capture pipeline and tell the node to release its decode
    /// workers. Idempotent: calling it with no active capture does nothing.
    pub async fn stop_capture(&mut self) -> Result<(), ApiError> {
        let Some(mut capture) = self.capture.take() else {
            return Ok(());
        };
        let running = capture.state();
        if self.closed.is_none() {
            let _ = self
                .send_control(&ControlMessage::CaptureStop {
                    video: running.video,
                    audio: running.audio,
                })
                .await;
        }
        capture.shutdown().await;
        self.captured = Some(capture.state());
        Ok(())
    }

    /// The capture counters, including after a stop.
    pub fn capture_state(&self) -> Option<ClientCaptureState> {
        self.capture
            .as_ref()
            .map(ClientCapture::state)
            .or_else(|| self.captured.clone())
    }

    async fn send_capture_state(
        &self,
        video: bool,
        audio: bool,
        video_codec: Option<String>,
        width: Option<u16>,
        height: Option<u16>,
        error: Option<ApiError>,
    ) -> Result<(), ApiError> {
        self.send_control(&ControlMessage::CaptureState {
            video,
            audio,
            video_codec,
            width,
            height,
            error,
        })
        .await
    }

    /// Feed one raw control payload through the normal event handler.
    ///
    /// Integration tests use this to deliver a node-side `KeyframeRequest`
    /// without a second connection. Not part of the stable API.
    #[doc(hidden)]
    pub fn handle_control_for_test(&mut self, payload: &[u8]) -> Result<ClientEvent, ApiError> {
        self.handle_control_payload(payload)
    }

    /// Send `ReleaseAll`, send `Bye`, and close the QUIC connection.
    pub async fn close(&mut self) -> Result<(), ApiError> {
        if self.closed.is_some() {
            return Ok(());
        }
        let _ = self.stop_capture().await;
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
        self.disconnect().await
    }

    /// Detach this transport without asking the node to end the session.
    pub async fn disconnect(&mut self) -> Result<(), ApiError> {
        if self.closed.is_some() {
            return Ok(());
        }
        let _ = self.stop_capture().await;
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

    async fn await_hello_ack(
        &mut self,
        scene_cache_bytes: Option<usize>,
    ) -> Result<Session, ApiError> {
        loop {
            match self.transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                    let value: Value = serde_json::from_slice(&payload).map_err(|error| {
                        error::protocol(format!("Invalid control JSON from the node: {error}"))
                    })?;
                    match control_kind(&value)? {
                        "hello_ack" => {
                            let session = parse_hello_ack(&value)?;
                            self.wire_format = parse_wire_format(&value)?;
                            if let (Some(budget), Some(display)) =
                                (scene_cache_bytes, control_data(&value).get("display"))
                            {
                                let display =
                                    serde_json::from_value::<DisplayProfile>(display.clone())
                                        .map_err(|error| {
                                            error::protocol(format!(
                                                "Invalid display profile: {error}"
                                            ))
                                        })?;
                                if let DisplayProfile::Scene {
                                    version: SCENE_VERSION,
                                    initial_scene,
                                } = display
                                {
                                    self.resources = Some(self.transport.resource_reader()?);
                                    self.scene = Some(SceneReceiver::new(initial_scene, budget));
                                }
                            }
                            return Ok(session);
                        }
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
                self.wire_format = parse_wire_format(&value)?;
                Ok(ClientEvent::SessionUpdated(session))
            }
            "scene_commit" if self.scene.is_some() => {
                let commit = control_data(&value)
                    .get("commit")
                    .ok_or_else(|| error::protocol("SceneCommit has no commit"))?;
                let commit = serde_json::from_value(commit.clone())
                    .map_err(|error| error::protocol(format!("Invalid scene commit: {error}")))?;
                self.apply_scene_commit(commit)
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
            "input_latency_ack" => parse_input_latency_ack(&value),
            "keyframe_request" => {
                // A node-side keyframe request targets the camera uplink. It is
                // deliberately reported as an unknown control message so this
                // client does not grow a breaking public variant.
                if let Some(capture) = &self.capture {
                    capture.request_keyframe();
                }
                Ok(ClientEvent::UnknownControl {
                    kind: "keyframe_request".to_string(),
                })
            }
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

    /// Handle one frame from the reliable `Display` channel.
    ///
    /// Scene snapshots continue to feed the compositor's [`SceneReceiver`].
    /// Every other message kind is applied to the [`DisplaySession`] state
    /// machine and surfaced as [`ClientEvent::Display`].
    ///
    /// Unknown tags and malformed frames are reported as
    /// [`ClientEvent::UnknownControl`] instead of terminating the session, so a
    /// newer node can add message kinds without breaking this client. A
    /// decoded message that the state machine rejects (for example a data
    /// update for an unknown field) is still a protocol error.
    fn handle_display_payload(&mut self, payload: &[u8]) -> Result<ClientEvent, ApiError> {
        let message = match DisplayMessage::decode(payload) {
            Ok(message) => message,
            Err(CodecError::UnknownTag(tag)) => {
                return Ok(ClientEvent::UnknownControl {
                    kind: format!("display.unknown_tag.0x{tag:02x}"),
                });
            }
            Err(error) => {
                return Ok(ClientEvent::UnknownControl {
                    kind: format!("display.malformed.{error}"),
                });
            }
        };

        match message {
            DisplayMessage::SceneSnapshot(snapshot) => {
                if self.scene.is_none() {
                    return Ok(ClientEvent::UnknownControl {
                        kind: "display.scene_snapshot".to_string(),
                    });
                }
                let commit = scene_convert::scene_commit_from_proto(&snapshot)
                    .map_err(|error| error::protocol(format!("Invalid scene snapshot: {error}")))?;
                self.apply_scene_commit(commit)
            }
            message => {
                let kind = message.kind();
                let Some(display) = self.display.as_mut() else {
                    return Ok(ClientEvent::UnknownControl {
                        kind: format!("display.{kind}"),
                    });
                };
                let event = display.apply(message).map_err(|error| {
                    error::protocol(format!("Display {kind} message was rejected: {error}"))
                })?;
                Ok(ClientEvent::Display(event))
            }
        }
    }

    fn apply_scene_commit(
        &mut self,
        commit: ozma_protocol::scene::SceneCommit,
    ) -> Result<ClientEvent, ApiError> {
        let scene = self.scene.as_mut().expect("scene support was checked");
        match scene.receive_commit(commit) {
            CommitStatus::Presented => Ok(ClientEvent::ScenePresented {
                revision: scene.presented().revision,
            }),
            CommitStatus::InvalidPlacements => {
                Err(error::protocol("Scene commit contains invalid placements"))
            }
            CommitStatus::WaitingForResources | CommitStatus::IgnoredStale => {
                Ok(ClientEvent::ScenePending)
            }
        }
    }
}

struct AckSample {
    client_sent_micros: u64,
    server_received_micros: u64,
    latest_frame_sequence: u64,
    received: Instant,
}

struct CompletedFrame {
    sequence: u64,
    at: Instant,
    at_micros: u64,
}

fn latency_report(
    nonce: u64,
    started: Instant,
    ack: &AckSample,
    frame_sequence: u64,
    frame_at: Instant,
    frame_micros: u64,
) -> InputLatencyReport {
    // The raw timestamps come from different machines. Only use them for
    // server-to-frame when they agree to within a second; otherwise report
    // the clock-independent local measurements and leave this as `None`.
    let clock_skew = ack.server_received_micros.abs_diff(ack.client_sent_micros);
    let server_to_frame_ms =
        if clock_skew <= 1_000_000 && frame_micros >= ack.server_received_micros {
            Some((frame_micros - ack.server_received_micros) as f64 / 1000.0)
        } else {
            None
        };

    InputLatencyReport {
        nonce,
        event_accepted: true,
        ack_rtt_ms: ack
            .received
            .saturating_duration_since(started)
            .as_secs_f64()
            * 1000.0,
        server_to_frame_ms,
        input_to_frame_ms: frame_at.saturating_duration_since(started).as_secs_f64() * 1000.0,
        latest_frame_sequence: ack.latest_frame_sequence,
        frame_sequence,
        client_sent_micros: ack.client_sent_micros,
        server_received_micros: ack.server_received_micros,
    }
}

fn now_micros() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
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

/// Read the negotiated wire format from a `HelloAck`.
///
/// A node that predates negotiation omits the field; that means the legacy
/// JSON path remains in effect.
fn parse_wire_format(value: &Value) -> Result<WireFormat, ApiError> {
    match control_data(value).get("wire_format") {
        Some(format) => serde_json::from_value(format.clone()).map_err(|error| {
            error::protocol(format!("HelloAck has an invalid wire_format: {error}"))
                .with_details(json!({ "wire_format": format }))
        }),
        None => Ok(WireFormat::default()),
    }
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

fn parse_input_latency_ack(value: &Value) -> Result<ClientEvent, ApiError> {
    let data = control_data(value);
    let integer = |field: &str| {
        data.get(field).and_then(Value::as_u64).ok_or_else(|| {
            error::protocol(format!("InputLatencyAck has no unsigned integer '{field}'"))
                .with_details(json!({ "message": value }))
        })
    };
    Ok(ClientEvent::InputLatencyAck {
        nonce: integer("nonce")?,
        client_sent_micros: integer("client_sent_micros")?,
        server_received_micros: integer("server_received_micros")?,
        latest_frame_sequence: integer("latest_frame_sequence")?,
    })
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

#[cfg(test)]
mod latency_tests {
    use super::*;
    use crate::cert::certificate_fingerprint;
    use ozma_protocol::media::VideoFragmentHeader;
    use ozma_protocol::{
        ClientId, Participant, ParticipantRole, Permission, PermissionSet, Session, SessionId,
        SessionSpec, SessionState,
    };
    use ozma_transport::offer::{SessionOfferBuilder, TransportDescriptorBuilder};
    use ozma_transport::{QuicServer, TransportEvent};

    fn session() -> Session {
        let spec = SessionSpec {
            client_id: ClientId::new("client-test"),
            principal: "probe".to_string(),
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
        session.participants = vec![Participant {
            client_id: ClientId::new("client-test"),
            role: ParticipantRole::Controller,
            connected: true,
            last_seen: 2,
        }];
        session
    }

    fn offer_for(address: SocketAddr, fingerprint: &str) -> SessionOffer {
        let descriptor = TransportDescriptorBuilder::quic(address.to_string())
            .certificate_fingerprint(fingerprint)
            .build();
        SessionOfferBuilder::new("session-test")
            .token("token-test")
            .expires_at(1_700_000_000_000)
            .transport(descriptor)
            .build()
            .unwrap()
    }

    async fn recv_message(transport: &mut QuicTransport) -> ControlMessage {
        loop {
            match transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                    return serde_json::from_slice(&payload).unwrap();
                }
                TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
                TransportEvent::Error { error } => panic!("transport error: {error}"),
                TransportEvent::Closed { reason } => {
                    panic!("transport closed before control: {reason:?}")
                }
            }
        }
    }

    async fn send_control(transport: &QuicTransport, message: &ControlMessage) {
        transport
            .send(
                ChannelId::CONTROL,
                Bytes::from(serde_json::to_vec(message).unwrap()),
            )
            .await
            .unwrap();
    }

    async fn drain_until_close(transport: &mut QuicTransport) {
        loop {
            match transport.recv().await {
                TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                    let value: Value = serde_json::from_slice(&payload).unwrap();
                    if value["type"] == "bye" {
                        break;
                    }
                }
                TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
                TransportEvent::Error { .. } | TransportEvent::Closed { .. } => break,
            }
        }
    }

    #[tokio::test]
    async fn measurement_matches_its_ack_and_waits_for_a_frame_at_the_ack_sequence() {
        let server = QuicServer::bind("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = server.local_addr().unwrap();
        let certificate = server.certificate_der().to_vec();
        let offer = offer_for(address, &certificate_fingerprint(&certificate));

        let server_task = tokio::spawn(async move {
            let mut transport = server.accept().await.unwrap();
            let ControlMessage::Hello { .. } = recv_message(&mut transport).await else {
                panic!("expected Hello");
            };
            send_control(
                &transport,
                &ControlMessage::HelloAck {
                    session: session(),
                    wire_format: WireFormat::Json,
                    display: None,
                },
            )
            .await;

            let ControlMessage::InputLatency {
                event,
                client_sent_micros,
                nonce,
            } = recv_message(&mut transport).await
            else {
                panic!("expected InputLatency");
            };
            assert_eq!(event, InputEvent::ReleaseAll);

            // An ack for another nonce must be ignored by the client.
            send_control(
                &transport,
                &ControlMessage::InputLatencyAck {
                    nonce: nonce.wrapping_add(1),
                    client_sent_micros,
                    server_received_micros: client_sent_micros,
                    latest_frame_sequence: 99,
                },
            )
            .await;
            send_control(
                &transport,
                &ControlMessage::InputLatencyAck {
                    nonce,
                    client_sent_micros,
                    server_received_micros: client_sent_micros,
                    latest_frame_sequence: 10,
                },
            )
            .await;

            // A complete frame below the ack sequence is not the answer.
            let stale = VideoFragmentHeader::new(false, 0, 9, 0, 1).encode_into(b"stale");
            transport
                .send(ChannelId::VIDEO, Bytes::from(stale))
                .await
                .unwrap();
            let frame = VideoFragmentHeader::new(true, 0, 12, 0, 1).encode_into(b"live");
            transport
                .send(ChannelId::VIDEO, Bytes::from(frame))
                .await
                .unwrap();

            drain_until_close(&mut transport).await;
            let _ = transport.close_with_reason(CloseReason::Normal).await;
        });

        let mut client = NativeSessionClient::connect(&offer, &certificate)
            .await
            .unwrap();
        let report = client
            .measure_input_latency(&InputEvent::ReleaseAll, Duration::from_secs(2))
            .await
            .unwrap();
        assert_eq!(report.nonce, 1);
        assert!(report.event_accepted);
        assert_eq!(report.latest_frame_sequence, 10);
        assert_eq!(report.frame_sequence, 12);
        assert!(report.ack_rtt_ms >= 0.0);
        assert!(report.input_to_frame_ms >= 0.0);
        assert!(report.server_to_frame_ms.is_some());

        client.close().await.unwrap();
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .unwrap()
            .unwrap();
    }
}
