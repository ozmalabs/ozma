// SPDX-License-Identifier: AGPL-3.0-only
//! Native QUIC adapter.
//!
//! One QUIC connection carries a session:
//! - a reliable, ordered bidirectional stream for `control`, `input`,
//!   `telemetry`, and `bulk` channels;
//! - RFC 9221 unreliable datagrams for `video`, `audio`, and `mic` channels.
//!
//! Stream frames are `[channel: u8][length: u32 BE][payload]`. Datagrams are
//! `[channel: u8][payload]`; the channel must be marked unreliable by the
//! negotiated [`ChannelMap`]. TLS 1.3 with ALPN `ozma/1` is mandatory.
//!
//! This is an adapter, not a WebTransport server: it uses raw QUIC with
//! `quinn`, so a browser gateway (h3/WebTransport) can be layered on later
//! without changing the channel contract.

use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use bytes::Bytes;
use ozma_protocol::{codes, ApiError, Result, TransportMode};
use quinn::crypto::rustls::{QuicClientConfig, QuicServerConfig};
use quinn::{
    Connection, ConnectionError, Endpoint, IdleTimeout, ReadError, ReadExactError, RecvStream,
    SendDatagramError, ServerConfig, TransportConfig, VarInt,
};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio::time::timeout;

use crate::channel::{ChannelId, ChannelMap};
use crate::error::{channel_unknown, payload_too_large, protocol_error, transport_unavailable};
use crate::event::{CloseReason, TransportEvent};
use crate::metrics::{Metrics, TransportMetrics};
use crate::transport::SessionTransport;

/// ALPN for native Ozma QUIC sessions.
pub const ALPN: &str = "ozma/1";

/// Maximum reliable-stream frame accepted from a peer. Larger frames are a
/// protocol error so a malicious peer cannot make us allocate unbounded memory.
const MAX_FRAME_LEN: usize = 8 * 1024 * 1024;

/// QUIC configuration shared by server and client.
#[derive(Clone, Debug)]
pub struct QuicConfig {
    /// ALPN protocols to offer/accept. Defaults to [`ALPN`].
    pub alpn: Vec<Vec<u8>>,
    /// DNS names placed in the generated self-signed certificate.
    pub certificate_names: Vec<String>,
    /// Timeout for the handshake and for opening the control stream.
    pub handshake_timeout: Duration,
    /// QUIC idle timeout.
    pub idle_timeout: Duration,
    /// Optional keep-alive interval.
    pub keep_alive_interval: Option<Duration>,
    /// Bounded receive queue between the reader tasks and `recv`.
    pub event_queue: usize,
}

impl Default for QuicConfig {
    fn default() -> Self {
        Self {
            alpn: vec![ALPN.as_bytes().to_vec()],
            certificate_names: vec!["localhost".to_string()],
            handshake_timeout: Duration::from_secs(5),
            idle_timeout: Duration::from_secs(30),
            keep_alive_interval: None,
            event_queue: 64,
        }
    }
}

impl QuicConfig {
    /// A configuration using a specific ALPN.
    pub fn with_alpn(mut self, alpn: impl Into<Vec<u8>>) -> Self {
        self.alpn = vec![alpn.into()];
        self
    }

    /// Set the handshake timeout.
    pub fn with_handshake_timeout(mut self, duration: Duration) -> Self {
        self.handshake_timeout = duration;
        self
    }

    /// Set the idle timeout.
    pub fn with_idle_timeout(mut self, duration: Duration) -> Self {
        self.idle_timeout = duration;
        self
    }
}

/// A generated self-signed certificate and key, for tests and local bring-up.
pub struct SelfSignedCertificate {
    certificate: CertificateDer<'static>,
    key: PrivateKeyDer<'static>,
}

impl Clone for SelfSignedCertificate {
    fn clone(&self) -> Self {
        Self {
            certificate: self.certificate.clone(),
            key: self.key.clone_key(),
        }
    }
}

impl fmt::Debug for SelfSignedCertificate {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SelfSignedCertificate")
            .field("certificate_len", &self.certificate.as_ref().len())
            .field("key", &"<redacted>")
            .finish()
    }
}

impl SelfSignedCertificate {
    /// Generate a certificate for the supplied DNS names (default:
    /// `localhost`).
    pub fn generate(server_names: &[String]) -> Result<Self> {
        let names = if server_names.is_empty() {
            vec!["localhost".to_string()]
        } else {
            server_names.to_vec()
        };
        let generated = rcgen::generate_simple_self_signed(names).map_err(|error| {
            ApiError::internal(format!("failed to generate QUIC test certificate: {error}"))
        })?;
        Ok(Self {
            certificate: generated.cert.der().clone(),
            key: PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(generated.key_pair.serialize_der())),
        })
    }

    /// The DER-encoded certificate, suitable for a client trust store or a
    /// fingerprint pin.
    pub fn certificate_der(&self) -> &[u8] {
        self.certificate.as_ref()
    }

    fn key(&self) -> PrivateKeyDer<'static> {
        self.key.clone_key()
    }
}

enum Role {
    Server,
    Client,
}

struct QuicInner {
    conn: Connection,
    send: Mutex<quinn::SendStream>,
    closed: AtomicBool,
    metrics: Arc<Metrics>,
}

/// A live QUIC session transport.
///
/// Use [`QuicServer::accept`] on the node side or [`QuicClient::connect`] on the
/// client side. The transport implements [`SessionTransport`], so callers do
/// not need to know whether bytes came from a stream or a datagram.
pub struct QuicTransport {
    inner: Arc<QuicInner>,
    rx: mpsc::Receiver<TransportEvent>,
    map: ChannelMap,
    endpoint: Option<Endpoint>,
    owns_endpoint: bool,
    recv_closed: Option<CloseReason>,
    control_reader: JoinHandle<()>,
    datagram_reader: JoinHandle<()>,
}

impl fmt::Debug for QuicTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicTransport")
            .field("remote_address", &self.inner.conn.remote_address())
            .field("channels", &self.map.len())
            .field("closed", &self.is_closed())
            .finish()
    }
}

impl QuicTransport {
    /// The connected peer address.
    pub fn remote_address(&self) -> SocketAddr {
        self.inner.conn.remote_address()
    }

    /// Transfer one bounded resource over a dedicated stream so a stalled
    /// resource does not block the reliable control stream.
    pub async fn send_resource(&self, payload: Bytes) -> Result<()> {
        self.map.check_payload(ChannelId::BULK, payload.len())?;
        let mut stream = self
            .inner
            .conn
            .open_uni()
            .await
            .map_err(resource_stream_error)?;
        stream
            .write_all(&payload)
            .await
            .map_err(resource_stream_error)?;
        stream.finish().map_err(resource_stream_error)?;
        self.inner.metrics.record_sent(payload.len(), false);
        Ok(())
    }

    /// Receive one resource. Call this only after negotiating the scene
    /// profile. Callers can receive resources and control messages concurrently.
    pub async fn recv_resource(&self) -> Result<Bytes> {
        let limit = self.map.require(ChannelId::BULK)?.max_payload as usize;
        read_resource(&self.inner, limit).await
    }

    /// Start a bounded resource reader after negotiating the scene profile.
    /// The task owns each stream through completion, independently of polls
    /// of the control and media event queue.
    pub fn resource_reader(&self) -> Result<mpsc::Receiver<Result<Bytes>>> {
        let limit = self.map.require(ChannelId::BULK)?.max_payload as usize;
        let inner = self.inner.clone();
        let (tx, rx) = mpsc::channel(4);
        tokio::spawn(async move {
            loop {
                tokio::select! {
                    _ = tx.closed() => break,
                    payload = read_resource(&inner, limit) => {
                        let failed = payload.is_err();
                        if tx.send(payload).await.is_err() || failed {
                            break;
                        }
                    }
                }
            }
        });
        Ok(rx)
    }

    async fn start(
        conn: Connection,
        map: ChannelMap,
        config: &QuicConfig,
        endpoint: Option<Endpoint>,
        owns_endpoint: bool,
        role: Role,
    ) -> Result<Self> {
        let opened = match role {
            Role::Server => timeout(config.handshake_timeout, conn.accept_bi()).await,
            Role::Client => timeout(config.handshake_timeout, conn.open_bi()).await,
        };
        let (send, recv) = match opened {
            Ok(Ok(streams)) => streams,
            Ok(Err(error)) => {
                let api = transport_unavailable(format!("QUIC control stream failed: {error}"));
                conn.close(VarInt::from_u32(1), b"control stream failed");
                return Err(api);
            }
            Err(_) => {
                conn.close(VarInt::from_u32(1), b"control stream timeout");
                return Err(timeout_error(
                    "opening the QUIC control stream",
                    config.handshake_timeout,
                ));
            }
        };

        let metrics = Arc::new(Metrics::default());
        let closed_once = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::channel(config.event_queue.max(1));

        let control_reader = tokio::spawn(read_control_stream(
            recv,
            tx.clone(),
            map.clone(),
            metrics.clone(),
            closed_once.clone(),
            conn.clone(),
        ));
        let datagram_reader = tokio::spawn(read_datagrams(
            conn.clone(),
            tx,
            map.clone(),
            metrics.clone(),
            closed_once,
        ));

        Ok(Self {
            inner: Arc::new(QuicInner {
                conn,
                send: Mutex::new(send),
                closed: AtomicBool::new(false),
                metrics,
            }),
            rx,
            map,
            endpoint,
            owns_endpoint,
            recv_closed: None,
            control_reader,
            datagram_reader,
        })
    }
}

#[async_trait]
impl SessionTransport for QuicTransport {
    fn mode(&self) -> TransportMode {
        TransportMode::Quic
    }

    fn channel_map(&self) -> &ChannelMap {
        &self.map
    }

    async fn send(&self, channel: ChannelId, payload: Bytes) -> Result<()> {
        if self.inner.closed.load(Ordering::SeqCst) {
            self.inner.metrics.record_send_error();
            return Err(crate::error::channel_closed(
                Some(channel),
                CloseReason::LocalShutdown,
            ));
        }
        let descriptor = self.map.require(channel)?;
        self.map.check_payload(channel, payload.len())?;

        if descriptor.reliable {
            let mut frame = Vec::with_capacity(5 + payload.len());
            frame.push(channel.raw());
            frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
            frame.extend_from_slice(&payload);
            let mut send = self.inner.send.lock().await;
            if let Err(error) = send.write_all(&frame).await {
                self.inner.metrics.record_send_error();
                self.inner.metrics.mark_closed();
                return Err(transport_unavailable(format!(
                    "QUIC stream send failed: {error}"
                )));
            }
            self.inner.metrics.record_sent(payload.len(), false);
            return Ok(());
        }

        let max = self.inner.conn.max_datagram_size().ok_or_else(|| {
            ApiError::new(codes::UNSUPPORTED, "peer does not support QUIC datagrams")
                .with_remediation(
                    "Advertise RFC 9221 datagram support or use a reliable transport.",
                )
        })?;
        let capacity = max.saturating_sub(1);
        if payload.len() > capacity {
            self.inner.metrics.record_send_error();
            return Err(payload_too_large(channel, payload.len(), capacity));
        }
        let mut datagram = Vec::with_capacity(1 + payload.len());
        datagram.push(channel.raw());
        datagram.extend_from_slice(&payload);
        match self.inner.conn.send_datagram(Bytes::from(datagram)) {
            Ok(()) => {
                self.inner.metrics.record_sent(payload.len(), true);
                Ok(())
            }
            Err(SendDatagramError::TooLarge) => {
                self.inner.metrics.record_send_error();
                Err(payload_too_large(channel, payload.len(), capacity))
            }
            Err(SendDatagramError::UnsupportedByPeer | SendDatagramError::Disabled) => {
                self.inner.metrics.record_send_error();
                Err(ApiError::new(
                    codes::UNSUPPORTED,
                    "QUIC datagrams are not available on this connection",
                ))
            }
            Err(SendDatagramError::ConnectionLost(error)) => {
                self.inner.metrics.record_send_error();
                self.inner.metrics.mark_closed();
                Err(transport_unavailable(format!(
                    "QUIC datagram send failed: {error}"
                )))
            }
        }
    }

    async fn recv(&mut self) -> TransportEvent {
        if let Some(reason) = self.recv_closed {
            return TransportEvent::Closed { reason };
        }
        match self.rx.recv().await {
            Some(event) => {
                if let TransportEvent::Closed { reason } = event {
                    self.recv_closed = Some(reason);
                    self.inner.metrics.mark_closed();
                }
                event
            }
            None => {
                let reason = CloseReason::TransportError;
                self.recv_closed = Some(reason);
                self.inner.metrics.mark_closed();
                TransportEvent::Closed { reason }
            }
        }
    }

    async fn close_with_reason(&self, reason: CloseReason) -> Result<()> {
        if self.inner.closed.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        self.inner.metrics.mark_closed();
        self.inner
            .conn
            .close(VarInt::from_u32(0), reason.as_str().as_bytes());
        if self.owns_endpoint {
            if let Some(endpoint) = &self.endpoint {
                endpoint.close(VarInt::from_u32(0), b"session closed");
            }
        }
        Ok(())
    }

    fn metrics(&self) -> TransportMetrics {
        let mut snapshot = self.inner.metrics.snapshot(TransportMode::Quic);
        snapshot.closed |= self.recv_closed.is_some();
        snapshot
    }
}

impl Drop for QuicTransport {
    fn drop(&mut self) {
        self.control_reader.abort();
        self.datagram_reader.abort();
        if !self.inner.closed.swap(true, Ordering::SeqCst) {
            self.inner.metrics.mark_closed();
            self.inner
                .conn
                .close(VarInt::from_u32(0), b"transport dropped");
        }
    }
}

/// A QUIC listener that accepts one session connection at a time.
pub struct QuicServer {
    endpoint: Endpoint,
    certificate: CertificateDer<'static>,
    config: QuicConfig,
    channels: ChannelMap,
}

impl fmt::Debug for QuicServer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("QuicServer")
            .field("local_addr", &self.local_addr().ok())
            .field("channels", &self.channels.len())
            .finish()
    }
}

impl QuicServer {
    /// Bind on `addr`, generating a self-signed `localhost` certificate and
    /// using the default session channel map.
    ///
    /// Must be called from within a Tokio runtime. Use port 0 in tests.
    pub fn bind(addr: SocketAddr) -> Result<Self> {
        Self::bind_with_config(addr, QuicConfig::default(), ChannelMap::default_session())
    }

    /// Bind with explicit configuration and channels.
    pub fn bind_with_config(
        addr: SocketAddr,
        config: QuicConfig,
        channels: ChannelMap,
    ) -> Result<Self> {
        let certificate = SelfSignedCertificate::generate(&config.certificate_names)?;
        let server_config = build_server_config(&config, &certificate)?;
        let endpoint = Endpoint::server(server_config, addr).map_err(|error| {
            ApiError::internal(format!("failed to bind QUIC endpoint on {addr}: {error}"))
        })?;
        Ok(Self {
            endpoint,
            certificate: certificate.certificate,
            config,
            channels,
        })
    }

    /// The bound address, including the actual port when port 0 was requested.
    pub fn local_addr(&self) -> Result<SocketAddr> {
        self.endpoint.local_addr().map_err(|error| {
            ApiError::internal(format!("QUIC endpoint has no local address: {error}"))
        })
    }

    /// DER certificate clients should trust or pin.
    pub fn certificate_der(&self) -> &[u8] {
        self.certificate.as_ref()
    }

    /// The server's channel map (for diagnostics).
    pub fn channel_map(&self) -> &ChannelMap {
        &self.channels
    }

    /// Wait for and handshake one connection, then open the control stream.
    ///
    /// The returned transport must be kept alive for the lifetime of the
    /// session.
    pub async fn accept(&self) -> Result<QuicTransport> {
        let incoming = timeout(self.config.handshake_timeout, self.endpoint.accept())
            .await
            .map_err(|_| {
                timeout_error("accepting a QUIC connection", self.config.handshake_timeout)
            })?
            .ok_or_else(|| transport_unavailable("QUIC endpoint is closed"))?;
        let remote = incoming.remote_address();
        let conn = timeout(self.config.handshake_timeout, incoming)
            .await
            .map_err(|_| {
                timeout_error(
                    "completing the QUIC handshake",
                    self.config.handshake_timeout,
                )
            })?
            .map_err(|error| {
                transport_unavailable(format!("QUIC handshake with {remote} failed: {error}"))
            })?;
        tracing::debug!(%remote, "accepted QUIC session connection");
        QuicTransport::start(
            conn,
            self.channels.clone(),
            &self.config,
            Some(self.endpoint.clone()),
            false,
            Role::Server,
        )
        .await
    }

    /// Stop accepting new connections.
    pub fn close(&self) {
        self.endpoint.close(VarInt::from_u32(0), b"server closed");
    }
}

/// Client entry points for native QUIC sessions.
#[derive(Clone, Copy, Debug, Default)]
pub struct QuicClient;

impl QuicClient {
    /// Connect to a server, trusting the supplied DER certificate.
    pub async fn connect(
        addr: SocketAddr,
        server_name: &str,
        certificate_der: impl AsRef<[u8]>,
        channels: ChannelMap,
    ) -> Result<QuicTransport> {
        Self::connect_with_config(
            addr,
            server_name,
            certificate_der,
            QuicConfig::default(),
            channels,
        )
        .await
    }

    /// Connect with an explicit configuration.
    pub async fn connect_with_config(
        addr: SocketAddr,
        server_name: &str,
        certificate_der: impl AsRef<[u8]>,
        config: QuicConfig,
        channels: ChannelMap,
    ) -> Result<QuicTransport> {
        let client_config = build_client_config(&config, certificate_der.as_ref())?;
        let bind: SocketAddr = if addr.is_ipv6() {
            SocketAddr::from(([0u16; 8], 0))
        } else {
            SocketAddr::from(([0u8; 4], 0))
        };
        let mut endpoint = Endpoint::client(bind).map_err(|error| {
            ApiError::internal(format!("failed to create QUIC client endpoint: {error}"))
        })?;
        endpoint.set_default_client_config(client_config);

        let connecting = endpoint.connect(addr, server_name).map_err(|error| {
            transport_unavailable(format!("could not start QUIC connect: {error}"))
        })?;
        let conn = timeout(config.handshake_timeout, connecting)
            .await
            .map_err(|_| timeout_error("connecting over QUIC", config.handshake_timeout))?
            .map_err(|error| {
                transport_unavailable(format!("QUIC handshake with {addr} failed: {error}"))
            })?;
        tracing::debug!(%addr, "connected QUIC session");
        QuicTransport::start(conn, channels, &config, Some(endpoint), true, Role::Client).await
    }
}

fn build_server_config(
    config: &QuicConfig,
    certificate: &SelfSignedCertificate,
) -> Result<ServerConfig> {
    let mut crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![certificate.certificate.clone()], certificate.key())
        .map_err(|error| ApiError::internal(format!("invalid QUIC server certificate: {error}")))?;
    crypto.alpn_protocols = config.alpn.clone();
    let crypto = QuicServerConfig::try_from(crypto)
        .map_err(|error| ApiError::internal(format!("invalid QUIC server TLS config: {error}")))?;
    let mut server_config = ServerConfig::with_crypto(Arc::new(crypto));
    server_config.transport_config(Arc::new(transport_config(config)?));
    Ok(server_config)
}

fn build_client_config(config: &QuicConfig, certificate_der: &[u8]) -> Result<quinn::ClientConfig> {
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(certificate_der.to_vec()))
        .map_err(|error| ApiError::internal(format!("invalid pinned QUIC certificate: {error}")))?;
    let mut crypto = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    crypto.alpn_protocols = config.alpn.clone();
    let crypto = QuicClientConfig::try_from(crypto)
        .map_err(|error| ApiError::internal(format!("invalid QUIC client TLS config: {error}")))?;
    let mut client_config = quinn::ClientConfig::new(Arc::new(crypto));
    client_config.transport_config(Arc::new(transport_config(config)?));
    Ok(client_config)
}

fn transport_config(config: &QuicConfig) -> Result<TransportConfig> {
    let mut transport = TransportConfig::default();
    let idle = IdleTimeout::try_from(config.idle_timeout)
        .map_err(|_| ApiError::invalid("QUIC idle timeout is too large for the varint encoding"))?;
    transport.max_idle_timeout(Some(idle));
    transport.keep_alive_interval(config.keep_alive_interval);
    // The control stream plus a little headroom for future channels.
    transport.max_concurrent_bidi_streams(8u8.into());
    transport.max_concurrent_uni_streams(4u8.into());
    Ok(transport)
}

async fn read_resource(inner: &QuicInner, limit: usize) -> Result<Bytes> {
    let mut stream = inner
        .conn
        .accept_uni()
        .await
        .map_err(resource_stream_error)?;
    let payload = stream
        .read_to_end(limit)
        .await
        .map_err(resource_stream_error)?;
    inner.metrics.record_received(payload.len(), false);
    Ok(Bytes::from(payload))
}

fn resource_stream_error(error: impl std::fmt::Display) -> ApiError {
    transport_unavailable(format!("QUIC resource stream failed: {error}"))
}

fn timeout_error(what: &str, duration: Duration) -> ApiError {
    transport_unavailable(format!(
        "timed out {what} after {} ms",
        duration.as_millis()
    ))
}

async fn read_control_stream(
    mut recv: RecvStream,
    tx: mpsc::Sender<TransportEvent>,
    map: ChannelMap,
    metrics: Arc<Metrics>,
    closed_once: Arc<AtomicBool>,
    conn: Connection,
) {
    loop {
        let mut header = [0u8; 5];
        match recv.read_exact(&mut header).await {
            Ok(()) => {}
            Err(error) => {
                let reason = reason_for_read_exact(&error, &conn);
                if reason == CloseReason::ProtocolError {
                    emit_error(
                        &tx,
                        &metrics,
                        protocol_error(format!("malformed QUIC control frame: {error}")),
                    )
                    .await;
                }
                finish_closed(&tx, &closed_once, &metrics, reason).await;
                return;
            }
        }

        let channel = ChannelId::new(header[0]);
        let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
        if len > MAX_FRAME_LEN {
            emit_error(
                &tx,
                &metrics,
                protocol_error(format!("QUIC control frame of {len} bytes is too large")),
            )
            .await;
            finish_closed(&tx, &closed_once, &metrics, CloseReason::ProtocolError).await;
            return;
        }
        let mut payload = vec![0u8; len];
        if let Err(error) = recv.read_exact(&mut payload).await {
            let reason = reason_for_read_exact(&error, &conn);
            if reason == CloseReason::ProtocolError {
                emit_error(
                    &tx,
                    &metrics,
                    protocol_error(format!("truncated QUIC control frame: {error}")),
                )
                .await;
            }
            finish_closed(&tx, &closed_once, &metrics, reason).await;
            return;
        }

        let Some(descriptor) = map.get(channel) else {
            emit_error(&tx, &metrics, channel_unknown(channel)).await;
            continue;
        };
        if !descriptor.reliable {
            emit_error(
                &tx,
                &metrics,
                protocol_error(format!(
                    "channel {channel} is unreliable but arrived on the control stream"
                )),
            )
            .await;
            continue;
        }
        if len > descriptor.max_payload as usize {
            emit_error(
                &tx,
                &metrics,
                payload_too_large(channel, len, descriptor.max_payload as usize),
            )
            .await;
            continue;
        }

        metrics.record_received(len, false);
        if tx
            .send(TransportEvent::Payload {
                channel,
                payload: Bytes::from(payload),
            })
            .await
            .is_err()
        {
            return;
        }
    }
}

async fn read_datagrams(
    conn: Connection,
    tx: mpsc::Sender<TransportEvent>,
    map: ChannelMap,
    metrics: Arc<Metrics>,
    closed_once: Arc<AtomicBool>,
) {
    loop {
        match conn.read_datagram().await {
            Ok(datagram) => {
                if datagram.is_empty() {
                    emit_error(&tx, &metrics, protocol_error("empty QUIC datagram")).await;
                    continue;
                }
                let channel = ChannelId::new(datagram[0]);
                let Some(descriptor) = map.get(channel) else {
                    emit_error(&tx, &metrics, channel_unknown(channel)).await;
                    continue;
                };
                if descriptor.reliable {
                    emit_error(
                        &tx,
                        &metrics,
                        protocol_error(format!(
                            "channel {channel} is reliable but arrived as a datagram"
                        )),
                    )
                    .await;
                    continue;
                }
                let payload = datagram.slice(1..);
                if payload.len() > descriptor.max_payload as usize {
                    emit_error(
                        &tx,
                        &metrics,
                        payload_too_large(channel, payload.len(), descriptor.max_payload as usize),
                    )
                    .await;
                    continue;
                }
                metrics.record_received(payload.len(), true);
                if tx
                    .send(TransportEvent::Datagram { channel, payload })
                    .await
                    .is_err()
                {
                    return;
                }
            }
            Err(error) => {
                let reason = match error {
                    ConnectionError::ApplicationClosed(_) => CloseReason::PeerClosed,
                    ConnectionError::LocallyClosed => CloseReason::LocalShutdown,
                    ConnectionError::TimedOut => CloseReason::Timeout,
                    _ => CloseReason::TransportError,
                };
                finish_closed(&tx, &closed_once, &metrics, reason).await;
                return;
            }
        }
    }
}

async fn emit_error(tx: &mpsc::Sender<TransportEvent>, metrics: &Metrics, error: ApiError) {
    metrics.record_receive_error();
    let _ = tx.send(TransportEvent::Error { error }).await;
}

async fn finish_closed(
    tx: &mpsc::Sender<TransportEvent>,
    closed_once: &AtomicBool,
    metrics: &Metrics,
    reason: CloseReason,
) {
    metrics.mark_closed();
    if !closed_once.swap(true, Ordering::SeqCst) {
        let _ = tx.send(TransportEvent::Closed { reason }).await;
    }
}

fn reason_for_read_exact(error: &ReadExactError, conn: &Connection) -> CloseReason {
    match error {
        ReadExactError::FinishedEarly(0) => CloseReason::PeerClosed,
        ReadExactError::FinishedEarly(_) => CloseReason::ProtocolError,
        ReadExactError::ReadError(ReadError::ConnectionLost(error)) => {
            reason_for_connection_error(error, conn)
        }
        ReadExactError::ReadError(ReadError::Reset(_)) => CloseReason::ProtocolError,
        ReadExactError::ReadError(ReadError::ClosedStream) => CloseReason::PeerClosed,
        ReadExactError::ReadError(_) => CloseReason::TransportError,
    }
}

fn reason_for_connection_error(error: &ConnectionError, conn: &Connection) -> CloseReason {
    match error {
        ConnectionError::ApplicationClosed(_) => CloseReason::PeerClosed,
        ConnectionError::LocallyClosed => CloseReason::LocalShutdown,
        ConnectionError::TimedOut => CloseReason::Timeout,
        _ => {
            if let Some(close_reason) = conn.close_reason() {
                reason_for_connection_error(&close_reason, conn)
            } else {
                CloseReason::TransportError
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn self_signed_certificate_is_usable_der() {
        let certificate = SelfSignedCertificate::generate(&["localhost".to_string()]).unwrap();
        assert!(!certificate.certificate_der().is_empty());
        // A second call produces a fresh key/certificate.
        let other = SelfSignedCertificate::generate(&[]).unwrap();
        assert_ne!(certificate.certificate_der(), other.certificate_der());
    }

    #[test]
    fn channel_framing_header_is_big_endian() {
        let payload = b"hello";
        let mut frame = Vec::new();
        frame.push(ChannelId::CONTROL.raw());
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        assert_eq!(&frame[..5], &[0, 0, 0, 0, 5]);
    }
}
