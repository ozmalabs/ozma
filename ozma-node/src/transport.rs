// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Native QUIC session service.
//!
//! One QUIC connection carries a session's typed channels:
//! reliable control/input on the QUIC stream, unreliable video/audio on QUIC
//! datagrams. The node verifies the controller-issued session token in the
//! first `Hello`, then runs the fake media pipeline while the client is
//! attached. Real V4L2 capture replaces the fake source without changing this
//! control contract.

use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use ozma_encode::{H264SoftwareEncoder, SelectedEncoder};
use ozma_media::{
    CaptureSource, Codec, FakeCapture, FakeEncoder, Frame, FrameFormat, MediaError, MediaFormat,
    Pipeline, PipelineConfig, RunningPipeline,
};
use ozma_protocol::scene::{DisplayCapabilities, DisplayProfile};
use ozma_protocol::{
    ClientId, ControlMessage, DisplayId, InputEvent, Permission, SessionEvent, SessionId,
};
use ozma_transport::{
    ChannelId, CloseReason, QuicServer, QuicTransport, SessionTransport, TransportEvent,
};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, Interval};
use tracing::{debug, info, warn};

use crate::audio_runtime::AudioRuntime;
use crate::backend::SessionBackend;

/// A bound QUIC listener plus the certificate information clients need.
pub struct QuicRuntime {
    server: QuicServer,
    advertised: SocketAddr,
    certificate_der: Vec<u8>,
    fingerprint: String,
}

impl QuicRuntime {
    /// Bind the native QUIC listener on `port` (`0` for an ephemeral port).
    pub fn bind(port: u16) -> Result<Self> {
        Self::bind_on(SocketAddr::from((Ipv4Addr::UNSPECIFIED, port)))
    }

    /// Bind on an explicit address. Tests use loopback with port 0.
    pub fn bind_on(bind_addr: SocketAddr) -> Result<Self> {
        let server = QuicServer::bind(bind_addr).context("binding native QUIC listener")?;
        let local = server.local_addr().context("reading QUIC local address")?;
        let advertised = advertised_address(local);
        let certificate_der = server.certificate_der().to_vec();
        let fingerprint = hex_sha256(&certificate_der);
        Ok(Self {
            server,
            advertised,
            certificate_der,
            fingerprint,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.advertised
    }

    pub fn endpoint(&self) -> String {
        self.advertised.to_string()
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

fn advertised_address(local: SocketAddr) -> SocketAddr {
    if local.ip().is_unspecified() {
        if let Ok(IpAddr::V4(ip)) = local_ip_address::local_ip() {
            return SocketAddr::new(IpAddr::V4(ip), local.port());
        }
    }
    local
}

fn hex_sha256(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// Accept QUIC sessions until the listener fails. Each accepted connection
/// runs in its own task; a bad client cannot stop the listener.
pub async fn run(runtime: QuicRuntime, backend: Arc<SessionBackend>) -> Result<()> {
    info!(
        endpoint = %runtime.endpoint(),
        fingerprint = %runtime.fingerprint(),
        "Native QUIC session listener ready"
    );
    loop {
        match runtime.server.accept().await {
            Ok(transport) => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(transport, backend).await {
                        debug!(error = %error, "QUIC session ended with error");
                    }
                });
            }
            Err(error) => {
                // `accept` uses the handshake timeout while waiting for a
                // client; an idle period is not a listener failure.
                if error.message.contains("closed") {
                    warn!(error = %error, "QUIC listener closed");
                    return Err(error.into());
                }
                debug!(error = %error, "no QUIC client yet; continuing to listen");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(
    mut transport: QuicTransport,
    backend: Arc<SessionBackend>,
) -> Result<()> {
    let (hello_session_id, hello_token, display) =
        timeout(Duration::from_secs(10), read_hello(&mut transport))
            .await
            .context("timed out waiting for session Hello")??;

    let claims = match backend.verify_token(&hello_token) {
        Ok(claims) => claims,
        Err(error) => {
            let _ = send_control(&transport, &ControlMessage::Error { error }).await;
            let _ = transport
                .close_with_reason(CloseReason::ProtocolError)
                .await;
            return Ok(());
        }
    };
    if claims.session_id != hello_session_id {
        let _ = send_control(
            &transport,
            &ControlMessage::Error {
                error: ozma_protocol::ApiError::new(
                    ozma_protocol::error::codes::FORBIDDEN,
                    "The token is for a different session",
                ),
            },
        )
        .await;
        let _ = transport
            .close_with_reason(CloseReason::ProtocolError)
            .await;
        return Ok(());
    }

    let session = match backend.manager().get(&claims.session_id) {
        Ok(session) => session,
        Err(error) => {
            let _ = send_control(&transport, &ControlMessage::Error { error }).await;
            let _ = transport
                .close_with_reason(CloseReason::ProtocolError)
                .await;
            return Ok(());
        }
    };
    if !session.state.is_active() {
        let _ = send_control(
            &transport,
            &ControlMessage::Error {
                error: ozma_protocol::ApiError::new(
                    ozma_protocol::error::codes::SESSION_NOT_READY,
                    "The session is not ready for a media connection",
                ),
            },
        )
        .await;
        let _ = transport
            .close_with_reason(CloseReason::ProtocolError)
            .await;
        return Ok(());
    }

    let session_id = claims.session_id.clone();
    let client_id = claims.client_id.clone();
    let _ = backend.manager().join(&session_id, &client_id);
    let can_control = session.permissions.contains(Permission::Control);
    if can_control {
        if let Err(error) = backend.manager().claim_control(&session_id, &client_id) {
            debug!(error = %error, "client did not become controller");
        }
    }

    let session = backend
        .manager()
        .get(&session_id)
        .unwrap_or_else(|_| session.clone());
    let selected_display = display
        .as_ref()
        .map(|caps| caps.select(ChannelId::VIDEO.raw()));
    send_control(
        &transport,
        &ControlMessage::HelloAck {
            session: session.clone(),
            display: selected_display.clone(),
        },
    )
    .await?;

    let mut pipeline = match start_media(&backend, &session).await {
        Ok(pipeline) => pipeline,
        Err(error) => {
            let _ = send_control(&transport, &ControlMessage::Error { error }).await;
            let _ = transport
                .close_with_reason(CloseReason::TransportError)
                .await;
            return Ok(());
        }
    };

    let mut audio = if session.spec.audio.enabled {
        Some(AudioRuntime::start())
    } else {
        None
    };

    let mut events = backend.manager().subscribe();
    let result = session_loop(
        &mut transport,
        &backend,
        &session_id,
        &client_id,
        &mut pipeline,
        &mut audio,
        &mut events,
        matches!(selected_display, Some(DisplayProfile::Scene { .. })).then(|| SceneStream {
            session: &session,
            last_revision: 0,
            initialized: false,
            poll: interval(Duration::from_millis(50)),
        }),
    )
    .await;

    if let Some(mut audio) = audio {
        audio.stop();
    }
    pipeline.cancel();
    let _ = pipeline.wait().await;
    let _ = backend
        .manager()
        .on_client_disconnect(&session_id, &client_id);
    let _ = backend.input_sink().release_all();
    let _ = transport.close_with_reason(CloseReason::Normal).await;

    result
}

async fn start_media(
    backend: &Arc<SessionBackend>,
    session: &ozma_protocol::Session,
) -> Result<RunningPipeline, ozma_protocol::ApiError> {
    let source_id = session
        .spec
        .tracks
        .first()
        .and_then(|track| track.source_id.as_deref());
    let source = match backend.open_capture(source_id) {
        Ok(real) => {
            tracing::info!(source = ?source_id, "Using real V4L2 capture for session");
            SessionSource::Real(real)
        }
        Err(error) => {
            tracing::debug!(error = %error, "No real capture available; using fake source");
            SessionSource::Fake(
                FakeCapture::default()
                    .with_id(format!("fake:{}", session.id))
                    .with_fps(30),
            )
        }
    };
    let input = source.format();
    let request = MediaFormat::new(
        Codec::H264,
        input.width.max(2),
        input.height.max(2),
        30,
        2_000,
    );
    let encoder = match H264SoftwareEncoder::new(input, request) {
        Ok(encoder) => {
            tracing::info!(input = %input, "Using software H.264 encoder");
            SelectedEncoder::Software(encoder.with_keyframe_interval(60))
        }
        Err(error) => {
            tracing::warn!(error = %error, "Software H.264 encoder unavailable; using fake encoder");
            SelectedEncoder::Fake(FakeEncoder::h264(input, 2_000))
        }
    };
    Pipeline::start(source, encoder, PipelineConfig::default()).map_err(Into::into)
}

/// The transport can use a real V4L2 source when one is present and the fake
/// source otherwise, without making `Pipeline` generic over a boxed trait.
enum SessionSource {
    Real(Box<dyn CaptureSource>),
    Fake(FakeCapture),
}

impl SessionSource {
    fn format(&self) -> FrameFormat {
        match self {
            SessionSource::Real(source) => source.format(),
            SessionSource::Fake(source) => source.format(),
        }
    }
}

#[async_trait]
impl CaptureSource for SessionSource {
    fn id(&self) -> &str {
        match self {
            SessionSource::Real(source) => source.id(),
            SessionSource::Fake(source) => source.id(),
        }
    }

    fn format(&self) -> FrameFormat {
        SessionSource::format(self)
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        match self {
            SessionSource::Real(source) => source.next_frame().await,
            SessionSource::Fake(source) => source.next_frame().await,
        }
    }

    async fn stop(&mut self) -> Result<(), MediaError> {
        match self {
            SessionSource::Real(source) => source.stop().await,
            SessionSource::Fake(source) => source.stop().await,
        }
    }
}

/// QUIC datagrams are MTU-bound, so a video access unit is split into
/// fragments with the protocol's reassembly header.
const VIDEO_FRAGMENT_PAYLOAD: usize = 1_100;

async fn send_video_frame(
    transport: &QuicTransport,
    frame: &ozma_media::EncodedFrame,
) -> Result<(), ozma_protocol::ApiError> {
    use ozma_protocol::media::VideoFragmentHeader;
    let data = frame.data();
    let count = data.len().div_ceil(VIDEO_FRAGMENT_PAYLOAD).max(1) as u16;
    for (index, chunk) in data.chunks(VIDEO_FRAGMENT_PAYLOAD).enumerate() {
        let header = VideoFragmentHeader::new(
            frame.is_keyframe(),
            0,
            frame.sequence(),
            index as u16,
            count,
        );
        transport
            .send(ChannelId::VIDEO, Bytes::from(header.encode_into(chunk)))
            .await?;
    }
    Ok(())
}

async fn send_initial_scene(
    transport: &QuicTransport,
    backend: &SessionBackend,
    session: &ozma_protocol::Session,
) -> Result<u64, ozma_protocol::ApiError> {
    let mut last_revision = 0;
    send_scene_updates(
        transport,
        backend.scene_updates(session),
        &mut last_revision,
    )
    .await?;
    Ok(last_revision)
}

async fn send_scene_updates(
    transport: &QuicTransport,
    updates: Vec<crate::backend::SceneUpdate>,
    last_revision: &mut u64,
) -> Result<(), ozma_protocol::ApiError> {
    for update in updates {
        if let Some(resource) = update.resource {
            timeout(
                Duration::from_secs(2),
                transport.send_resource(Bytes::from(resource)),
            )
            .await
            .map_err(|_| {
                ozma_protocol::ApiError::new(
                    ozma_protocol::codes::TRANSPORT_UNAVAILABLE,
                    "Scene resource upload timed out",
                )
            })??;
        }
        let revision = update.commit.revision;
        send_control(
            transport,
            &ControlMessage::SceneCommit {
                commit: update.commit,
            },
        )
        .await?;
        *last_revision = revision;
    }
    Ok(())
}

struct SceneStream<'a> {
    session: &'a ozma_protocol::Session,
    last_revision: u64,
    initialized: bool,
    poll: Interval,
}

async fn session_loop(
    transport: &mut QuicTransport,
    backend: &Arc<SessionBackend>,
    session_id: &SessionId,
    client_id: &ClientId,
    pipeline: &mut RunningPipeline,
    audio: &mut Option<AudioRuntime>,
    events: &mut broadcast::Receiver<SessionEvent>,
    mut scene_stream: Option<SceneStream<'_>>,
) -> Result<()> {
    loop {
        tokio::select! {
            event = transport.recv() => {
                match event {
                    TransportEvent::Payload { channel, payload } => {
                        if channel == ChannelId::CONTROL {
                            if let Err(error) = handle_control(backend, session_id, client_id, transport, &payload).await {
                                let _ = send_control(transport, &ControlMessage::Error { error }).await;
                            }
                        }
                    }
                    TransportEvent::Datagram { .. } => {}
                    TransportEvent::Closed { .. } => break,
                    TransportEvent::Error { error } => {
                        let _ = send_control(transport, &ControlMessage::Error { error }).await;
                        break;
                    }
                }
            }
            frame = pipeline.recv() => {
                match frame {
                    Some(frame) => {
                        if let Err(error) = send_video_frame(transport, &frame).await {
                            debug!(error = %error, "sending video frame failed");
                            break;
                        }
                        if let Some(scene) = scene_stream.as_mut().filter(|scene| !scene.initialized) {
                            match send_initial_scene(transport, backend, scene.session).await {
                                Ok(revision) => {
                                    scene.last_revision = revision;
                                    scene.initialized = true;
                                }
                                Err(error) => {
                                debug!(error = %error, "initial scene update incomplete");
                                }
                            }
                        }
                    }
                    None => break,
                }
            }
            audio_packet = async {
                match audio.as_mut() {
                    Some(runtime) => runtime.recv().await,
                    None => std::future::pending::<Option<ozma_audio::EncodedAudio>>().await,
                }
            } => {
                if let Some(packet) = audio_packet {
                    if let Err(error) = transport.send(ChannelId::AUDIO, Bytes::from(packet.data)).await {
                        debug!(error = %error, "sending audio packet failed");
                        break;
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(event) => {
                        let _ = send_control(transport, &ControlMessage::Event { event }).await;
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _tick = async {
                match scene_stream.as_mut() {
                    Some(scene) => {
                        scene.poll.tick().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            } => {
                if let Some(scene) = scene_stream.as_mut().filter(|scene| scene.initialized) {
                    let updates = backend.scene_updates_since(scene.session, scene.last_revision);
                    if let Err(error) = send_scene_updates(
                        transport,
                        updates,
                        &mut scene.last_revision,
                    ).await {
                        debug!(error = %error, "scene update failed");
                    }
                }
            }
        }
    }
    Ok(())
}

async fn handle_control(
    backend: &Arc<SessionBackend>,
    session_id: &SessionId,
    client_id: &ClientId,
    transport: &QuicTransport,
    payload: &[u8],
) -> Result<(), ozma_protocol::ApiError> {
    let message: ControlMessage = serde_json::from_slice(payload).map_err(|error| {
        ozma_protocol::ApiError::new(
            ozma_protocol::error::codes::INVALID_REQUEST,
            format!("invalid control message: {error}"),
        )
    })?;
    match message {
        ControlMessage::Input { event } => {
            match backend
                .manager()
                .submit_input(session_id, client_id, &event)
            {
                Ok(()) => {
                    if let Err(error) = backend.input_sink().submit(&event) {
                        send_control(transport, &ControlMessage::Error { error }).await?;
                    }
                }
                Err(error) => {
                    send_control(transport, &ControlMessage::Error { error }).await?;
                }
            }
        }
        ControlMessage::Ping { nonce } => {
            send_control(transport, &ControlMessage::Pong { nonce }).await?;
        }
        ControlMessage::Bye { reason } => {
            let _ = backend.manager().stop(session_id, reason);
            let _ = backend.input_sink().release_all();
        }
        ControlMessage::Hello { .. }
        | ControlMessage::HelloAck { .. }
        | ControlMessage::SceneCommit { .. } => {}
        ControlMessage::Pong { .. } => {}
        ControlMessage::Event { .. } => {}
        ControlMessage::Error { .. } => {}
    }
    Ok(())
}

async fn read_hello(
    transport: &mut QuicTransport,
) -> Result<(SessionId, String, Option<DisplayCapabilities>), ozma_protocol::ApiError> {
    loop {
        match transport.recv().await {
            TransportEvent::Payload { channel, payload } if channel == ChannelId::CONTROL => {
                let message: ControlMessage =
                    serde_json::from_slice(&payload).map_err(|error| {
                        ozma_protocol::ApiError::new(
                            ozma_protocol::error::codes::INVALID_REQUEST,
                            format!("invalid control message: {error}"),
                        )
                    })?;
                match message {
                    ControlMessage::Hello {
                        session_id,
                        token,
                        display,
                    } => {
                        return Ok((session_id, token, display));
                    }
                    _ => {
                        return Err(ozma_protocol::ApiError::new(
                            ozma_protocol::error::codes::UNAUTHORIZED,
                            "The first control message must be Hello",
                        ))
                    }
                }
            }
            TransportEvent::Payload { .. } | TransportEvent::Datagram { .. } => continue,
            TransportEvent::Closed { .. } => {
                return Err(ozma_protocol::ApiError::new(
                    ozma_protocol::error::codes::TRANSPORT_UNAVAILABLE,
                    "The client disconnected before Hello",
                ));
            }
            TransportEvent::Error { error } => return Err(error),
        }
    }
}

async fn send_control(
    transport: &QuicTransport,
    message: &ControlMessage,
) -> Result<(), ozma_protocol::ApiError> {
    let payload = serde_json::to_vec(message).map_err(|error| {
        ozma_protocol::ApiError::internal(format!("encoding control message: {error}"))
    })?;
    transport
        .send(ChannelId::CONTROL, Bytes::from(payload))
        .await
}

/// A client-side helper used by tests and the native client crate.
pub async fn send_input(
    transport: &QuicTransport,
    event: &InputEvent,
) -> Result<(), ozma_protocol::ApiError> {
    let message = ControlMessage::Input {
        event: event.clone(),
    };
    send_control(transport, &message).await
}

/// A client-side helper to send a display/edge update. Display ids are carried
/// in the input event; this keeps the server's accepted set explicit.
pub fn input_display(event: &InputEvent) -> Option<&DisplayId> {
    match event {
        InputEvent::PointerAbsolute { display_id, .. }
        | InputEvent::Touch { display_id, .. }
        | InputEvent::Pen { display_id, .. } => display_id.as_ref(),
        _ => None,
    }
}

/// Keep the control task handle type visible to future extensions.
pub type ControlTask = JoinHandle<()>;
