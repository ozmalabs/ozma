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
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result};
use async_trait::async_trait;
use bytes::Bytes;
use ozma_client_core::VideoAssembler;
use ozma_display_protocol::{scene_convert, DisplayMessage};
use ozma_media::{
    CaptureSource, FakeCapture, Frame, FrameFormat, MediaError, Pipeline, PipelineConfig,
    RunningPipeline,
};
use ozma_protocol::scene::{DisplayCapabilities, DisplayProfile};
use ozma_protocol::{
    ApiError, ChannelKind, ClientId, ControlMessage, DisplayId, InputEvent, Permission,
    SessionEvent, SessionId, WireFormat,
};
use ozma_transport::{ChannelId, CloseReason, QuicServer, SessionTransport, TransportEvent};
#[cfg(feature = "webtransport")]
use ozma_transport::{ChannelMap, WebTransportConfig, WebTransportServer};
use sha2::{Digest, Sha256};
use tokio::sync::broadcast;
use tokio::task::JoinHandle;
use tokio::time::{interval, timeout, Interval};
use tracing::{debug, info, warn};

use crate::audio_runtime::AudioRuntime;
use crate::backend::SessionBackend;
use crate::client_capture::{ClientCaptureHub, MAX_CAMERA_ACCESS_UNIT};

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

/// A bound WebTransport (HTTP/3) listener for the browser client.
///
/// It carries the same channels as [`QuicRuntime`] but a separate ECDSA P-256
/// certificate with a 14-day validity, because browsers only accept short-lived
/// certificates through `serverCertificateHashes`. Clients pin
/// [`Self::fingerprint`] (or the DER from [`Self::certificate_der`]).
#[cfg(feature = "webtransport")]
pub struct WebTransportRuntime {
    server: WebTransportServer,
    advertised: SocketAddr,
    session_url: String,
    certificate_der: Vec<u8>,
    fingerprint: String,
}

#[cfg(feature = "webtransport")]
impl WebTransportRuntime {
    /// Bind the WebTransport listener on `port` (`0` for an ephemeral port).
    /// The default bind address is loopback; use [`Self::bind_on`] for another.
    pub fn bind(port: u16) -> Result<Self> {
        Self::bind_on(SocketAddr::from((Ipv4Addr::LOCALHOST, port)))
    }

    /// Bind on an explicit address. Tests use loopback with port 0.
    pub fn bind_on(bind_addr: SocketAddr) -> Result<Self> {
        // Resolve the address a LAN browser will dial *before* binding so the
        // certificate's SANs cover it; the port comes from the bound socket.
        let advertised_ip = if bind_addr.ip().is_unspecified() {
            local_ip_address::local_ip().unwrap_or(bind_addr.ip())
        } else {
            bind_addr.ip()
        };
        let mut config = WebTransportConfig::default();
        for address in [advertised_ip, bind_addr.ip()] {
            let name = address.to_string();
            if !config.certificate_names.contains(&name) {
                config.certificate_names.push(name);
            }
        }
        let server =
            WebTransportServer::bind_with_config(bind_addr, config, ChannelMap::default_session())
                .context("binding WebTransport listener")?;
        let advertised = SocketAddr::new(advertised_ip, server.local_addr().port());
        let session_url = webtransport_url(advertised, "/session");
        let certificate_der = server.certificate_der().to_vec();
        let fingerprint = server.fingerprint().to_string();
        tracing::debug!(
            local = %server.local_addr(),
            advertised = %advertised,
            url = %session_url,
            "WebTransport listener bound"
        );
        Ok(Self {
            server,
            advertised,
            session_url,
            certificate_der,
            fingerprint,
        })
    }

    pub fn local_addr(&self) -> SocketAddr {
        self.advertised
    }

    /// The `https://host:port/session` URL advertised to browsers.
    pub fn endpoint(&self) -> String {
        self.session_url.clone()
    }

    /// The same value as [`Self::endpoint`], for symmetry with the API field.
    pub fn session_url(&self) -> &str {
        &self.session_url
    }

    pub fn certificate_der(&self) -> &[u8] {
        &self.certificate_der
    }

    /// Raw SHA-256 of the DER certificate, for `WebTransportClient` pinning.
    pub fn certificate_sha256(&self) -> [u8; 32] {
        self.server.certificate_sha256()
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }
}

#[cfg(feature = "webtransport")]
fn webtransport_url(address: SocketAddr, path: &str) -> String {
    let host = match address.ip() {
        IpAddr::V6(ip) => format!("[{ip}]"),
        IpAddr::V4(ip) => ip.to_string(),
    };
    let path = if path.starts_with('/') {
        path.to_string()
    } else {
        format!("/{path}")
    };
    format!("https://{host}:{}{path}", address.port())
}

fn hex_sha256(data: &[u8]) -> String {
    let digest = Sha256::digest(data);
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn now_micros() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_micros().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
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
                    if let Err(error) = handle_connection(Box::new(transport), backend).await {
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

/// Accept WebTransport sessions until the listener fails. Each accepted
/// connection runs the same [`handle_connection`] path as raw QUIC.
#[cfg(feature = "webtransport")]
pub async fn run_webtransport(
    runtime: WebTransportRuntime,
    backend: Arc<SessionBackend>,
) -> Result<()> {
    info!(
        endpoint = %runtime.endpoint(),
        fingerprint = %runtime.fingerprint(),
        "WebTransport session listener ready"
    );
    loop {
        match runtime.server.accept().await {
            Ok(transport) => {
                let backend = backend.clone();
                tokio::spawn(async move {
                    if let Err(error) = handle_connection(Box::new(transport), backend).await {
                        debug!(error = %error, "WebTransport session ended with error");
                    }
                });
            }
            Err(error) => {
                // `accept` uses the handshake timeout while waiting for a
                // browser; an idle period is not a listener failure.
                if error.message.contains("closed") {
                    warn!(error = %error, "WebTransport listener closed");
                    return Err(error.into());
                }
                debug!(error = %error, "no WebTransport client yet; continuing to listen");
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        }
    }
}

async fn handle_connection(
    transport: Box<dyn SessionTransport>,
    backend: Arc<SessionBackend>,
) -> Result<()> {
    let mut transport = transport;
    let (hello_session_id, hello_token, display, client_wire_formats) =
        timeout(Duration::from_secs(10), read_hello(&mut *transport))
            .await
            .context("timed out waiting for session Hello")??;

    // Protobuf is preferred when both peers support it. A client that predates
    // negotiation sends no formats, so the JSON compatibility path is selected.
    let wire_format = WireFormat::negotiate(
        &[WireFormat::Protobuf, WireFormat::Json],
        &client_wire_formats,
    )
    .unwrap_or(WireFormat::Json);

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
            wire_format,
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
        Some(AudioRuntime::start().await)
    } else {
        None
    };

    let mut events = backend.manager().subscribe();
    // Resolve browser uplink channels from the negotiated map rather than
    // assuming the conventional id layout.
    let channels = transport.channel_map();
    let camera_channel = channels.channel_for_kind(ChannelKind::Camera);
    let mic_channel = channels.channel_for_kind(ChannelKind::Mic);
    let display_channel = channels.channel_for_kind(ChannelKind::Display);
    let capture = backend.client_capture(&session_id);
    // Shared with control handling so an input-latency ack can name the most
    // recent encoded frame without blocking the media loop. This tracks the
    // encoded frame sequence sent to the client, not display scanout.
    let latest_frame_sequence = AtomicU64::new(0);
    let result = session_loop(
        &mut transport,
        &backend,
        &session_id,
        &client_id,
        &mut pipeline,
        &mut audio,
        &mut events,
        &latest_frame_sequence,
        &capture,
        camera_channel,
        mic_channel,
        matches!(selected_display, Some(DisplayProfile::Scene { .. })).then(|| SceneStream {
            session: &session,
            wire_format,
            display_channel,
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
    let effective_max_fps = backend.effective_max_fps(&session.spec);
    let (effective_max_bitrate_kbps, _) = backend.effective_max_bitrate_kbps(&session.spec);
    let input = source.format();
    let encoder = match backend.build_encoder(input, effective_max_fps, effective_max_bitrate_kbps)
    {
        Ok(encoder) => {
            tracing::info!(
                input = %input,
                encoder = ozma_media::Encoder::id(&encoder),
                "Using selected H.264 encoder"
            );
            encoder
        }
        Err(error) => {
            tracing::warn!(error = %error, "No real H.264 encoder available; using fake encoder");
            SessionBackend::fake_fallback_encoder(input, effective_max_bitrate_kbps)
        }
    };
    let source = FrameRateLimitedCapture::new(source, effective_max_fps);
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

/// Enforce the admitted maximum on outgoing media even when a capture device
/// or the fake fallback produces frames faster than the selected encoder rate.
struct FrameRateLimitedCapture {
    source: SessionSource,
    max_fps: u32,
    last_frame_at: Option<Instant>,
}

impl FrameRateLimitedCapture {
    fn new(source: SessionSource, max_fps: u32) -> Self {
        Self {
            source,
            max_fps: max_fps.max(1),
            last_frame_at: None,
        }
    }
}

#[async_trait]
impl CaptureSource for FrameRateLimitedCapture {
    fn id(&self) -> &str {
        self.source.id()
    }

    fn format(&self) -> FrameFormat {
        self.source.format()
    }

    async fn next_frame(&mut self) -> Result<Frame, MediaError> {
        let minimum_interval = Duration::from_secs_f64(1.0 / f64::from(self.max_fps));
        loop {
            let frame = self.source.next_frame().await?;
            let now = Instant::now();
            if self
                .last_frame_at
                .is_some_and(|last| now.duration_since(last) < minimum_interval)
            {
                continue;
            }
            self.last_frame_at = Some(now);
            return Ok(frame);
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
    transport: &dyn SessionTransport,
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
    transport: &dyn SessionTransport,
    backend: &SessionBackend,
    session: &ozma_protocol::Session,
    wire_format: WireFormat,
    display_channel: Option<ChannelId>,
) -> Result<u64, ozma_protocol::ApiError> {
    let mut last_revision = 0;
    send_scene_updates(
        transport,
        backend.scene_updates(session),
        &mut last_revision,
        wire_format,
        display_channel,
    )
    .await?;
    Ok(last_revision)
}

async fn send_scene_updates(
    transport: &dyn SessionTransport,
    updates: Vec<crate::backend::SceneUpdate>,
    last_revision: &mut u64,
    wire_format: WireFormat,
    display_channel: Option<ChannelId>,
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
        if wire_format == WireFormat::Protobuf {
            if let Some(channel) = display_channel {
                let snapshot =
                    scene_convert::scene_commit_to_proto(&update.commit, ChannelId::VIDEO.raw())
                        .map_err(|error| {
                            ozma_protocol::ApiError::new(
                                ozma_protocol::codes::INVALID_REQUEST,
                                format!("Could not encode the scene commit as protobuf: {error}"),
                            )
                        })?;
                let frame = DisplayMessage::SceneSnapshot(snapshot)
                    .encode()
                    .map_err(|error| {
                        ozma_protocol::ApiError::new(
                            ozma_protocol::codes::INVALID_REQUEST,
                            format!("Could not encode the scene snapshot: {error}"),
                        )
                    })?;
                transport.send(channel, Bytes::from(frame)).await?;
                *last_revision = revision;
                continue;
            }
        }
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

/// Reassemble one browser camera datagram and hand complete access units to
/// the capture hub. Reassembly failures are counted and never end the session.
fn feed_camera(capture: &ClientCaptureHub, assembler: &mut VideoAssembler, payload: &[u8]) {
    match assembler.push(payload) {
        Ok(Some(frame)) => {
            let sequence = frame.sequence;
            let keyframe = frame.keyframe;
            if let Err(error) = capture.push_video(sequence, keyframe, Bytes::from(frame.data)) {
                debug!(error = %error, "rejecting browser camera access unit");
            }
        }
        Ok(None) => {}
        Err(error) => {
            capture.record_rejected();
            capture.record_error(ApiError::new(
                "capture.reassembly_failed",
                format!("The browser camera fragment could not be reassembled: {error}"),
            ));
            // Drop the partial access unit so one bad datagram does not poison
            // the next frame.
            assembler.reset();
            debug!(error = %error, "browser camera fragment rejected");
        }
    }
}

struct SceneStream<'a> {
    session: &'a ozma_protocol::Session,
    wire_format: WireFormat,
    display_channel: Option<ChannelId>,
    last_revision: u64,
    initialized: bool,
    poll: Interval,
}

async fn session_loop(
    transport: &mut dyn SessionTransport,
    backend: &Arc<SessionBackend>,
    session_id: &SessionId,
    client_id: &ClientId,
    pipeline: &mut RunningPipeline,
    audio: &mut Option<AudioRuntime>,
    events: &mut broadcast::Receiver<SessionEvent>,
    latest_frame_sequence: &AtomicU64,
    capture: &ClientCaptureHub,
    camera_channel: Option<ChannelId>,
    mic_channel: Option<ChannelId>,
    mut scene_stream: Option<SceneStream<'_>>,
) -> Result<()> {
    let mut camera_assembler = VideoAssembler::new(MAX_CAMERA_ACCESS_UNIT, 4);
    loop {
        tokio::select! {
            event = transport.recv() => {
                match event {
                    TransportEvent::Payload { channel, payload }
                    | TransportEvent::Datagram { channel, payload } => {
                        if channel == ChannelId::CONTROL {
                            if let Err(error) = handle_control(
                                backend,
                                session_id,
                                client_id,
                                latest_frame_sequence,
                                capture,
                                pipeline,
                                &*transport,
                                &payload,
                            )
                            .await
                            {
                                let _ = send_control(&*transport, &ControlMessage::Error { error }).await;
                            }
                        } else if Some(channel) == camera_channel {
                            feed_camera(capture, &mut camera_assembler, &payload);
                        } else if Some(channel) == mic_channel {
                            if let Err(error) = capture.push_audio(payload) {
                                debug!(error = %error, "rejecting browser microphone packet");
                            }
                        }
                    }
                    TransportEvent::Closed { .. } => break,
                    TransportEvent::Error { error } => {
                        let _ = send_control(&*transport, &ControlMessage::Error { error }).await;
                        break;
                    }
                }
            }
            frame = pipeline.recv() => {
                match frame {
                    Some(frame) => {
                        if let Err(error) = send_video_frame(&*transport, &frame).await {
                            debug!(error = %error, "sending video frame failed");
                            break;
                        }
                        latest_frame_sequence.store(frame.sequence(), Ordering::Relaxed);
                        if let Some(scene) = scene_stream.as_mut().filter(|scene| !scene.initialized) {
                            match send_initial_scene(
                                &*transport,
                                backend,
                                scene.session,
                                scene.wire_format,
                                scene.display_channel,
                            )
                            .await
                            {
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
                    let wire_format = scene.wire_format;
                    let display_channel = scene.display_channel;
                    if let Err(error) = send_scene_updates(
                        &*transport,
                        updates,
                        &mut scene.last_revision,
                        wire_format,
                        display_channel,
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
    latest_frame_sequence: &AtomicU64,
    capture: &ClientCaptureHub,
    pipeline: &RunningPipeline,
    transport: &dyn SessionTransport,
    payload: &[u8],
) -> Result<(), ozma_protocol::ApiError> {
    // Capture receive time before parsing so the ack measures the node's
    // handling path rather than serde's cost.
    let server_received_micros = now_micros();
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
        ControlMessage::InputLatency {
            event,
            client_sent_micros,
            nonce,
        } => {
            // The same authorization and sink path as ordinary input; only a
            // successful submission produces an ack. Unauthorized probes use
            // the existing structured Error, so a view-only client cannot tell
            // whether the event would have been accepted.
            match authorize_latency_event(
                backend,
                session_id,
                client_id,
                &event,
                nonce,
                client_sent_micros,
                server_received_micros,
                latest_frame_sequence.load(Ordering::Relaxed),
            ) {
                Ok(ack) => send_control(transport, &ack).await?,
                Err(error) => send_control(transport, &ControlMessage::Error { error }).await?,
            }
        }
        ControlMessage::Ping { nonce } => {
            send_control(transport, &ControlMessage::Pong { nonce }).await?;
        }
        ControlMessage::Bye { reason } => {
            let _ = backend.manager().stop(session_id, reason);
            let _ = backend.input_sink().release_all();
        }
        ControlMessage::CaptureStart { video, audio } => {
            capture.start(video, audio)?;
            info!(video, audio, "Browser camera/microphone capture started");
        }
        ControlMessage::CaptureStop { video, audio } => {
            capture.stop(video, audio);
            info!(video, audio, "Browser camera/microphone capture stopped");
        }
        ControlMessage::CaptureState {
            video,
            audio,
            video_codec,
            width,
            height,
            error,
        } => {
            capture.apply_state(video, audio, video_codec.as_deref(), width, height);
            if let Some(error) = error {
                warn!(
                    code = %error.code,
                    message = %error.message,
                    "Browser capture reported an error"
                );
                capture.record_error(error);
            }
        }
        ControlMessage::KeyframeRequest { track } => {
            pipeline.request_keyframe();
            info!(?track, "Forcing the next encoded frame to be a keyframe");
        }
        ControlMessage::Hello { .. }
        | ControlMessage::HelloAck { .. }
        | ControlMessage::SceneCommit { .. } => {}
        ControlMessage::Pong { .. } => {}
        ControlMessage::InputLatencyAck { .. } => {}
        ControlMessage::Event { .. } => {}
        ControlMessage::Error { .. } => {}
    }
    Ok(())
}

/// Authorize and apply one latency-probe event, then build the ack. Split out
/// from the transport loop so the authorization/ack contract is unit-testable
/// without a QUIC connection.
fn authorize_latency_event(
    backend: &SessionBackend,
    session_id: &SessionId,
    client_id: &ClientId,
    event: &InputEvent,
    nonce: u64,
    client_sent_micros: u64,
    server_received_micros: u64,
    latest_frame_sequence: u64,
) -> Result<ControlMessage, ApiError> {
    backend
        .manager()
        .submit_input(session_id, client_id, event)?;
    backend.input_sink().submit(event)?;
    Ok(ControlMessage::InputLatencyAck {
        nonce,
        client_sent_micros,
        server_received_micros,
        latest_frame_sequence,
    })
}

async fn read_hello(
    transport: &mut dyn SessionTransport,
) -> Result<
    (
        SessionId,
        String,
        Option<DisplayCapabilities>,
        Vec<WireFormat>,
    ),
    ozma_protocol::ApiError,
> {
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
                        wire_formats,
                        display,
                    } => {
                        return Ok((session_id, token, display, wire_formats));
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
    transport: &dyn SessionTransport,
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
    transport: &dyn SessionTransport,
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

#[cfg(test)]
mod latency_tests {
    use super::*;
    use ozma_auth::{ConsentPolicy, MachineClass};
    use ozma_protocol::{ClientId, NodeCapabilities, Permission, PermissionSet, SessionSpec};

    fn fixture(permissions: PermissionSet) -> (Arc<SessionBackend>, SessionId, ClientId) {
        let backend = Arc::new(SessionBackend::new(
            NodeCapabilities::fake("latency-test"),
            ConsentPolicy::for_machine(MachineClass::Server),
        ));
        let client_id = ClientId::new("client-1");
        let spec = SessionSpec {
            client_id: client_id.clone(),
            principal: "alice".into(),
            tracks: Vec::new(),
            audio: Default::default(),
            input: Default::default(),
            privacy: Default::default(),
            transport: Default::default(),
            quality: None,
            ttl_seconds: 60,
        };
        let now = now_micros() / 1_000_000;
        let session = backend
            .manager()
            .create(spec, permissions.clone(), now)
            .unwrap();
        backend.manager().start(&session.id).unwrap();
        backend.manager().mark_streaming(&session.id).unwrap();
        backend.manager().join(&session.id, &client_id).unwrap();
        if permissions.contains(Permission::Control) {
            backend
                .manager()
                .claim_control(&session.id, &client_id)
                .unwrap();
        }
        (backend, session.id, client_id)
    }

    #[test]
    fn input_latency_applies_the_event_and_acks_with_timing_and_frame_sequence() {
        let permissions = PermissionSet::from_iter([Permission::View, Permission::Control]);
        let (backend, session_id, client_id) = fixture(permissions);
        let latest = AtomicU64::new(37);
        let event = InputEvent::Key {
            code: 0x04,
            down: true,
            modifiers: Default::default(),
        };

        let ack = authorize_latency_event(
            &backend,
            &session_id,
            &client_id,
            &event,
            99,
            1_700_000_000_000_123,
            1_700_000_000_000_456,
            latest.load(Ordering::Relaxed),
        )
        .unwrap();

        match ack {
            ControlMessage::InputLatencyAck {
                nonce,
                client_sent_micros,
                server_received_micros,
                latest_frame_sequence,
            } => {
                assert_eq!(nonce, 99);
                assert_eq!(client_sent_micros, 1_700_000_000_000_123);
                assert_eq!(server_received_micros, 1_700_000_000_000_456);
                assert_eq!(latest_frame_sequence, 37);
            }
            other => panic!("expected InputLatencyAck, got {other:?}"),
        }
        assert!(
            backend.input_sink().held().has_key(0x04),
            "the probe event must reach the same input sink as ordinary input"
        );
    }

    #[test]
    fn unauthorized_input_latency_returns_the_existing_error_instead_of_an_ack() {
        let permissions = PermissionSet::from_iter([Permission::View]);
        let (backend, session_id, client_id) = fixture(permissions);
        let event = InputEvent::Button {
            button: ozma_protocol::PointerButton::Left,
            down: true,
        };

        let error =
            authorize_latency_event(&backend, &session_id, &client_id, &event, 1, 10, 20, 0)
                .unwrap_err();

        assert_eq!(error.code, ozma_protocol::codes::FORBIDDEN);
        assert!(
            backend.input_sink().held().is_empty(),
            "an unauthorized probe must not reach the input sink"
        );
    }
}
