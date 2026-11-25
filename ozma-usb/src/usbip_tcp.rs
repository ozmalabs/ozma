// SPDX-License-Identifier: AGPL-3.0-only
//! Real USB/IP URB data endpoint over an imported TCP connection.
//!
//! [`LinuxUsbIpKernelDataEndpoint`] is the production sibling of
//! [`crate::usbip_relay::NoUsbIpKernelDataEndpoint`]. For one attachment it:
//!
//! 1. validates the remote host and bus id;
//! 2. enforces the optional host/token/key-fingerprint authorization before any
//!    network operation;
//! 3. dials the remote USB/IP server (default port 3240) through an injectable
//!    [`UsbIpUrbConnector`];
//! 4. sends the version-`0x0111` `OP_REQ_IMPORT` request and validates the
//!    import reply with the existing control codec; and
//! 5. returns a [`LinuxUsbIpUrbSession`] that translates
//!    [`UsbIpDataEndpoint`] calls into `USBIP_CMD_SUBMIT`/`USBIP_CMD_UNLINK`
//!    frames and pairs the matching `USBIP_RET_*` replies.
//!
//! The session reuses the [`crate::usbip_urb`] codec exactly; it never invents a
//! completion. A refused import, missing port, malformed reply, disconnected
//! stream, or deadline is a typed error. The remote device id is translated in
//! both directions so a proxy can present a different local devid.
//!
//! No kernel module, `usbip` command, root privilege, or USB device is touched.
//! The adapter speaks the same wire protocol a kernel `vhci_hcd`/`usbip-host`
//! pair would speak, over an ordinary TCP socket. TLS is still external (the
//! USB/IP protocol has no in-band TLS and this crate does not terminate it).

use std::collections::BTreeMap;
use std::io::{self, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::usbip::{
    validate_host, UsbIpAttachment, UsbIpAuthorization, UsbIpPeerGrant, DEFAULT_USBIP_PORT,
};
use crate::usbip_protocol::{
    UsbIpImportReply, UsbIpImportRequest, UsbIpOpCode, UsbIpOpHeader, UsbIpStatus,
    USBIP_IMPORT_REPLY_BYTES, USBIP_OP_HEADER_BYTES, USBIP_PROTOCOL_VERSION,
};
use crate::usbip_relay::{UsbIpDataEndpoint, UsbIpDataError, UsbIpKernelDataEndpoint};
use crate::usbip_urb::{
    UsbIpCmdSubmit, UsbIpCmdUnlink, UsbIpRetSubmit, UsbIpRetUnlink, UsbIpUrb,
    USBIP_URB_HEADER_BYTES, USBIP_URB_MAX_TRANSFER_BYTES, USBIP_URB_UNLINK_CANCELLED,
};

/// Default deadline for one remote URB submit.
pub const USBIP_TCP_DEFAULT_TIMEOUT_MS: u64 = 5_000;

/// Largest accepted deadline.
pub const USBIP_TCP_MAX_TIMEOUT_MS: u64 = 60_000;

/// Default poll interval used to notice shutdown while a socket read blocks.
const STREAM_POLL_TIMEOUT: Duration = Duration::from_millis(250);

/// `-ECONNRESET`.
const ECONNRESET: i32 = -104;
/// `-ETIMEDOUT`.
const ETIMEDOUT: i32 = -110;
/// `-EINVAL`.
const EINVAL: i32 = -22;
/// `-EMSGSIZE`.
const EMSGSIZE: i32 = -90;
/// `-EPROTO`.
const EPROTO: i32 = -71;
/// `-ENOTCONN`.
const ENOTCONN: i32 = -107;

/// A duplex byte stream carrying USB/IP control and URB frames.
///
/// `TcpStream` is the production implementation. Tests can provide an in-memory
/// stream; implementing [`Self::shutdown`] is enough to wake a blocked reader.
pub trait UsbIpUrbStream: Read + Write + Send {
    /// Set read/write timeouts. The reader uses a short timeout so shutdown is
    /// observable without relying on socket closure alone.
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        let _ = timeout;
        Ok(())
    }

    /// Wake a blocked reader/writer.
    fn shutdown(&self) -> io::Result<()> {
        Ok(())
    }

    /// Clone the underlying handle for the reader thread.
    fn try_clone(&self) -> io::Result<Box<dyn UsbIpUrbStream>>;
}

impl UsbIpUrbStream for TcpStream {
    fn set_timeout(&self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }

    fn shutdown(&self) -> io::Result<()> {
        TcpStream::shutdown(self, std::net::Shutdown::Both)
    }

    fn try_clone(&self) -> io::Result<Box<dyn UsbIpUrbStream>> {
        TcpStream::try_clone(self).map(|stream| Box::new(stream) as Box<dyn UsbIpUrbStream>)
    }
}

/// Opens a stream to a remote USB/IP server.
pub trait UsbIpUrbConnector: Send + Sync {
    /// Whether this connector can attempt a connection.
    fn available(&self) -> bool {
        true
    }

    /// Connect to `host:port`.
    fn connect(&self, host: &str, port: u16) -> Result<Box<dyn UsbIpUrbStream>, UsbError>;
}

/// Production TCP connector with bounded connect/read/write timeouts.
#[derive(Clone, Copy, Debug)]
pub struct TcpUsbIpConnector {
    connect_timeout: Duration,
    io_timeout: Duration,
}

impl Default for TcpUsbIpConnector {
    fn default() -> Self {
        Self {
            connect_timeout: Duration::from_secs(5),
            io_timeout: STREAM_POLL_TIMEOUT,
        }
    }
}

impl TcpUsbIpConnector {
    /// Connector with explicit deadlines.
    pub fn new(connect_timeout: Duration, io_timeout: Duration) -> Self {
        Self {
            connect_timeout,
            io_timeout,
        }
    }

    /// Connect deadline.
    pub fn connect_timeout(&self) -> Duration {
        self.connect_timeout
    }

    /// Per-read/write socket timeout.
    pub fn io_timeout(&self) -> Duration {
        self.io_timeout
    }
}

impl UsbIpUrbConnector for TcpUsbIpConnector {
    fn connect(&self, host: &str, port: u16) -> Result<Box<dyn UsbIpUrbStream>, UsbError> {
        validate_host(host)?;
        if port == 0 {
            return Err(UsbError::InvalidUsbIp {
                field: "port",
                reason: "USB/IP server port must be in 1..=65535".to_owned(),
            });
        }
        let host = host.trim_start_matches('[').trim_end_matches(']');
        let addresses: Vec<_> = (host, port)
            .to_socket_addrs()
            .map_err(|source| UsbError::Io {
                operation: "resolve USB/IP data host",
                path: std::path::PathBuf::from("<usbip-data-host>"),
                source,
            })?
            .collect();
        let address = addresses.first().ok_or_else(|| UsbError::InvalidUsbIp {
            field: "host",
            reason: format!("{host:?} resolved to no addresses"),
        })?;
        let stream =
            TcpStream::connect_timeout(address, self.connect_timeout).map_err(|source| {
                UsbError::Io {
                    operation: "connect USB/IP data endpoint",
                    path: std::path::PathBuf::from("<usbip-data-stream>"),
                    source,
                }
            })?;
        stream
            .set_timeout(Some(self.io_timeout))
            .map_err(|source| UsbError::Io {
                operation: "configure USB/IP data endpoint",
                path: std::path::PathBuf::from("<usbip-data-stream>"),
                source,
            })?;
        Ok(Box::new(stream))
    }
}

/// Bounds and authorization for [`LinuxUsbIpKernelDataEndpoint`].
#[derive(Clone, Debug)]
pub struct LinuxUsbIpDataConfig {
    /// Remote USB/IP TCP port.
    pub port: u16,
    /// Per-submit/unlink deadline in milliseconds.
    pub timeout_ms: u64,
    /// Largest accepted transfer buffer.
    pub max_transfer_bytes: usize,
    /// Optional host/token/key-fingerprint allow-list.
    pub authorization: Option<UsbIpAuthorization>,
}

impl Default for LinuxUsbIpDataConfig {
    fn default() -> Self {
        Self {
            port: DEFAULT_USBIP_PORT,
            timeout_ms: USBIP_TCP_DEFAULT_TIMEOUT_MS,
            max_transfer_bytes: USBIP_URB_MAX_TRANSFER_BYTES,
            authorization: None,
        }
    }
}

impl LinuxUsbIpDataConfig {
    /// Validate port, deadline, and transfer bound.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.port == 0 {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_data.port",
                reason: "port must be in 1..=65535".to_owned(),
            });
        }
        if self.timeout_ms == 0 || self.timeout_ms > USBIP_TCP_MAX_TIMEOUT_MS {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_data.timeout_ms",
                reason: format!("timeout_ms must be 1..={USBIP_TCP_MAX_TIMEOUT_MS}"),
            });
        }
        if !(1..=USBIP_URB_MAX_TRANSFER_BYTES).contains(&self.max_transfer_bytes) {
            return Err(UsbError::InvalidUsbIp {
                field: "usbip_data.max_transfer_bytes",
                reason: format!("max_transfer_bytes must be 1..={USBIP_URB_MAX_TRANSFER_BYTES}"),
            });
        }
        if let Some(authorization) = &self.authorization {
            authorization.validate()?;
        }
        Ok(())
    }
}

/// Real USB/IP data-endpoint factory.
///
/// `open` uses the attachment host and the configured remote port. It is called
/// by the control server only after a successful authorized import, but it is
/// also usable by an embedder that drives the relay directly.
pub struct LinuxUsbIpKernelDataEndpoint {
    connector: Arc<dyn UsbIpUrbConnector>,
    config: LinuxUsbIpDataConfig,
}

impl std::fmt::Debug for LinuxUsbIpKernelDataEndpoint {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("LinuxUsbIpKernelDataEndpoint")
            .field("port", &self.config.port)
            .field("timeout_ms", &self.config.timeout_ms)
            .field("max_transfer_bytes", &self.config.max_transfer_bytes)
            .field("authorization", &self.config.authorization.is_some())
            .finish_non_exhaustive()
    }
}

impl Default for LinuxUsbIpKernelDataEndpoint {
    fn default() -> Self {
        Self::system()
    }
}

impl LinuxUsbIpKernelDataEndpoint {
    /// Production TCP adapter with default bounds.
    pub fn system() -> Self {
        Self::with_connector(Arc::new(TcpUsbIpConnector::default()))
    }

    /// Adapter over an injected connector.
    pub fn with_connector(connector: Arc<dyn UsbIpUrbConnector>) -> Self {
        Self {
            connector,
            config: LinuxUsbIpDataConfig::default(),
        }
    }

    /// Replace the bounds/authorization.
    pub fn with_config(mut self, config: LinuxUsbIpDataConfig) -> Self {
        self.config = config;
        self
    }

    /// Configured port/deadline/bounds.
    pub fn config(&self) -> &LinuxUsbIpDataConfig {
        &self.config
    }

    /// Whether the connector can attempt a connection.
    pub fn available(&self) -> bool {
        self.connector.available()
    }

    /// Open an endpoint after authorizing the attachment host.
    ///
    /// The USB/IP wire protocol carries no token or fingerprint. Embedders that
    /// require them must use this method with explicit credentials; the trait
    /// `open` presents nothing and therefore fails closed when the configured
    /// authorization requires a token or key.
    pub fn open_authorized(
        &self,
        attachment: &UsbIpAttachment,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<Arc<dyn UsbIpDataEndpoint>, UsbError> {
        self.config.validate()?;
        attachment.validate()?;
        let grant = match &self.config.authorization {
            Some(authorization) => {
                authorization.authorize_peer(&attachment.host, token, key_fingerprint)?
            }
            None => UsbIpPeerGrant {
                host: attachment.host.clone(),
                token_presented: token.is_some(),
                key: None,
            },
        };
        let _ = grant;

        let mut stream = self.connector.connect(&attachment.host, self.config.port)?;
        stream
            .set_timeout(Some(STREAM_POLL_TIMEOUT))
            .map_err(|source| UsbError::Io {
                operation: "configure USB/IP data endpoint",
                path: std::path::PathBuf::from("<usbip-data-stream>"),
                source,
            })?;

        let request = UsbIpImportRequest::new(&attachment.bus_id);
        let encoded = request.encode()?;
        stream.write_all(&encoded).map_err(|source| UsbError::Io {
            operation: "write USB/IP import request",
            path: std::path::PathBuf::from("<usbip-data-stream>"),
            source,
        })?;

        let mut header_bytes = [0u8; USBIP_OP_HEADER_BYTES];
        read_exact_stream(&mut *stream, &mut header_bytes, self.config.timeout_ms)?;
        let header = UsbIpOpHeader::decode(&header_bytes)?;
        if header.version != USBIP_PROTOCOL_VERSION {
            return Err(
                crate::usbip_protocol::UsbIpProtocolError::UnsupportedVersion {
                    version: header.version,
                    expected: USBIP_PROTOCOL_VERSION,
                }
                .into(),
            );
        }
        if header.opcode != UsbIpOpCode::RepImport {
            return Err(
                crate::usbip_protocol::UsbIpProtocolError::UnexpectedOpcode {
                    expected: UsbIpOpCode::RepImport.name(),
                    actual: header.opcode.name(),
                }
                .into(),
            );
        }
        if header.status != UsbIpStatus::Ok {
            return Err(match header.status {
                UsbIpStatus::NoDev => UsbError::UsbIpUnknownDevice {
                    bus_id: attachment.bus_id.clone(),
                },
                other => UsbError::Protocol {
                    protocol: "usbip",
                    reason: format!(
                        "remote USB/IP server refused bus id {:?} with status {}; no URB \
                         endpoint was opened",
                        attachment.bus_id,
                        other.as_str()
                    ),
                },
            });
        }

        let mut reply_bytes = vec![0u8; USBIP_IMPORT_REPLY_BYTES];
        reply_bytes[..USBIP_OP_HEADER_BYTES].copy_from_slice(&header.encode());
        read_exact_stream(
            &mut *stream,
            &mut reply_bytes[USBIP_OP_HEADER_BYTES..],
            self.config.timeout_ms,
        )?;
        let reply = UsbIpImportReply::decode(&reply_bytes)?;
        let device = reply.device.ok_or_else(|| UsbError::Protocol {
            protocol: "usbip",
            reason: "successful import reply carried no device record".to_owned(),
        })?;
        if device.bus_id != attachment.bus_id {
            return Err(UsbError::Protocol {
                protocol: "usbip",
                reason: format!(
                    "import reply bus id {:?} does not match requested {:?}",
                    device.bus_id, attachment.bus_id
                ),
            });
        }
        let remote_devid = (device.busnum << 16) | device.devnum;

        LinuxUsbIpUrbSession::start(
            stream,
            remote_devid,
            self.config.timeout_ms,
            self.config.max_transfer_bytes,
        )
        .map(|session| Arc::new(session) as Arc<dyn UsbIpDataEndpoint>)
    }
}

impl UsbIpKernelDataEndpoint for LinuxUsbIpKernelDataEndpoint {
    fn available(&self) -> bool {
        self.connector.available() && self.config.validate().is_ok()
    }

    fn open(&self, attachment: &UsbIpAttachment) -> Result<Arc<dyn UsbIpDataEndpoint>, UsbError> {
        self.open_authorized(attachment, None, None)
    }
}

fn read_exact_stream(
    stream: &mut dyn UsbIpUrbStream,
    buffer: &mut [u8],
    timeout_ms: u64,
) -> Result<(), UsbError> {
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    let mut filled = 0;
    while filled < buffer.len() {
        if Instant::now() >= deadline {
            return Err(crate::usbip_protocol::UsbIpProtocolError::TimedOut { timeout_ms }.into());
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(UsbError::Protocol {
                    protocol: "usbip",
                    reason: format!(
                        "remote USB/IP server closed the data stream after {filled} of {} bytes",
                        buffer.len()
                    ),
                });
            }
            Ok(read) => filled += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(source) => {
                return Err(UsbError::Io {
                    operation: "read USB/IP import reply",
                    path: std::path::PathBuf::from("<usbip-data-stream>"),
                    source,
                });
            }
        }
    }
    Ok(())
}

#[derive(Debug, Default)]
struct UrbState {
    pending: BTreeMap<u32, Option<Result<UsbIpRetSubmit, UsbIpDataError>>>,
    unlinks: BTreeMap<u32, UnlinkWaiter>,
    closed: bool,
    stream_error: Option<String>,
}

#[derive(Clone, Copy, Debug)]
struct UnlinkWaiter {
    target: u32,
    result: Option<i32>,
}

/// Cumulative session counters; never payload bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpUrbSessionStats {
    /// Submits written to the remote server.
    pub submits: u64,
    /// Matching completions delivered.
    pub completions: u64,
    /// Unlink requests written.
    pub unlinks: u64,
    /// Unlink replies delivered.
    pub unlink_replies: u64,
    /// Requests that hit their deadline.
    pub timeouts: u64,
    /// Completions received without a waiter.
    pub late_completions: u64,
    /// Protocol/I/O failures that closed the session.
    pub errors: u64,
}

#[derive(Debug)]
struct UrbShared {
    state: Mutex<UrbState>,
    changed: Condvar,
    stats: Mutex<UsbIpUrbSessionStats>,
    stop: AtomicBool,
    max_transfer_bytes: usize,
}

impl UrbShared {
    fn new(max_transfer_bytes: usize) -> Self {
        Self {
            state: Mutex::new(UrbState::default()),
            changed: Condvar::new(),
            stats: Mutex::new(UsbIpUrbSessionStats::default()),
            stop: AtomicBool::new(false),
            max_transfer_bytes,
        }
    }

    fn lock_state(&self) -> MutexGuard<'_, UrbState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn lock_stats(&self) -> MutexGuard<'_, UsbIpUrbSessionStats> {
        self.stats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn bump<F: FnOnce(&mut UsbIpUrbSessionStats)>(&self, update: F) {
        update(&mut self.lock_stats());
    }

    fn fail(&self, reason: &str) {
        let mut state = self.lock_state();
        if state.closed {
            return;
        }
        state.closed = true;
        state.stream_error = Some(reason.to_owned());
        for result in state.pending.values_mut() {
            if result.is_none() {
                *result = Some(Err(UsbIpDataError::from_errno(
                    ECONNRESET,
                    "USB/IP data session closed",
                )));
            }
        }
        for waiter in state.unlinks.values_mut() {
            if waiter.result.is_none() {
                waiter.result = Some(ECONNRESET);
            }
        }
        self.changed.notify_all();
        drop(state);
        self.bump(|stats| stats.errors = stats.errors.saturating_add(1));
    }
}

/// One live client-side URB session.
///
/// Submits and unlinks may be called concurrently from the relay's worker
/// threads. One reader thread pairs `USBIP_RET_SUBMIT`/`USBIP_RET_UNLINK`
/// frames with the waiting call by sequence number. No completion is fabricated:
/// a closed stream fails every waiter with `-ECONNRESET`, and a deadline fails
/// only that request with `-ETIMEDOUT`.
pub struct LinuxUsbIpUrbSession {
    remote_devid: u32,
    timeout_ms: u64,
    max_transfer_bytes: usize,
    writer: Mutex<Box<dyn UsbIpUrbStream>>,
    shared: Arc<UrbShared>,
    next_unlink: AtomicU32,
    reader: Mutex<Option<JoinHandle<()>>>,
}

impl std::fmt::Debug for LinuxUsbIpUrbSession {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self.shared.lock_state();
        formatter
            .debug_struct("LinuxUsbIpUrbSession")
            .field("remote_devid", &self.remote_devid)
            .field("timeout_ms", &self.timeout_ms)
            .field("pending", &state.pending.len())
            .field("closed", &state.closed)
            .field("stats", &*self.shared.lock_stats())
            .finish_non_exhaustive()
    }
}

impl LinuxUsbIpUrbSession {
    fn start(
        stream: Box<dyn UsbIpUrbStream>,
        remote_devid: u32,
        timeout_ms: u64,
        max_transfer_bytes: usize,
    ) -> Result<Self, UsbError> {
        let reader_stream = stream.try_clone().map_err(|source| UsbError::Io {
            operation: "clone USB/IP data stream",
            path: std::path::PathBuf::from("<usbip-data-stream>"),
            source,
        })?;
        let shared = Arc::new(UrbShared::new(max_transfer_bytes));
        let reader_shared = Arc::clone(&shared);
        let reader = thread::Builder::new()
            .name("ozma-usbip-tcp-urbs".to_owned())
            .spawn(move || reader_loop(reader_stream, reader_shared))
            .map_err(|source| UsbError::Spawn {
                command: "ozma-usbip-tcp-urbs".to_owned(),
                source,
            })?;
        Ok(Self {
            remote_devid,
            timeout_ms,
            max_transfer_bytes,
            writer: Mutex::new(stream),
            shared,
            next_unlink: AtomicU32::new(0x8000_0000),
            reader: Mutex::new(Some(reader)),
        })
    }

    /// Remote device id advertised by the import reply.
    pub fn remote_devid(&self) -> u32 {
        self.remote_devid
    }

    /// Configured per-request deadline.
    pub fn timeout_ms(&self) -> u64 {
        self.timeout_ms
    }

    /// Cumulative counters.
    pub fn stats(&self) -> UsbIpUrbSessionStats {
        *self.shared.lock_stats()
    }

    /// Whether the session still has a live reader.
    pub fn is_open(&self) -> bool {
        !self.shared.lock_state().closed
    }

    fn next_unlink_seqnum(&self) -> u32 {
        loop {
            let value = self.next_unlink.fetch_add(1, Ordering::Relaxed);
            let state = self.shared.lock_state();
            if !state.pending.contains_key(&value) && !state.unlinks.contains_key(&value) {
                return value;
            }
        }
    }

    fn send_frame(&self, bytes: &[u8]) -> Result<(), UsbIpDataError> {
        let mut writer = self
            .writer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Err(source) = writer.write_all(bytes) {
            let error = UsbIpDataError::from_errno(
                errno_for_io(&source),
                format!("write USB/IP URB frame: {source}"),
            );
            drop(writer);
            self.shared.fail(&error.reason);
            return Err(error);
        }
        if let Err(source) = writer.flush() {
            let error = UsbIpDataError::from_errno(
                errno_for_io(&source),
                format!("flush USB/IP URB frame: {source}"),
            );
            drop(writer);
            self.shared.fail(&error.reason);
            return Err(error);
        }
        Ok(())
    }

    fn wait_submit(&self, seqnum: u32) -> Result<UsbIpRetSubmit, UsbIpDataError> {
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        let mut state = self.shared.lock_state();
        loop {
            if matches!(state.pending.get(&seqnum), Some(Some(_))) {
                let result = state
                    .pending
                    .remove(&seqnum)
                    .expect("presence was checked")
                    .expect("completion was checked");
                if result.is_ok() {
                    drop(state);
                    self.shared
                        .bump(|stats| stats.completions = stats.completions.saturating_add(1));
                }
                return result;
            }
            if state.closed {
                state.pending.remove(&seqnum);
                return Err(UsbIpDataError::from_errno(
                    ECONNRESET,
                    "USB/IP data session closed",
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                state.pending.remove(&seqnum);
                drop(state);
                self.shared
                    .bump(|stats| stats.timeouts = stats.timeouts.saturating_add(1));
                return Err(UsbIpDataError::from_errno(
                    ETIMEDOUT,
                    format!(
                        "USB/IP URB {seqnum} did not complete within {} ms",
                        self.timeout_ms
                    ),
                ));
            }
            let (guard, _) = self
                .shared
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }

    fn wait_unlink(&self, seqnum: u32) -> Result<i32, UsbIpDataError> {
        let deadline = Instant::now() + Duration::from_millis(self.timeout_ms);
        let mut state = self.shared.lock_state();
        loop {
            if let Some(waiter) = state.unlinks.get(&seqnum).copied() {
                if let Some(result) = waiter.result {
                    state.unlinks.remove(&seqnum);
                    drop(state);
                    self.shared.bump(|stats| {
                        stats.unlink_replies = stats.unlink_replies.saturating_add(1)
                    });
                    return Ok(result);
                }
            } else if state.closed {
                return Err(UsbIpDataError::from_errno(
                    ECONNRESET,
                    "USB/IP data session closed",
                ));
            }
            if state.closed {
                state.unlinks.remove(&seqnum);
                return Err(UsbIpDataError::from_errno(
                    ECONNRESET,
                    "USB/IP data session closed",
                ));
            }
            let now = Instant::now();
            if now >= deadline {
                state.unlinks.remove(&seqnum);
                drop(state);
                self.shared
                    .bump(|stats| stats.timeouts = stats.timeouts.saturating_add(1));
                return Err(UsbIpDataError::from_errno(
                    ETIMEDOUT,
                    format!(
                        "USB/IP unlink {seqnum} did not complete within {} ms",
                        self.timeout_ms
                    ),
                ));
            }
            let (guard, _) = self
                .shared
                .changed
                .wait_timeout(state, deadline.saturating_duration_since(now))
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state = guard;
        }
    }
}

impl UsbIpDataEndpoint for LinuxUsbIpUrbSession {
    fn submit(&self, request: &UsbIpCmdSubmit) -> Result<UsbIpRetSubmit, UsbIpDataError> {
        if request.transfer_buffer_length as usize > self.max_transfer_bytes {
            return Err(UsbIpDataError::new(
                EMSGSIZE,
                format!(
                    "transfer_buffer_length {} exceeds {} bytes",
                    request.transfer_buffer_length, self.max_transfer_bytes
                ),
            ));
        }
        let mut outgoing = request.clone();
        outgoing.devid = self.remote_devid;
        let bytes = outgoing.encode().map_err(|error| {
            UsbIpDataError::new(EPROTO, format!("encode USBIP_CMD_SUBMIT: {error}"))
        })?;
        {
            let mut state = self.shared.lock_state();
            if state.closed {
                return Err(UsbIpDataError::from_errno(
                    ECONNRESET,
                    "USB/IP data session closed",
                ));
            }
            if state.pending.contains_key(&request.seqnum) {
                return Err(UsbIpDataError::new(
                    EINVAL,
                    format!("seqnum {} is already in flight", request.seqnum),
                ));
            }
            state.pending.insert(request.seqnum, None);
        }
        if let Err(error) = self.send_frame(&bytes) {
            self.shared.lock_state().pending.remove(&request.seqnum);
            return Err(error);
        }
        self.shared
            .bump(|stats| stats.submits = stats.submits.saturating_add(1));
        let mut completion = self.wait_submit(request.seqnum)?;
        completion.devid = request.devid;
        completion.direction = request.direction;
        completion.ep = request.ep;
        Ok(completion)
    }

    fn unlink(&self, devid: u32, seqnum: u32) -> Result<UsbIpRetUnlink, UsbIpDataError> {
        let unlink_seqnum = self.next_unlink_seqnum();
        {
            let mut state = self.shared.lock_state();
            if state.closed {
                return Err(UsbIpDataError::from_errno(
                    ECONNRESET,
                    "USB/IP data session closed",
                ));
            }
            state.unlinks.insert(
                unlink_seqnum,
                UnlinkWaiter {
                    target: seqnum,
                    result: None,
                },
            );
        }
        let request = UsbIpCmdUnlink::new(unlink_seqnum, self.remote_devid, seqnum);
        let bytes = request.encode().map_err(|error| {
            UsbIpDataError::new(EPROTO, format!("encode USBIP_CMD_UNLINK: {error}"))
        })?;
        if let Err(error) = self.send_frame(&bytes) {
            self.shared.lock_state().unlinks.remove(&unlink_seqnum);
            return Err(error);
        }
        self.shared
            .bump(|stats| stats.unlinks = stats.unlinks.saturating_add(1));
        let status = self.wait_unlink(unlink_seqnum)?;
        Ok(UsbIpRetUnlink::new(seqnum, devid, status))
    }
}

impl Drop for LinuxUsbIpUrbSession {
    fn drop(&mut self) {
        self.shared.stop.store(true, Ordering::SeqCst);
        if let Ok(writer) = self.writer.lock() {
            let _ = writer.shutdown();
        }
        self.shared.fail("USB/IP data session dropped");
        let reader = self
            .reader
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take();
        if let Some(reader) = reader {
            // A shutdown or short read timeout wakes the reader; joining keeps
            // teardown deterministic and avoids a detached thread touching a
            // closed socket.
            let _ = reader.join();
        }
    }
}

fn reader_loop(mut stream: Box<dyn UsbIpUrbStream>, shared: Arc<UrbShared>) {
    loop {
        if shared.stop.load(Ordering::SeqCst) {
            break;
        }
        let mut header = [0u8; USBIP_URB_HEADER_BYTES];
        if !read_exact_reader(&mut *stream, &mut header, &shared) {
            break;
        }
        let frame_len =
            match UsbIpUrb::frame_len_from_header_bounded(&header, shared.max_transfer_bytes) {
                Ok(len) => len,
                Err(error) => {
                    shared.fail(&format!("invalid USB/IP URB header: {error}"));
                    break;
                }
            };
        let mut frame = vec![0u8; frame_len];
        frame[..USBIP_URB_HEADER_BYTES].copy_from_slice(&header);
        if frame_len > USBIP_URB_HEADER_BYTES
            && !read_exact_reader(&mut *stream, &mut frame[USBIP_URB_HEADER_BYTES..], &shared)
        {
            break;
        }
        let urb = match UsbIpUrb::decode_bounded(&frame, shared.max_transfer_bytes) {
            Ok(urb) => urb,
            Err(error) => {
                shared.fail(&format!("invalid USB/IP URB frame: {error}"));
                break;
            }
        };
        match urb {
            UsbIpUrb::RetSubmit(completion) => {
                let mut state = shared.lock_state();
                match state.pending.get_mut(&completion.seqnum) {
                    Some(slot) if slot.is_none() => {
                        *slot = Some(Ok(completion));
                        shared.changed.notify_all();
                    }
                    _ => {
                        drop(state);
                        shared.bump(|stats| {
                            stats.late_completions = stats.late_completions.saturating_add(1)
                        });
                    }
                }
            }
            UsbIpUrb::RetUnlink(reply) => {
                let mut state = shared.lock_state();
                let Some(waiter) = state.unlinks.get_mut(&reply.seqnum) else {
                    continue;
                };
                let target = waiter.target;
                waiter.result = Some(reply.status);
                if reply.status == USBIP_URB_UNLINK_CANCELLED {
                    if let Some(slot) = state.pending.get_mut(&target) {
                        *slot = Some(Err(UsbIpDataError::from_errno(
                            ECONNRESET,
                            "USB/IP URB was unlinked",
                        )));
                    }
                }
                shared.changed.notify_all();
            }
            UsbIpUrb::CmdSubmit(_) | UsbIpUrb::CmdUnlink(_) => {
                shared.fail("remote server sent a client-only URB command");
                break;
            }
        }
    }
    shared.fail("USB/IP data stream reader stopped");
}

fn read_exact_reader(
    stream: &mut dyn UsbIpUrbStream,
    buffer: &mut [u8],
    shared: &UrbShared,
) -> bool {
    let mut filled = 0;
    while filled < buffer.len() {
        if shared.stop.load(Ordering::SeqCst) {
            return false;
        }
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => return false,
            Ok(read) => filled += read,
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::Interrupted
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(_) => return false,
        }
    }
    true
}

fn errno_for_io(error: &io::Error) -> i32 {
    match error.kind() {
        io::ErrorKind::TimedOut | io::ErrorKind::WouldBlock => ETIMEDOUT,
        io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe | io::ErrorKind::UnexpectedEof => {
            ENOTCONN
        }
        _ => EPROTO,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_rejects_zero_deadline_and_port() {
        let mut config = LinuxUsbIpDataConfig::default();
        config.timeout_ms = 0;
        assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");
        let mut config = LinuxUsbIpDataConfig::default();
        config.port = 0;
        assert_eq!(config.validate().unwrap_err().kind(), "invalid-usbip");
    }
}
