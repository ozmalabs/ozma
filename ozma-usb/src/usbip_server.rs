// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded USB/IP control-plane server.
//!
//! The server answers the two version-`0x0111` control requests a USB/IP
//! client sends before any data path exists:
//!
//! * `OP_REQ_DEVLIST` -> `OP_REP_DEVLIST` built from the daemon's exported
//!   devices that have a registered [`crate::usbip_protocol::UsbIpDeviceDescriptor`];
//! * `OP_REQ_IMPORT` -> `OP_REP_IMPORT` after the same host/token/key
//!   fingerprint authorization the [`crate::usbip_daemon::UsbIpDaemon`] uses.
//!
//! Authorization is enforced through the daemon itself
//! (`accept_connection` and `attach`), so a denied peer never reaches the
//! injected [`crate::usbip_daemon::UsbIpKernelBackend`]. The USB/IP wire
//! protocol has no token or fingerprint field, so those credentials are
//! supplied by an out-of-band [`UsbIpCredentialSource`] (for example from a TLS
//! terminator's client-certificate verification). The server never writes a
//! token or fingerprint to the wire, status, or logs; [`UsbIpPresentedCredentials`]
//! has a redacted `Debug`.
//!
//! A successful `OP_REP_IMPORT` carries the 312-byte device record only after
//! `UsbIpDaemon::attach` returned an attachment with a real local `vhci` port
//! from the kernel backend. With the default `NoUsbIpKernel` backend (or any
//! backend whose `attach` fails), the reply is an error status with no device
//! body and the caller receives the typed `usbip-kernel-required` error. This
//! server never invents a bus id, path, port, or successful attach.
//!
//! TLS is unchanged: this listener is plaintext and is meant to sit behind the
//! external `usbipd`/TLS terminator or encrypted tunnel described by
//! [`crate::usbip::UsbIpTlsStatus`]. `UsbIpTlsPolicy::Required` still fails
//! closed at daemon construction; no in-process TLS is claimed or faked.
//!
//! Framing is bounded end to end: the configured frame ceiling, the codec's
//! device/interface limits, a connection ceiling, and an idle deadline that is
//! enforced with short poll timeouts so `stop()` can interrupt blocked reads.

use std::fmt;
use std::io::{self, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::usbip_daemon::{UsbIpDaemon, MAX_USBIP_DAEMON_CONNECTIONS};
use crate::usbip_protocol::{
    UsbIpDevListReply, UsbIpDevListRequest, UsbIpImportReply, UsbIpImportRequest, UsbIpOpCode,
    UsbIpOpHeader, UsbIpProtocolError, UsbIpStatus, USBIP_BUSID_BYTES, USBIP_DEVLIST_HEADER_BYTES,
    USBIP_IMPORT_REQUEST_BYTES, USBIP_MAX_FRAME_BYTES, USBIP_OP_HEADER_BYTES,
    USBIP_PROTOCOL_VERSION,
};

/// Maximum simultaneous sessions accepted by one server.
pub const USBIP_SERVER_MAX_CONNECTIONS: usize = MAX_USBIP_DAEMON_CONNECTIONS;

/// Default idle deadline between complete control frames.
pub const USBIP_SERVER_DEFAULT_IDLE_TIMEOUT_MS: u64 = 30_000;

/// Poll interval used to observe `stop()` and idle deadlines on blocking reads.
const POLL_INTERVAL: Duration = Duration::from_millis(50);

/// Bounds for one USB/IP control listener/session.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbIpServerConfig {
    /// Maximum simultaneous accepted sessions.
    pub max_connections: usize,
    /// Largest control frame accepted or produced.
    pub max_frame_bytes: usize,
    /// Idle deadline between complete frames; `None` waits forever.
    pub idle_timeout_ms: Option<u64>,
}

impl Default for UsbIpServerConfig {
    fn default() -> Self {
        Self {
            max_connections: 8,
            max_frame_bytes: USBIP_MAX_FRAME_BYTES,
            idle_timeout_ms: Some(USBIP_SERVER_DEFAULT_IDLE_TIMEOUT_MS),
        }
    }
}

impl UsbIpServerConfig {
    /// Validate connection, frame, and deadline bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !(1..=USBIP_SERVER_MAX_CONNECTIONS).contains(&self.max_connections) {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_server.max_connections",
                reason: format!("max_connections must be 1..={USBIP_SERVER_MAX_CONNECTIONS}"),
            });
        }
        if !(USBIP_DEVLIST_HEADER_BYTES..=USBIP_MAX_FRAME_BYTES).contains(&self.max_frame_bytes) {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_server.max_frame_bytes",
                reason: format!(
                    "max_frame_bytes must be {USBIP_DEVLIST_HEADER_BYTES}..={USBIP_MAX_FRAME_BYTES}"
                ),
            });
        }
        if matches!(self.idle_timeout_ms, Some(0)) {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_server.idle_timeout_ms",
                reason: "idle_timeout_ms must be greater than zero when set".to_owned(),
            });
        }
        Ok(())
    }
}

/// A clock used for idle deadlines.
///
/// Tests inject [`UsbIpManualClock`]; the server defaults to
/// [`UsbIpSystemClock`].
pub trait UsbIpClock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_millis(&self) -> u64;
}

/// Wall-clock implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct UsbIpSystemClock;

impl UsbIpClock for UsbIpSystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Deterministic clock for tests.
#[derive(Debug, Default)]
pub struct UsbIpManualClock {
    millis: AtomicU64,
}

impl UsbIpManualClock {
    /// Start at `millis`.
    pub fn new(millis: u64) -> Self {
        Self {
            millis: AtomicU64::new(millis),
        }
    }

    /// Advance the clock.
    pub fn advance(&self, millis: u64) {
        self.millis.fetch_add(millis, Ordering::SeqCst);
    }

    /// Set the clock.
    pub fn set(&self, millis: u64) {
        self.millis.store(millis, Ordering::SeqCst);
    }
}

impl UsbIpClock for UsbIpManualClock {
    fn now_millis(&self) -> u64 {
        self.millis.load(Ordering::SeqCst)
    }
}

/// Out-of-band credentials presented for one connection.
///
/// The USB/IP control protocol has no field for either value. A TLS
/// terminator, trusted proxy, or local policy hook supplies them through a
/// [`UsbIpCredentialSource`]. The values are never serialized and `Debug` only
/// reports whether each value was present.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct UsbIpPresentedCredentials {
    token: Option<String>,
    key_fingerprint: Option<String>,
}

impl UsbIpPresentedCredentials {
    /// No credentials.
    pub fn new() -> Self {
        Self::default()
    }

    /// Present a bearer token.
    pub fn with_token(mut self, token: impl Into<String>) -> Self {
        self.token = Some(token.into());
        self
    }

    /// Present a pairing-key fingerprint.
    pub fn with_key_fingerprint(mut self, fingerprint: impl Into<String>) -> Self {
        self.key_fingerprint = Some(fingerprint.into());
        self
    }

    /// The bearer token, when present.
    pub fn token(&self) -> Option<&str> {
        self.token.as_deref()
    }

    /// The key fingerprint, when present.
    pub fn key_fingerprint(&self) -> Option<&str> {
        self.key_fingerprint.as_deref()
    }

    /// Whether a token was presented (never the value).
    pub fn token_presented(&self) -> bool {
        self.token.is_some()
    }

    /// Whether a fingerprint was presented (never the value).
    pub fn key_fingerprint_presented(&self) -> bool {
        self.key_fingerprint.is_some()
    }
}

impl fmt::Debug for UsbIpPresentedCredentials {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("UsbIpPresentedCredentials")
            .field("token_presented", &self.token.is_some())
            .field("key_fingerprint_presented", &self.key_fingerprint.is_some())
            .finish()
    }
}

/// Supplies connection credentials that the USB/IP wire protocol cannot carry.
pub trait UsbIpCredentialSource: Send + Sync {
    /// Credentials for an accepted peer host.
    fn credentials(&self, peer_host: &str) -> UsbIpPresentedCredentials;
}

/// A source that presents nothing, so any token/fingerprint requirement fails
/// closed.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUsbIpCredentials;

impl UsbIpCredentialSource for NoUsbIpCredentials {
    fn credentials(&self, _peer_host: &str) -> UsbIpPresentedCredentials {
        UsbIpPresentedCredentials::default()
    }
}

/// A stream that can carry USB/IP control frames.
///
/// `TcpStream` implements this; tests can provide an in-memory duplex. The
/// default timeout/peer implementations are no-ops so in-memory transports do
/// not need a socket.
pub trait UsbIpTransport: Read + Write {
    /// Configure read/write poll timeouts (used for stop and idle checks).
    fn set_io_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let _ = timeout;
        Ok(())
    }

    /// Peer host label used for authorization (no port).
    fn peer_host(&self) -> Option<String> {
        None
    }
}

impl UsbIpTransport for TcpStream {
    fn set_io_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }

    fn peer_host(&self) -> Option<String> {
        self.peer_addr()
            .ok()
            .map(|address| address.ip().to_string())
    }
}

/// Why a control session ended normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbIpSessionEnd {
    /// The peer closed the stream between frames.
    PeerClosed,
    /// The server handle requested shutdown.
    Stopped,
    /// The configured idle deadline expired.
    Timeout,
}

/// A completed control-session summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpSessionOutcome {
    /// Peer host used for authorization.
    pub peer: Option<String>,
    /// Negotiated protocol version, once a valid request arrived.
    pub version: Option<u16>,
    /// Complete `OP_REQ_DEVLIST` requests answered.
    pub devlist_requests: u64,
    /// Complete `OP_REQ_IMPORT` requests processed.
    pub import_requests: u64,
    /// Devices advertised across all device-list replies.
    pub devices_listed: usize,
    /// Bus id whose import succeeded through the kernel backend.
    pub attached_bus_id: Option<String>,
    /// Bytes read from the peer.
    pub bytes_read: u64,
    /// Bytes written to the peer.
    pub bytes_written: u64,
    /// Normal termination reason.
    pub end: Option<UsbIpSessionEnd>,
}

/// Cumulative server counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpServerStats {
    /// Connections accepted by the listener.
    pub connections_accepted: u64,
    /// Connections refused at the listener or daemon ceiling.
    pub connections_rejected: u64,
    /// Sessions that ran to completion or failed after acceptance.
    pub sessions: u64,
    /// `OP_REQ_DEVLIST` requests answered.
    pub devlist_requests: u64,
    /// `OP_REQ_IMPORT` requests processed.
    pub import_requests: u64,
    /// Imports that reached a successful kernel attach.
    pub imports: u64,
    /// Imports that failed (authorization, unknown device, kernel, ...).
    pub import_failures: u64,
    /// Devices advertised across all replies.
    pub devices_listed: u64,
    /// Sessions denied by host/token/fingerprint authorization.
    pub denied: u64,
    /// Sessions that failed with a protocol/frame error.
    pub protocol_errors: u64,
    /// Bytes read from peers.
    pub bytes_read: u64,
    /// Bytes written to peers.
    pub bytes_written: u64,
}

#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    rejected: AtomicU64,
    sessions: AtomicU64,
    devlist_requests: AtomicU64,
    import_requests: AtomicU64,
    imports: AtomicU64,
    import_failures: AtomicU64,
    devices_listed: AtomicU64,
    denied: AtomicU64,
    protocol_errors: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
}

impl Counters {
    fn add_outcome(&self, outcome: &UsbIpSessionOutcome) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
        self.devlist_requests
            .fetch_add(outcome.devlist_requests, Ordering::Relaxed);
        self.import_requests
            .fetch_add(outcome.import_requests, Ordering::Relaxed);
        if outcome.attached_bus_id.is_some() {
            self.imports.fetch_add(1, Ordering::Relaxed);
        }
        self.devices_listed
            .fetch_add(outcome.devices_listed as u64, Ordering::Relaxed);
        self.bytes_read
            .fetch_add(outcome.bytes_read, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(outcome.bytes_written, Ordering::Relaxed);
    }

    fn add_error(&self, error: &UsbError) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
        match error {
            UsbError::UsbIpUnauthorized { .. } => {
                self.denied.fetch_add(1, Ordering::Relaxed);
            }
            UsbError::UsbIpKernelRequired { .. }
            | UsbError::UsbIpUnknownDevice { .. }
            | UsbError::UsbIpConnectionLimit { .. } => {
                self.import_failures.fetch_add(1, Ordering::Relaxed);
                if matches!(error, UsbError::UsbIpConnectionLimit { .. }) {
                    self.rejected.fetch_add(1, Ordering::Relaxed);
                }
            }
            _ => {
                self.protocol_errors.fetch_add(1, Ordering::Relaxed);
            }
        }
    }

    fn snapshot(&self) -> UsbIpServerStats {
        UsbIpServerStats {
            connections_accepted: self.accepted.load(Ordering::Relaxed),
            connections_rejected: self.rejected.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
            devlist_requests: self.devlist_requests.load(Ordering::Relaxed),
            import_requests: self.import_requests.load(Ordering::Relaxed),
            imports: self.imports.load(Ordering::Relaxed),
            import_failures: self.import_failures.load(Ordering::Relaxed),
            devices_listed: self.devices_listed.load(Ordering::Relaxed),
            denied: self.denied.load(Ordering::Relaxed),
            protocol_errors: self.protocol_errors.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
        }
    }
}

/// A bound USB/IP control listener waiting for `start`.
pub struct UsbIpServer {
    listener: TcpListener,
    daemon: Arc<UsbIpDaemon>,
    config: UsbIpServerConfig,
    credentials: Arc<dyn UsbIpCredentialSource>,
    clock: Arc<dyn UsbIpClock>,
}

impl UsbIpServer {
    /// Bind a plaintext TCP control listener.
    ///
    /// TLS is not terminated here; see the module documentation and
    /// [`crate::usbip::UsbIpTlsStatus`].
    pub fn bind_tcp(
        address: impl ToSocketAddrs,
        daemon: Arc<UsbIpDaemon>,
        config: UsbIpServerConfig,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        let listener = TcpListener::bind(address).map_err(|source| UsbError::Io {
            operation: "bind USB/IP control listener",
            path: PathBuf::from("<usbip-listener>"),
            source,
        })?;
        Ok(Self {
            listener,
            daemon,
            config,
            credentials: Arc::new(NoUsbIpCredentials),
            clock: Arc::new(UsbIpSystemClock),
        })
    }

    /// Inject an out-of-band credential source.
    pub fn with_credentials(mut self, credentials: Arc<dyn UsbIpCredentialSource>) -> Self {
        self.credentials = credentials;
        self
    }

    /// Inject a clock (tests can use [`UsbIpManualClock`]).
    pub fn with_clock(mut self, clock: Arc<dyn UsbIpClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Bound local address.
    pub fn local_addr(&self) -> Result<SocketAddr, UsbError> {
        self.listener.local_addr().map_err(|source| UsbError::Io {
            operation: "read USB/IP listener address",
            path: PathBuf::from("<usbip-listener>"),
            source,
        })
    }

    /// Start accepting control sessions on a background thread.
    pub fn start(self) -> Result<UsbIpServerHandle, UsbError> {
        let UsbIpServer {
            listener,
            daemon,
            config,
            credentials,
            clock,
        } = self;
        listener
            .set_nonblocking(true)
            .map_err(|source| UsbError::Io {
                operation: "configure USB/IP listener",
                path: PathBuf::from("<usbip-listener>"),
                source,
            })?;
        let local_addr = listener.local_addr().ok();
        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let active = Arc::new(AtomicU64::new(0));

        let accept_stop = Arc::clone(&stop);
        let accept_counters = Arc::clone(&counters);
        let accept_active = Arc::clone(&active);
        let accept_config = config.clone();
        let accept_credentials = Arc::clone(&credentials);
        let accept_clock = Arc::clone(&clock);
        let accept_daemon = Arc::clone(&daemon);
        let join = thread::Builder::new()
            .name("ozma-usbip-accept".to_owned())
            .spawn(move || {
                accept_loop(
                    listener,
                    accept_daemon,
                    accept_config,
                    accept_credentials,
                    accept_clock,
                    accept_stop,
                    accept_counters,
                    accept_active,
                );
            })
            .map_err(|source| UsbError::Spawn {
                command: "ozma-usbip-accept".to_owned(),
                source,
            })?;

        Ok(UsbIpServerHandle {
            stop,
            join: Some(join),
            counters,
            active,
            local_addr,
        })
    }
}

fn accept_loop(
    listener: TcpListener,
    daemon: Arc<UsbIpDaemon>,
    config: UsbIpServerConfig,
    credentials: Arc<dyn UsbIpCredentialSource>,
    clock: Arc<dyn UsbIpClock>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    active: Arc<AtomicU64>,
) {
    let mut sessions: Vec<JoinHandle<()>> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                if stop.load(Ordering::SeqCst) {
                    let _ = stream.shutdown(Shutdown::Both);
                    break;
                }
                let live = active.fetch_add(1, Ordering::SeqCst) + 1;
                if live > config.max_connections as u64 {
                    active.fetch_sub(1, Ordering::SeqCst);
                    counters.rejected.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.shutdown(Shutdown::Both);
                    continue;
                }
                counters.accepted.fetch_add(1, Ordering::Relaxed);
                let session_daemon = Arc::clone(&daemon);
                let session_config = config.clone();
                let session_credentials = Arc::clone(&credentials);
                let session_clock = Arc::clone(&clock);
                let session_stop = Arc::clone(&stop);
                let session_counters = Arc::clone(&counters);
                let session_active = Arc::clone(&active);
                match thread::Builder::new()
                    .name("ozma-usbip-session".to_owned())
                    .spawn(move || {
                        let mut stream = stream;
                        let _ = stream.set_io_timeout(Some(POLL_INTERVAL));
                        let result = serve_session(
                            &mut stream,
                            session_daemon.as_ref(),
                            &session_config,
                            session_credentials.as_ref(),
                            session_clock.as_ref(),
                            session_stop.as_ref(),
                        );
                        match result {
                            Ok(outcome) => session_counters.add_outcome(&outcome),
                            Err(error) => session_counters.add_error(&error),
                        }
                        session_active.fetch_sub(1, Ordering::SeqCst);
                    }) {
                    Ok(handle) => sessions.push(handle),
                    Err(_) => {
                        active.fetch_sub(1, Ordering::SeqCst);
                        counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                counters.protocol_errors.fetch_add(1, Ordering::Relaxed);
                thread::sleep(POLL_INTERVAL);
            }
        }
    }
    for session in sessions {
        let _ = session.join();
    }
}

/// A running USB/IP control server.
///
/// Dropping the handle requests shutdown and joins the listener plus any
/// sessions. Sessions observe shutdown through the same short poll interval
/// used for idle deadlines.
pub struct UsbIpServerHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
    active: Arc<AtomicU64>,
    local_addr: Option<SocketAddr>,
}

impl UsbIpServerHandle {
    /// Bound local address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Current counters.
    pub fn stats(&self) -> UsbIpServerStats {
        self.counters.snapshot()
    }

    /// Currently active sessions.
    pub fn active_connections(&self) -> u64 {
        self.active.load(Ordering::SeqCst)
    }

    /// Stop accepting, wake the accept loop, and join all sessions.
    pub fn stop(mut self) -> UsbIpServerStats {
        self.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.counters.snapshot()
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(address) = self.local_addr {
            let address = if address.ip().is_unspecified() {
                SocketAddr::from(([127, 0, 0, 1], address.port()))
            } else {
                address
            };
            let _ = TcpStream::connect_timeout(&address, Duration::from_millis(100));
        }
    }
}

impl Drop for UsbIpServerHandle {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

/// Serve one USB/IP control session over an injected transport.
///
/// The session authorizes the peer through the daemon before reading a frame,
/// negotiates version `0x0111`, answers `OP_REQ_DEVLIST`/`OP_REQ_IMPORT`, and
/// closes its tracked connection on every exit path. Frame, connection, and
/// deadline errors are typed; truncation/oversize/version/opcode failures are
/// [`UsbError::UsbIpProtocol`].
pub fn serve_session<S: UsbIpTransport>(
    stream: &mut S,
    daemon: &UsbIpDaemon,
    config: &UsbIpServerConfig,
    credentials: &dyn UsbIpCredentialSource,
    clock: &dyn UsbIpClock,
    stop: &AtomicBool,
) -> Result<UsbIpSessionOutcome, UsbError> {
    config.validate()?;
    let peer = stream.peer_host();
    let host = peer.clone().unwrap_or_else(|| "unknown".to_owned());
    let presented = credentials.credentials(&host);
    let connection =
        daemon.accept_connection(&host, presented.token(), presented.key_fingerprint())?;
    let _ = stream.set_io_timeout(Some(POLL_INTERVAL));

    let mut session = Session {
        stream,
        daemon,
        config,
        clock,
        stop,
        host,
        presented,
        last_activity: clock.now_millis(),
        outcome: UsbIpSessionOutcome {
            peer,
            ..UsbIpSessionOutcome::default()
        },
    };
    let outcome = session.run();
    let _ = daemon.close_connection(connection.id);
    outcome
}

struct Session<'a, S: UsbIpTransport> {
    stream: &'a mut S,
    daemon: &'a UsbIpDaemon,
    config: &'a UsbIpServerConfig,
    clock: &'a dyn UsbIpClock,
    stop: &'a AtomicBool,
    host: String,
    presented: UsbIpPresentedCredentials,
    last_activity: u64,
    outcome: UsbIpSessionOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ReadSignal {
    Complete,
    End(UsbIpSessionEnd),
}

impl<'a, S: UsbIpTransport> Session<'a, S> {
    fn run(&mut self) -> Result<UsbIpSessionOutcome, UsbError> {
        loop {
            let mut header_bytes = [0u8; USBIP_OP_HEADER_BYTES];
            match self.read_exact(&mut header_bytes, true)? {
                ReadSignal::Complete => {}
                ReadSignal::End(end) => {
                    self.outcome.end = Some(end);
                    return Ok(std::mem::take(&mut self.outcome));
                }
            }

            let version = u16::from_be_bytes([header_bytes[0], header_bytes[1]]);
            let raw_code = u16::from_be_bytes([header_bytes[2], header_bytes[3]]);
            let Some(opcode) = UsbIpOpCode::from_raw(raw_code) else {
                return Err(UsbIpProtocolError::UnknownOpcode { code: raw_code }.into());
            };
            if !opcode.is_request() {
                return Err(UsbIpProtocolError::UnexpectedOpcode {
                    expected: "a request",
                    actual: opcode.name(),
                }
                .into());
            }
            if version != USBIP_PROTOCOL_VERSION {
                let _ = self.send_error_reply(opcode, UsbIpStatus::Error);
                return Err(UsbIpProtocolError::UnsupportedVersion {
                    version,
                    expected: USBIP_PROTOCOL_VERSION,
                }
                .into());
            }
            self.outcome.version = Some(version);

            match opcode {
                UsbIpOpCode::ReqDevList => self.handle_devlist(&header_bytes)?,
                UsbIpOpCode::ReqImport => {
                    if let Some(end) = self.handle_import(&header_bytes)? {
                        self.outcome.end = Some(end);
                        return Ok(std::mem::take(&mut self.outcome));
                    }
                }
                UsbIpOpCode::RepDevList | UsbIpOpCode::RepImport => {
                    return Err(UsbIpProtocolError::UnexpectedOpcode {
                        expected: "a request",
                        actual: opcode.name(),
                    }
                    .into());
                }
            }
        }
    }

    fn handle_devlist(
        &mut self,
        header_bytes: &[u8; USBIP_OP_HEADER_BYTES],
    ) -> Result<(), UsbError> {
        let _request = UsbIpDevListRequest::decode(header_bytes)?;
        let devices = self.daemon.device_list()?;
        let count = devices.len();
        let reply = UsbIpDevListReply::new(devices);
        let bytes = reply.encode()?;
        self.write_frame(&bytes)?;
        self.outcome.devlist_requests = self.outcome.devlist_requests.saturating_add(1);
        self.outcome.devices_listed = self.outcome.devices_listed.saturating_add(count);
        Ok(())
    }

    fn handle_import(
        &mut self,
        header_bytes: &[u8; USBIP_OP_HEADER_BYTES],
    ) -> Result<Option<UsbIpSessionEnd>, UsbError> {
        if USBIP_IMPORT_REQUEST_BYTES > self.config.max_frame_bytes {
            return Err(UsbIpProtocolError::Oversized {
                field: "frame",
                limit: self.config.max_frame_bytes,
                requested: USBIP_IMPORT_REQUEST_BYTES,
            }
            .into());
        }
        let mut body = [0u8; USBIP_BUSID_BYTES];
        match self.read_exact(&mut body, false)? {
            ReadSignal::Complete => {}
            ReadSignal::End(end) => return Ok(Some(end)),
        }
        let mut frame = [0u8; USBIP_IMPORT_REQUEST_BYTES];
        frame[..USBIP_OP_HEADER_BYTES].copy_from_slice(header_bytes);
        frame[USBIP_OP_HEADER_BYTES..].copy_from_slice(&body);
        let request = UsbIpImportRequest::decode(&frame)?;
        self.outcome.import_requests = self.outcome.import_requests.saturating_add(1);

        let devices = self.daemon.device_list()?;
        let Some(descriptor) = devices
            .into_iter()
            .find(|descriptor| descriptor.bus_id == request.bus_id)
        else {
            let _ = self.send_error_reply(UsbIpOpCode::ReqImport, UsbIpStatus::NoDev);
            return Err(UsbError::UsbIpUnknownDevice {
                bus_id: request.bus_id,
            });
        };

        let attachment = match self.daemon.attach(
            &self.host,
            &descriptor.bus_id,
            self.presented.token(),
            self.presented.key_fingerprint(),
        ) {
            Ok(attachment) => attachment,
            Err(error) => {
                let _ = self.send_error_reply(UsbIpOpCode::ReqImport, wire_status_for(&error));
                return Err(error);
            }
        };
        // Only a backend that really assigned a local vhci port can make an
        // import successful. This is a defensive check; the daemon constructs
        // the attachment from the backend's `u16` port.
        if attachment.port.is_none() {
            let _ = self.send_error_reply(UsbIpOpCode::ReqImport, UsbIpStatus::DevErr);
            return Err(UsbError::UsbIpKernelRequired {
                operation: "attach",
                reason: "the kernel backend reported no local vhci port; no bus id was returned"
                    .to_owned(),
            });
        }

        let reply = UsbIpImportReply::new(descriptor);
        let bytes = reply.encode()?;
        self.write_frame(&bytes)?;
        self.outcome.attached_bus_id = Some(request.bus_id);
        Ok(None)
    }

    fn send_error_reply(
        &mut self,
        request: UsbIpOpCode,
        status: UsbIpStatus,
    ) -> Result<(), UsbError> {
        let Some(reply_opcode) = request.reply() else {
            return Ok(());
        };
        let header = UsbIpOpHeader::reply(reply_opcode, status);
        let bytes = match reply_opcode {
            UsbIpOpCode::RepDevList => UsbIpDevListReply {
                header,
                devices: Vec::new(),
            }
            .encode()?,
            UsbIpOpCode::RepImport => UsbIpImportReply {
                header,
                device: None,
            }
            .encode()?,
            UsbIpOpCode::ReqDevList | UsbIpOpCode::ReqImport => header.encode().to_vec(),
        };
        self.write_frame(&bytes)
    }

    fn read_exact(
        &mut self,
        buffer: &mut [u8],
        allow_clean_eof: bool,
    ) -> Result<ReadSignal, UsbError> {
        let mut filled = 0;
        while filled < buffer.len() {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(ReadSignal::End(UsbIpSessionEnd::Stopped));
            }
            if let Some(idle_ms) = self.config.idle_timeout_ms {
                let deadline = self.last_activity.saturating_add(idle_ms);
                if self.clock.now_millis() >= deadline {
                    return Ok(ReadSignal::End(UsbIpSessionEnd::Timeout));
                }
            }
            match self.stream.read(&mut buffer[filled..]) {
                Ok(0) => {
                    if filled == 0 && allow_clean_eof {
                        return Ok(ReadSignal::End(UsbIpSessionEnd::PeerClosed));
                    }
                    return Err(UsbIpProtocolError::Truncated {
                        needed: buffer.len(),
                        available: filled,
                    }
                    .into());
                }
                Ok(read) => {
                    filled += read;
                    self.outcome.bytes_read = self.outcome.bytes_read.saturating_add(read as u64);
                    self.last_activity = self.clock.now_millis();
                }
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock
                            | io::ErrorKind::TimedOut
                            | io::ErrorKind::Interrupted
                    ) => {}
                Err(error) => {
                    return Err(UsbError::Io {
                        operation: "read USB/IP control frame",
                        path: PathBuf::from("<usbip-stream>"),
                        source: error,
                    });
                }
            }
        }
        Ok(ReadSignal::Complete)
    }

    fn write_frame(&mut self, bytes: &[u8]) -> Result<(), UsbError> {
        if bytes.len() > self.config.max_frame_bytes {
            return Err(UsbIpProtocolError::Oversized {
                field: "frame",
                limit: self.config.max_frame_bytes,
                requested: bytes.len(),
            }
            .into());
        }
        if bytes.len() > USBIP_MAX_FRAME_BYTES {
            return Err(UsbIpProtocolError::Oversized {
                field: "frame",
                limit: USBIP_MAX_FRAME_BYTES,
                requested: bytes.len(),
            }
            .into());
        }
        self.stream
            .write_all(bytes)
            .map_err(|source| UsbError::Io {
                operation: "write USB/IP control frame",
                path: PathBuf::from("<usbip-stream>"),
                source,
            })?;
        let _ = self.stream.flush();
        self.outcome.bytes_written = self
            .outcome
            .bytes_written
            .saturating_add(bytes.len() as u64);
        self.last_activity = self.clock.now_millis();
        Ok(())
    }
}

fn wire_status_for(error: &UsbError) -> UsbIpStatus {
    match error {
        UsbError::UsbIpUnauthorized { .. } => UsbIpStatus::Error,
        UsbError::UsbIpKernelRequired { .. } => UsbIpStatus::DevErr,
        UsbError::UsbIpUnknownDevice { .. } => UsbIpStatus::NoDev,
        UsbError::UsbIpConnectionLimit { .. } => UsbIpStatus::Error,
        UsbError::InvalidUsbIp { .. } => UsbIpStatus::DevBusy,
        _ => UsbIpStatus::Error,
    }
}
