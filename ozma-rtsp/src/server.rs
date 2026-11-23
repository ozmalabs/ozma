// SPDX-License-Identifier: AGPL-3.0-only
//! RTSP server and interleaved/UDP RTP media path.
//!
//! One TCP connection owns one or more sessions. Requests are read sequentially;
//! a `PLAY` spawns a media task that shares the write half through a mutex so
//! RTSP responses and `$`-framed RTP packets never interleave mid-message.

use std::collections::HashMap;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use ozma_media::{Codec, EncodedFrame};
use ozma_rtp::{
    H264Packetizer, PacketizerConfig, RtpTimestampClock, DEFAULT_PACKETIZER_MAX_ACCESS_UNIT_BYTES,
};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{watch, Mutex};
use tokio::task::JoinHandle;
use tracing::{debug, warn};

use crate::error::RtspError;
use crate::request::{
    parse_head, request_from_head, Method, RtspRequest, RtspResponse, MAX_BODY_BYTES,
};
use crate::sdp::{h264_sdp, SdpConfig};
use crate::session::{parse_transport, SessionState, TransportRequest};
use crate::source::EncodedVideoSource;

/// Factory invoked for each `PLAY`, so sessions do not share mutable sources.
pub type SourceFactory = Arc<dyn Fn() -> Box<dyn EncodedVideoSource> + Send + Sync>;

/// Server configuration.
#[derive(Clone, Debug)]
pub struct RtspConfig {
    /// Value of the `Server` response header.
    pub server_name: String,
    /// RTP MTU used by the packetizer.
    pub mtu: usize,
    /// RTP payload type advertised in SDP and used in packets.
    pub payload_type: u8,
    /// Idle timeout for RTSP keepalives and per-session cleanup.
    pub session_timeout: Duration,
    /// Accept `RTP/AVP;unicast;client_port=N-M` SETUP requests.
    pub udp_enabled: bool,
    /// Advertised frame rate and fallback timestamp step.
    pub fps: u32,
    /// Maximum request header bytes accepted.
    pub max_header_bytes: usize,
    /// Maximum request body bytes accepted.
    pub max_body_bytes: usize,
    /// Maximum access-unit size accepted from a source.
    pub max_access_unit_bytes: usize,
}

impl Default for RtspConfig {
    fn default() -> Self {
        Self {
            server_name: "Ozma RTSP".to_owned(),
            mtu: ozma_rtp::DEFAULT_MTU,
            payload_type: ozma_rtp::DEFAULT_PAYLOAD_TYPE,
            session_timeout: Duration::from_secs(60),
            udp_enabled: true,
            fps: 30,
            max_header_bytes: 16 * 1024,
            max_body_bytes: MAX_BODY_BYTES,
            max_access_unit_bytes: DEFAULT_PACKETIZER_MAX_ACCESS_UNIT_BYTES,
        }
    }
}

impl RtspConfig {
    /// Default configuration.
    pub fn new() -> Self {
        Self::default()
    }
}

#[derive(Clone)]
struct ServerState {
    nonce: u64,
    counter: Arc<AtomicU64>,
}

impl ServerState {
    fn new() -> Self {
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos() as u64;
        let nonce = nanos ^ ((std::process::id() as u64) << 24) ^ 0x9e37_79b9_7f4a_7c15;
        Self {
            nonce,
            counter: Arc::new(AtomicU64::new(0)),
        }
    }

    fn session_id(&self) -> String {
        let count = self.counter.fetch_add(1, Ordering::Relaxed);
        format!("{:016x}{:04x}", self.nonce, count as u16)
    }
}

/// A TCP RTSP server.
///
/// ```no_run
/// use ozma_rtsp::{RtspConfig, RtspServer};
/// use tokio::net::TcpListener;
///
/// # async fn run() -> std::io::Result<()> {
/// let listener = TcpListener::bind("0.0.0.0:8554").await?;
/// let server = RtspServer::new(RtspConfig::default());
/// server.serve(listener).await
/// # }
/// ```
#[derive(Clone)]
pub struct RtspServer {
    config: Arc<RtspConfig>,
    factory: Option<SourceFactory>,
    state: ServerState,
}

impl RtspServer {
    /// Create a server without a media source.
    ///
    /// `DESCRIBE` and `OPTIONS` still work; `PLAY` answers 503 until
    /// [`RtspServer::with_source_factory`] is used.
    pub fn new(config: RtspConfig) -> Self {
        Self {
            config: Arc::new(config),
            factory: None,
            state: ServerState::new(),
        }
    }

    /// Set the factory used to create a fresh source for each `PLAY`.
    pub fn with_source_factory<F>(mut self, factory: F) -> Self
    where
        F: Fn() -> Box<dyn EncodedVideoSource> + Send + Sync + 'static,
    {
        self.factory = Some(Arc::new(factory));
        self
    }

    /// Current configuration.
    pub fn config(&self) -> &RtspConfig {
        &self.config
    }

    /// Accept connections until the listener fails.
    ///
    /// Each connection is handled in its own Tokio task. Abort the returned
    /// future (or drop the listener) to stop accepting new clients.
    pub async fn serve(self, listener: TcpListener) -> io::Result<()> {
        loop {
            let (stream, peer) = listener.accept().await?;
            let _ = stream.set_nodelay(true);
            let server = self.clone();
            tokio::spawn(async move {
                if let Err(error) = server.handle_connection(stream, peer).await {
                    debug!(%peer, %error, "RTSP connection ended");
                }
            });
        }
    }

    async fn handle_connection(&self, stream: TcpStream, peer: SocketAddr) -> io::Result<()> {
        let local_addr = stream.local_addr()?;
        let (read_half, write_half) = stream.into_split();
        let mut connection = Connection {
            peer,
            local_addr,
            writer: Arc::new(Mutex::new(write_half)),
            config: self.config.clone(),
            factory: self.factory.clone(),
            sessions: HashMap::new(),
            current: None,
            state: self.state.clone(),
        };
        let mut reader = BufReader::new(read_half);
        let idle = connection.config.session_timeout;
        loop {
            let read = tokio::time::timeout(
                idle,
                read_request(
                    &mut reader,
                    connection.config.max_header_bytes,
                    connection.config.max_body_bytes,
                ),
            )
            .await;
            let outcome = match read {
                Err(_) => {
                    debug!("RTSP session timed out");
                    break;
                }
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    debug!(%error, "RTSP read error");
                    break;
                }
            };
            match outcome {
                ReadOutcome::Eof => break,
                ReadOutcome::Malformed(message) => {
                    let response = connection.error_text(400, "Bad Request", &message);
                    if !connection.send(response).await {
                        break;
                    }
                }
                ReadOutcome::Fatal(message) => {
                    let response = connection.error_text(400, "Bad Request", &message);
                    let _ = connection.send(response).await;
                    break;
                }
                ReadOutcome::Request(request) => {
                    connection.prune_sessions();
                    let (response, close) = connection.handle(request).await;
                    if let Some(response) = response {
                        if !connection.send(response).await {
                            break;
                        }
                    }
                    if close {
                        break;
                    }
                }
            }
        }
        // Dropping sessions cancels and aborts their media tasks.
        Ok(())
    }
}

struct Connection {
    peer: SocketAddr,
    local_addr: SocketAddr,
    writer: Arc<Mutex<OwnedWriteHalf>>,
    config: Arc<RtspConfig>,
    factory: Option<SourceFactory>,
    sessions: HashMap<String, ActiveSession>,
    current: Option<String>,
    state: ServerState,
}

struct ActiveSession {
    state: SessionState,
    transport: Option<SessionTransport>,
    media: Option<MediaHandle>,
    last_activity: Instant,
}

impl ActiveSession {
    fn new(state: SessionState) -> Self {
        Self {
            state,
            transport: None,
            media: None,
            last_activity: Instant::now(),
        }
    }

    fn stop_media(&mut self) {
        let _ = self.media.take();
    }
}

impl Drop for ActiveSession {
    fn drop(&mut self) {
        self.stop_media();
    }
}

struct MediaHandle {
    cancel: watch::Sender<bool>,
    task: JoinHandle<()>,
}

impl Drop for MediaHandle {
    fn drop(&mut self) {
        let _ = self.cancel.send(true);
        self.task.abort();
    }
}

#[derive(Clone)]
enum SessionTransport {
    Tcp {
        rtp_channel: u8,
        rtcp_channel: u8,
    },
    Udp {
        rtp_socket: Arc<UdpSocket>,
        _rtcp_socket: Arc<UdpSocket>,
        client_rtp: SocketAddr,
        client_rtcp: SocketAddr,
        server_rtp: u16,
        server_rtcp: u16,
    },
}

struct PlayPlan {
    session_id: String,
    source: Box<dyn EncodedVideoSource>,
    sink: MediaSink,
    packetizer: H264Packetizer,
}

enum MediaSink {
    Tcp {
        writer: Arc<Mutex<OwnedWriteHalf>>,
        channel: u8,
    },
    Udp {
        socket: Arc<UdpSocket>,
        peer: SocketAddr,
    },
}

impl MediaSink {
    async fn send(&self, packet: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp { writer, channel } => {
                if packet.len() > u16::MAX as usize {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "RTP packet is too large for interleaved framing",
                    ));
                }
                let mut framed = Vec::with_capacity(4 + packet.len());
                framed.push(b'$');
                framed.push(*channel);
                framed.extend_from_slice(&(packet.len() as u16).to_be_bytes());
                framed.extend_from_slice(packet);
                let mut writer = writer.lock().await;
                writer.write_all(&framed).await?;
                writer.flush().await
            }
            Self::Udp { socket, peer } => {
                let sent = socket.send_to(packet, *peer).await?;
                if sent != packet.len() {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "UDP RTP send was truncated",
                    ));
                }
                Ok(())
            }
        }
    }
}

enum ReadOutcome {
    Request(RtspRequest),
    Malformed(String),
    Fatal(String),
    Eof,
}

impl Connection {
    async fn handle(&mut self, request: RtspRequest) -> (Option<RtspResponse>, bool) {
        let close = request
            .headers
            .get("connection")
            .map(|value| value.eq_ignore_ascii_case("close"))
            .unwrap_or(false);
        if let Some(id) = request.session_id() {
            if let Some(session) = self.sessions.get_mut(id) {
                session.last_activity = Instant::now();
            }
        }

        match request.method.clone() {
            Method::Options => (Some(self.options(&request)), close),
            Method::Describe => (Some(self.describe(&request)), close),
            Method::Setup => match self.setup(&request).await {
                Ok(response) => (Some(response), close),
                Err(error) => (Some(self.error_response(&request, &error)), close),
            },
            Method::Play => match self.prepare_play(&request) {
                Ok((response, plan)) => {
                    if !self.send(response).await {
                        return (None, true);
                    }
                    self.start_play(plan);
                    (None, close)
                }
                Err(error) => (Some(self.error_response(&request, &error)), close),
            },
            Method::Pause => match self.pause(&request) {
                Ok(response) => (Some(response), close),
                Err(error) => (Some(self.error_response(&request, &error)), close),
            },
            Method::Teardown => match self.teardown(&request) {
                Ok(response) => (Some(response), close),
                Err(error) => (Some(self.error_response(&request, &error)), close),
            },
            Method::GetParameter => match self.parameters(&request, true) {
                Ok(response) => (Some(response), close),
                Err(error) => (Some(self.error_response(&request, &error)), close),
            },
            Method::SetParameter => match self.parameters(&request, false) {
                Ok(response) => (Some(response), close),
                Err(error) => (Some(self.error_response(&request, &error)), close),
            },
            Method::Unknown(method) => {
                let mut response = self.response(&request, 501, "Not Implemented");
                response.headers.insert("Allow", public_methods());
                response.body = format!("method `{method}` is not implemented\r\n").into_bytes();
                response.headers.insert("Content-Type", "text/plain");
                (Some(response), close)
            }
        }
    }

    fn options(&self, request: &RtspRequest) -> RtspResponse {
        let mut response = self.response(request, 200, "OK");
        response.headers.insert("Public", public_methods());
        response
    }

    fn describe(&mut self, request: &RtspRequest) -> RtspResponse {
        let id = match request.session_id() {
            Some(id) if self.sessions.contains_key(id) => id.to_owned(),
            Some(_) => return self.error_response(request, &RtspError::UnknownSession),
            None => match self.current.clone() {
                Some(id) if self.sessions.contains_key(&id) => id,
                _ => self.new_session(),
            },
        };
        if let Some(session) = self.sessions.get_mut(&id) {
            if matches!(session.state, SessionState::Init) {
                session.state = SessionState::Described;
            }
            session.last_activity = Instant::now();
        }
        self.current = Some(id.clone());

        let sdp = h264_sdp(&SdpConfig {
            session_id: id.clone(),
            payload_type: self.config.payload_type,
            fps: Some(self.config.fps),
            track_control: "trackID=0".to_owned(),
            profile_level_id: Some("42C01F".to_owned()),
            connection_address: Some(self.local_addr.ip()),
        });
        let mut response = self.response(request, 200, "OK");
        response.headers.insert("Content-Type", "application/sdp");
        response
            .headers
            .insert("Content-Base", self.content_base(&request.uri));
        response
            .headers
            .insert("Content-Length", sdp.len().to_string());
        response.headers.insert("Session", self.session_header(&id));
        response.body = sdp.into_bytes();
        response
    }

    async fn setup(&mut self, request: &RtspRequest) -> Result<RtspResponse, RtspError> {
        let transport_value = request
            .transport()
            .ok_or_else(|| RtspError::MalformedRequest("missing Transport header".to_owned()))?;
        let requested = parse_transport(transport_value)?;

        let id = match request.session_id() {
            Some(id) if self.sessions.contains_key(id) => id.to_owned(),
            Some(_) => return Err(RtspError::UnknownSession),
            None => match self.current.clone() {
                Some(id) if self.sessions.contains_key(&id) => id,
                _ => self.new_session(),
            },
        };

        let transport = match requested {
            TransportRequest::Tcp {
                rtp_channel,
                rtcp_channel,
            } => SessionTransport::Tcp {
                rtp_channel,
                rtcp_channel,
            },
            TransportRequest::Udp {
                client_rtp,
                client_rtcp,
            } => {
                if !self.config.udp_enabled {
                    return Err(RtspError::UnsupportedTransport(
                        "UDP SETUP is disabled on this server".to_owned(),
                    ));
                }
                self.bind_udp(client_rtp, client_rtcp).await?
            }
        };

        let transport_header = match &transport {
            SessionTransport::Tcp {
                rtp_channel,
                rtcp_channel,
            } => format!("RTP/AVP/TCP;unicast;interleaved={rtp_channel}-{rtcp_channel}"),
            SessionTransport::Udp {
                client_rtp,
                client_rtcp,
                server_rtp,
                server_rtcp,
                ..
            } => format!(
                "RTP/AVP;unicast;client_port={}-{};server_port={}-{}",
                client_rtp.port(),
                client_rtcp.port(),
                server_rtp,
                server_rtcp
            ),
        };

        if let Some(session) = self.sessions.get_mut(&id) {
            session.stop_media();
            session.transport = Some(transport);
            session.state = SessionState::Ready;
            session.last_activity = Instant::now();
        }
        self.current = Some(id.clone());

        let mut response = self.response(request, 200, "OK");
        response.headers.insert("Transport", transport_header);
        response.headers.insert("Session", self.session_header(&id));
        Ok(response)
    }

    fn prepare_play(
        &mut self,
        request: &RtspRequest,
    ) -> Result<(RtspResponse, PlayPlan), RtspError> {
        let id = self.resolve_session(request)?;
        let transport = self
            .sessions
            .get(&id)
            .and_then(|session| session.transport.clone())
            .ok_or(RtspError::InvalidState("SETUP must complete before PLAY"))?;
        let factory = self.factory.clone().ok_or(RtspError::NoSource)?;
        let sink = match &transport {
            SessionTransport::Tcp { rtp_channel, .. } => MediaSink::Tcp {
                writer: self.writer.clone(),
                channel: *rtp_channel,
            },
            SessionTransport::Udp {
                rtp_socket,
                client_rtp,
                ..
            } => MediaSink::Udp {
                socket: rtp_socket.clone(),
                peer: *client_rtp,
            },
        };

        let packetizer = H264Packetizer::with_config(
            ssrc_for(&id),
            PacketizerConfig {
                mtu: self.config.mtu,
                payload_type: self.config.payload_type,
                repeat_parameter_sets: true,
                max_access_unit_bytes: self.config.max_access_unit_bytes,
            },
        )
        .map_err(RtspError::Rtp)?;
        let sequence = packetizer.next_sequence();
        let source = factory();

        if let Some(session) = self.sessions.get_mut(&id) {
            session.stop_media();
        }

        let mut response = self.response(request, 200, "OK");
        response
            .headers
            .insert("RTP-Info", format!("url={};seq={sequence}", request.uri));
        response.headers.insert("Session", self.session_header(&id));
        Ok((
            response,
            PlayPlan {
                session_id: id,
                source,
                sink,
                packetizer,
            },
        ))
    }

    fn start_play(&mut self, plan: PlayPlan) {
        let (cancel, cancel_rx) = watch::channel(false);
        let fps = self.config.fps;
        let task = tokio::spawn(run_media(
            plan.source,
            plan.sink,
            plan.packetizer,
            fps,
            cancel_rx,
        ));
        if let Some(session) = self.sessions.get_mut(&plan.session_id) {
            session.stop_media();
            session.media = Some(MediaHandle { cancel, task });
            session.state = SessionState::Playing;
            session.last_activity = Instant::now();
        }
        self.current = Some(plan.session_id);
    }

    fn pause(&mut self, request: &RtspRequest) -> Result<RtspResponse, RtspError> {
        let id = self.resolve_session(request)?;
        let session = self
            .sessions
            .get_mut(&id)
            .ok_or(RtspError::UnknownSession)?;
        if session.state != SessionState::Playing {
            return Err(RtspError::InvalidState("session is not playing"));
        }
        session.stop_media();
        session.state = SessionState::Ready;
        session.last_activity = Instant::now();
        let mut response = self.response(request, 200, "OK");
        response.headers.insert("Session", self.session_header(&id));
        Ok(response)
    }

    fn teardown(&mut self, request: &RtspRequest) -> Result<RtspResponse, RtspError> {
        let id = match request.session_id() {
            Some(id) => {
                if !self.sessions.contains_key(id) {
                    return Err(RtspError::UnknownSession);
                }
                Some(id.to_owned())
            }
            None => self.resolve_session(request).ok(),
        };
        if let Some(id) = &id {
            self.sessions.remove(id);
            if self.current.as_deref() == Some(id.as_str()) {
                self.current = None;
            }
        }
        let mut response = self.response(request, 200, "OK");
        if let Some(id) = id {
            response.headers.insert("Session", self.session_header(&id));
        }
        Ok(response)
    }

    fn parameters(
        &mut self,
        request: &RtspRequest,
        echo_body: bool,
    ) -> Result<RtspResponse, RtspError> {
        let id = match request.session_id() {
            Some(id) => {
                if !self.sessions.contains_key(id) {
                    return Err(RtspError::UnknownSession);
                }
                if let Some(session) = self.sessions.get_mut(id) {
                    session.last_activity = Instant::now();
                }
                Some(id.to_owned())
            }
            None => self.resolve_session(request).ok(),
        };
        let mut response = self.response(request, 200, "OK");
        if echo_body && !request.body.is_empty() {
            response.headers.insert("Content-Type", "text/parameters");
            response.body = request.body.clone();
        }
        if let Some(id) = id {
            response.headers.insert("Session", self.session_header(&id));
        }
        Ok(response)
    }

    fn new_session(&mut self) -> String {
        let id = self.state.session_id();
        self.sessions
            .insert(id.clone(), ActiveSession::new(SessionState::Init));
        self.current = Some(id.clone());
        id
    }

    fn resolve_session(&self, request: &RtspRequest) -> Result<String, RtspError> {
        if let Some(id) = request.session_id() {
            if self.sessions.contains_key(id) {
                return Ok(id.to_owned());
            }
            return Err(RtspError::UnknownSession);
        }
        if let Some(current) = &self.current {
            if self.sessions.contains_key(current) {
                return Ok(current.clone());
            }
        }
        let mut keys = self.sessions.keys();
        match (keys.next(), keys.next()) {
            (Some(id), None) => Ok(id.clone()),
            _ => Err(RtspError::UnknownSession),
        }
    }

    fn prune_sessions(&mut self) {
        let timeout = self.config.session_timeout;
        self.sessions
            .retain(|_, session| session.last_activity.elapsed() < timeout);
        if let Some(current) = &self.current {
            if !self.sessions.contains_key(current) {
                self.current = None;
            }
        }
    }

    fn response(&self, request: &RtspRequest, status: u16, reason: &str) -> RtspResponse {
        let mut response = RtspResponse::new(status, reason);
        if let Some(cseq) = request.cseq() {
            response.headers.insert("CSeq", cseq);
        }
        response
            .headers
            .insert("Server", self.config.server_name.clone());
        response
    }

    fn error_response(&self, request: &RtspRequest, error: &RtspError) -> RtspResponse {
        let (status, reason) = match error {
            RtspError::MalformedRequest(_) => (400, "Bad Request"),
            RtspError::UnsupportedTransport(_) => (461, "Unsupported Transport"),
            RtspError::UnknownSession => (454, "Session Not Found"),
            RtspError::InvalidState(_) => (455, "Method Not Valid in This State"),
            RtspError::NoSource => (503, "Service Unavailable"),
            RtspError::Io(_) | RtspError::Rtp(_) => (500, "Internal Server Error"),
        };
        let mut response = self.response(request, status, reason);
        response.headers.insert("Content-Type", "text/plain");
        response.body = format!("{error}\r\n").into_bytes();
        response
    }

    fn error_text(&self, status: u16, reason: &str, message: &str) -> RtspResponse {
        let mut response = RtspResponse::new(status, reason);
        response
            .headers
            .insert("Server", self.config.server_name.clone());
        response.headers.insert("Content-Type", "text/plain");
        response.body = format!("{message}\r\n").into_bytes();
        response
    }

    async fn send(&self, response: RtspResponse) -> bool {
        let bytes = response.encode();
        let mut writer = self.writer.lock().await;
        if let Err(error) = writer.write_all(&bytes).await {
            debug!(%error, "RTSP write failed");
            return false;
        }
        if let Err(error) = writer.flush().await {
            debug!(%error, "RTSP flush failed");
            return false;
        }
        true
    }

    fn session_header(&self, id: &str) -> String {
        let timeout = self.config.session_timeout.as_secs().max(1);
        format!("{id};timeout={timeout}")
    }

    fn content_base(&self, uri: &str) -> String {
        if uri.starts_with("rtsp://") || uri.starts_with("rtsps://") {
            if uri.ends_with('/') {
                uri.to_owned()
            } else {
                format!("{uri}/")
            }
        } else {
            format!("rtsp://{}/", self.local_addr)
        }
    }
}

fn public_methods() -> &'static str {
    "OPTIONS, DESCRIBE, SETUP, PLAY, PAUSE, TEARDOWN, GET_PARAMETER, SET_PARAMETER"
}

fn ssrc_for(session_id: &str) -> u32 {
    let mut hash = 0x811c_9dc5u32;
    for byte in session_id.bytes() {
        hash ^= u32::from(byte);
        hash = hash.wrapping_mul(0x0100_0193);
    }
    hash
}

impl Connection {
    async fn bind_udp(
        &self,
        client_rtp: u16,
        client_rtcp: u16,
    ) -> Result<SessionTransport, RtspError> {
        let bind = if self.peer.is_ipv4() {
            SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0)
        } else {
            SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 0)
        };
        let rtp_socket = UdpSocket::bind(bind).await?;
        let rtcp_socket = UdpSocket::bind(bind).await?;
        let server_rtp = rtp_socket.local_addr()?.port();
        let server_rtcp = rtcp_socket.local_addr()?.port();
        Ok(SessionTransport::Udp {
            rtp_socket: Arc::new(rtp_socket),
            _rtcp_socket: Arc::new(rtcp_socket),
            client_rtp: SocketAddr::new(self.peer.ip(), client_rtp),
            client_rtcp: SocketAddr::new(self.peer.ip(), client_rtcp),
            server_rtp,
            server_rtcp,
        })
    }
}

struct MediaStats {
    frames: u64,
    packets: u64,
    bytes: u64,
    keyframes: u64,
    errors: u64,
}

struct MediaTimestamp {
    clock: RtpTimestampClock,
    step: u32,
    last: Option<u32>,
}

impl MediaTimestamp {
    fn new(fps: u32) -> Self {
        Self {
            clock: RtpTimestampClock::default(),
            step: 90_000 / fps.max(1),
            last: None,
        }
    }

    fn next(&mut self, frame: &EncodedFrame) -> u32 {
        let candidate = if frame.pts_micros() > 0 {
            self.clock.timestamp(frame.pts_micros())
        } else {
            (frame.sequence() as u32).wrapping_mul(self.step.max(1))
        };
        let timestamp = match self.last {
            Some(last) if candidate <= last => last.wrapping_add(self.step.max(1)),
            _ => candidate,
        };
        self.last = Some(timestamp);
        timestamp
    }
}

async fn run_media(
    mut source: Box<dyn EncodedVideoSource>,
    sink: MediaSink,
    mut packetizer: H264Packetizer,
    fps: u32,
    mut cancel: watch::Receiver<bool>,
) {
    let mut timestamps = MediaTimestamp::new(fps);
    let mut stats = MediaStats {
        frames: 0,
        packets: 0,
        bytes: 0,
        keyframes: 0,
        errors: 0,
    };
    let mut warned_codec = false;

    loop {
        let frame = tokio::select! {
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    break;
                }
                continue;
            }
            frame = source.next_frame() => match frame {
                Some(frame) => frame,
                None => break,
            },
        };

        if frame.format().codec != Codec::H264 {
            if !warned_codec {
                warn!(
                    codec = ?frame.format().codec,
                    "RTSP source produced a non-H.264 frame; skipping"
                );
                warned_codec = true;
            }
            continue;
        }

        let timestamp = timestamps.next(&frame);
        let keyframe = frame.is_keyframe();
        match packetizer.packetize(frame.data(), timestamp, keyframe) {
            Ok(packets) => {
                for packet in &packets {
                    if sink.send(packet).await.is_err() {
                        debug!("RTSP media send failed; stopping session playback");
                        return;
                    }
                    stats.packets = stats.packets.saturating_add(1);
                    stats.bytes = stats.bytes.saturating_add(packet.len() as u64);
                }
                stats.frames = stats.frames.saturating_add(1);
                if keyframe {
                    stats.keyframes = stats.keyframes.saturating_add(1);
                }
            }
            Err(error) => {
                stats.errors = stats.errors.saturating_add(1);
                warn!(%error, "failed to packetize an access unit");
            }
        }
    }

    debug!(
        frames = stats.frames,
        packets = stats.packets,
        bytes = stats.bytes,
        keyframes = stats.keyframes,
        errors = stats.errors,
        "RTSP media task finished"
    );
}

async fn read_request(
    reader: &mut BufReader<OwnedReadHalf>,
    max_header_bytes: usize,
    max_body_bytes: usize,
) -> io::Result<ReadOutcome> {
    loop {
        let mut first = [0u8; 1];
        match reader.read_exact(&mut first).await {
            Ok(_) => {}
            Err(error) if error.kind() == io::ErrorKind::UnexpectedEof => {
                return Ok(ReadOutcome::Eof)
            }
            Err(error) => return Err(error),
        }

        match first[0] {
            b'$' => {
                let mut interleaved = [0u8; 3];
                reader.read_exact(&mut interleaved).await?;
                let length = u16::from_be_bytes([interleaved[1], interleaved[2]]) as usize;
                let mut buffer = [0u8; 4096];
                let mut remaining = length;
                while remaining > 0 {
                    let chunk = remaining.min(buffer.len());
                    reader.read_exact(&mut buffer[..chunk]).await?;
                    remaining -= chunk;
                }
                continue;
            }
            b'\r' | b'\n' => continue,
            byte if byte >= 0x80 => {
                return Ok(ReadOutcome::Malformed(
                    "non-ASCII byte at start of request".to_owned(),
                ));
            }
            _ => {}
        }

        let mut head = String::new();
        let mut line = vec![first[0]];
        match read_limited_line(reader, max_header_bytes, &mut line).await {
            Ok(true) => {}
            Ok(false) => return Ok(ReadOutcome::Eof),
            Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                return Ok(ReadOutcome::Fatal(
                    "request line exceeds the configured header limit".to_owned(),
                ));
            }
            Err(error) => return Err(error),
        }
        head.push_str(&String::from_utf8_lossy(&line));
        if head.len() > max_header_bytes {
            return Ok(ReadOutcome::Fatal(
                "request headers exceed the configured limit".to_owned(),
            ));
        }

        let mut header_bytes = head.len();
        let mut header_count = 0usize;
        loop {
            let mut line = Vec::new();
            match read_limited_line(reader, max_header_bytes, &mut line).await {
                Ok(true) => {}
                Ok(false) => return Ok(ReadOutcome::Eof),
                Err(error) if error.kind() == io::ErrorKind::InvalidData => {
                    return Ok(ReadOutcome::Fatal(
                        "request headers exceed the configured limit".to_owned(),
                    ));
                }
                Err(error) => return Err(error),
            }
            header_bytes = header_bytes.saturating_add(line.len());
            if header_bytes > max_header_bytes {
                return Ok(ReadOutcome::Fatal(
                    "request headers exceed the configured limit".to_owned(),
                ));
            }
            let line = String::from_utf8_lossy(&line);
            if line == "\r\n" || line == "\n" {
                break;
            }
            header_count += 1;
            if header_count > 128 {
                return Ok(ReadOutcome::Fatal("too many request headers".to_owned()));
            }
            head.push_str(&line);
        }

        let headers = match parse_head(&head) {
            Ok((_, _, _, headers)) => headers,
            Err(error) => return Ok(ReadOutcome::Malformed(error.to_string())),
        };
        let body_length = match headers.content_length() {
            Ok(Some(length)) => length,
            Ok(None) => 0,
            Err(error) => return Ok(ReadOutcome::Fatal(error.to_string())),
        };
        if body_length > max_body_bytes {
            return Ok(ReadOutcome::Fatal(
                "request body exceeds the configured limit".to_owned(),
            ));
        }
        let mut body = vec![0u8; body_length];
        if body_length > 0 {
            reader.read_exact(&mut body).await?;
        }
        return match request_from_head(&head, &body) {
            Ok(request) => Ok(ReadOutcome::Request(request)),
            Err(error) => Ok(ReadOutcome::Malformed(error.to_string())),
        };
    }
}

/// Read one newline-terminated line, appending it to `out`.
///
/// Returns `false` on EOF before any byte. A line (or the accumulated `out`)
/// larger than `max` is an `InvalidData` error, so a client cannot exhaust
/// memory by sending an unbounded header line.
async fn read_limited_line(
    reader: &mut BufReader<OwnedReadHalf>,
    max: usize,
    out: &mut Vec<u8>,
) -> io::Result<bool> {
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(false);
        }
        if let Some(position) = available.iter().position(|byte| *byte == b'\n') {
            let length = position + 1;
            if out.len().saturating_add(length) > max {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "line exceeds limit",
                ));
            }
            out.extend_from_slice(&available[..length]);
            reader.consume(length);
            return Ok(true);
        }
        let length = available.len();
        if out.len().saturating_add(length) > max {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "line exceeds limit",
            ));
        }
        out.extend_from_slice(available);
        reader.consume(length);
    }
}
