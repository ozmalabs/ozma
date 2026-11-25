// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Opt-in, bounded serial/UART console surface.
//!
//! The provider is only created when the operator passes `--serial-device`.
//! Without it every implemented route returns a typed 503
//! `serial.not-configured`; all other historical `/serial/*` sub-surfaces keep
//! their typed 501 `serial.unsupported` body.
//!
//! The implementation is deliberately conservative:
//!
//! * The device path is explicit, absolute, and validated before it is opened.
//!   There is no auto-detection and no probe of unrelated devices.
//! * Line settings (baud/data bits/parity/stop bits/read timeout) are fixed at
//!   startup. The provider opens exactly one device and never changes it.
//! * Received bytes go into a fixed-size ring buffer. Both the retained buffer
//!   and every read response are bounded, so a noisy UART cannot grow memory
//!   without limit.
//! * `POST /serial/write` accepts exactly one bounded `text` or `hex` payload
//!   and writes those exact bytes. It never appends a newline or invents data.
//! * `GET /serial/stream` is an SSE stream over the same bounded buffer. It
//!   ends on an idle timeout or a disconnected reader, and dropping the client
//!   response only unsubscribes that client.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use axum::body::Body;
use axum::extract::{Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use bytes::Bytes;
use futures_util::stream;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use tokio::sync::broadcast;

/// Default serial baud rate, matching the Python `node/serial_capture.py`.
pub const DEFAULT_BAUD_RATE: u32 = 115_200;
/// Default number of received bytes retained for `GET /serial/read`.
pub const DEFAULT_BUFFER_BYTES: usize = 256 * 1024;
/// Largest retained receive buffer accepted on the command line.
pub const MAX_BUFFER_BYTES: usize = 4 * 1024 * 1024;
/// Default maximum bytes returned by one `GET /serial/read`.
pub const DEFAULT_READ_BYTES: usize = 4 * 1024;
/// Hard upper bound for one `GET /serial/read`.
pub const MAX_READ_BYTES: usize = 64 * 1024;
/// Hard upper bound for one `POST /serial/write` payload.
pub const MAX_WRITE_BYTES: usize = 64 * 1024;
/// Default SSE idle timeout.
pub const DEFAULT_STREAM_IDLE_MS: u64 = 15_000;
/// Smallest accepted SSE idle timeout (focused tests use this).
pub const MIN_STREAM_IDLE_MS: u64 = 100;
/// Largest accepted SSE idle timeout.
pub const MAX_STREAM_IDLE_MS: u64 = 120_000;
/// Largest chunk emitted by one SSE data event.
pub const STREAM_CHUNK_BYTES: usize = 8 * 1024;
/// Broadcast wake-ups retained for slow SSE consumers.
const STREAM_SIGNAL_CAPACITY: usize = 32;
/// Reader scratch buffer size. This is independent of the retained buffer.
const READ_CHUNK_BYTES: usize = 4096;
/// Longest error detail copied into a response.
const MAX_ERROR_DETAIL: usize = 400;

/// Serial parity setting.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SerialParity {
    /// No parity bit.
    #[default]
    None,
    /// Odd parity.
    Odd,
    /// Even parity.
    Even,
}

impl SerialParity {
    /// Stable lowercase name used by the CLI, status document, and errors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Odd => "odd",
            Self::Even => "even",
        }
    }

    fn native(self) -> serialport::Parity {
        match self {
            Self::None => serialport::Parity::None,
            Self::Odd => serialport::Parity::Odd,
            Self::Even => serialport::Parity::Even,
        }
    }
}

impl FromStr for SerialParity {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "n" => Ok(Self::None),
            "odd" | "o" => Ok(Self::Odd),
            "even" | "e" => Ok(Self::Even),
            other => Err(format!(
                "unknown serial parity {other:?}; expected none, odd, or even"
            )),
        }
    }
}

/// Serial line settings reported on `GET /serial/status`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SerialLineSettings {
    pub baud_rate: u32,
    pub data_bits: u8,
    pub parity: SerialParity,
    pub stop_bits: u8,
}

impl Default for SerialLineSettings {
    fn default() -> Self {
        Self {
            baud_rate: DEFAULT_BAUD_RATE,
            data_bits: 8,
            parity: SerialParity::None,
            stop_bits: 1,
        }
    }
}

/// Validated serial provider configuration.
#[derive(Clone, Debug)]
pub struct SerialConfig {
    /// Explicit absolute device path.
    pub device: PathBuf,
    pub line: SerialLineSettings,
    /// Blocking read timeout. It also bounds a stalled write.
    pub read_timeout: Duration,
    /// Fixed receive-buffer capacity.
    pub buffer_bytes: usize,
    /// Maximum bytes returned by one read request.
    pub max_read_bytes: usize,
    /// Maximum bytes accepted by one write request.
    pub max_write_bytes: usize,
    /// Default SSE idle timeout.
    pub stream_idle: Duration,
}

impl SerialConfig {
    /// Build a config with the default buffer/read/write/stream bounds.
    pub fn new(
        device: impl Into<PathBuf>,
        baud_rate: u32,
        data_bits: u8,
        parity: SerialParity,
        stop_bits: u8,
        read_timeout: Duration,
    ) -> Result<Self, SerialError> {
        let config = Self {
            device: device.into(),
            line: SerialLineSettings {
                baud_rate,
                data_bits,
                parity,
                stop_bits,
            },
            read_timeout,
            buffer_bytes: DEFAULT_BUFFER_BYTES,
            max_read_bytes: DEFAULT_READ_BYTES,
            max_write_bytes: MAX_WRITE_BYTES,
            stream_idle: Duration::from_millis(DEFAULT_STREAM_IDLE_MS),
        };
        config.validate()?;
        Ok(config)
    }

    /// Validate every setting before a device is opened.
    pub fn validate(&self) -> Result<(), SerialError> {
        validate_device_path(&self.device)?;
        if !(50..=4_000_000).contains(&self.line.baud_rate) {
            return Err(SerialError::InvalidConfig {
                detail: "baud rate must be between 50 and 4000000".to_owned(),
            });
        }
        if !(5..=8).contains(&self.line.data_bits) {
            return Err(SerialError::InvalidConfig {
                detail: "data bits must be between 5 and 8".to_owned(),
            });
        }
        if !(1..=2).contains(&self.line.stop_bits) {
            return Err(SerialError::InvalidConfig {
                detail: "stop bits must be 1 or 2".to_owned(),
            });
        }
        if self.read_timeout.is_zero() || self.read_timeout > Duration::from_secs(10) {
            return Err(SerialError::InvalidConfig {
                detail: "read timeout must be between 1 ms and 10 s".to_owned(),
            });
        }
        if !(1024..=MAX_BUFFER_BYTES).contains(&self.buffer_bytes) {
            return Err(SerialError::InvalidConfig {
                detail: format!("buffer must be between 1024 and {MAX_BUFFER_BYTES} bytes"),
            });
        }
        if !(1..=MAX_READ_BYTES).contains(&self.max_read_bytes) {
            return Err(SerialError::InvalidConfig {
                detail: format!("maximum read must be between 1 and {MAX_READ_BYTES} bytes"),
            });
        }
        if !(1..=MAX_WRITE_BYTES).contains(&self.max_write_bytes) {
            return Err(SerialError::InvalidConfig {
                detail: format!("maximum write must be between 1 and {MAX_WRITE_BYTES} bytes"),
            });
        }
        let idle = self.stream_idle.as_millis() as u64;
        if !(MIN_STREAM_IDLE_MS..=MAX_STREAM_IDLE_MS).contains(&idle) {
            return Err(SerialError::InvalidConfig {
                detail: format!(
                    "stream idle timeout must be between {MIN_STREAM_IDLE_MS} and {MAX_STREAM_IDLE_MS} ms"
                ),
            });
        }
        Ok(())
    }
}

fn validate_device_path(path: &Path) -> Result<(), SerialError> {
    if path.as_os_str().is_empty() {
        return Err(SerialError::InvalidDevice {
            reason: "device path is empty".to_owned(),
        });
    }
    if !path.is_absolute() {
        return Err(SerialError::InvalidDevice {
            reason: "device path must be absolute".to_owned(),
        });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::ParentDir | Component::CurDir))
    {
        return Err(SerialError::InvalidDevice {
            reason: "device path must not contain '.' or '..' components".to_owned(),
        });
    }
    if path
        .components()
        .any(|component| matches!(component, Component::RootDir | Component::Normal(_)))
        == false
    {
        return Err(SerialError::InvalidDevice {
            reason: "device path must name a filesystem entry".to_owned(),
        });
    }
    Ok(())
}

/// Typed serial failure, serialized with a stable `serial.*` code.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SerialError {
    #[error("serial is not configured on this node")]
    NotConfigured,
    #[error("invalid serial device path: {reason}")]
    InvalidDevice { reason: String },
    #[error("the serial device {path} does not exist")]
    DeviceMissing { path: String },
    #[error("permission denied opening serial device {path}")]
    PermissionDenied { path: String },
    #[error("the serial device {path} is busy or no longer available")]
    DeviceBusy { path: String },
    #[error("the serial device is not connected")]
    NotConnected,
    #[error("send exactly one non-empty text or hex payload")]
    InvalidPayload,
    #[error("serial payload exceeds the {max}-byte limit")]
    PayloadTooLarge { max: usize },
    #[error("invalid hex payload: {detail}")]
    InvalidHex { detail: String },
    #[error("max must be between 1 and {max}")]
    InvalidLimit { max: usize },
    #[error("the read cursor is beyond the buffered data")]
    InvalidCursor,
    #[error("serial write failed: {detail}")]
    WriteFailed { detail: String },
    #[error("serial read failed: {detail}")]
    ReadFailed { detail: String },
    #[error("invalid serial configuration: {detail}")]
    InvalidConfig { detail: String },
}

impl SerialError {
    /// Stable machine-readable error code.
    pub fn code(&self) -> &'static str {
        match self {
            Self::NotConfigured => "serial.not-configured",
            Self::InvalidDevice { .. } => "serial.invalid-device",
            Self::DeviceMissing { .. } => "serial.device-missing",
            Self::PermissionDenied { .. } => "serial.permission-denied",
            Self::DeviceBusy { .. } => "serial.device-busy",
            Self::NotConnected => "serial.not-connected",
            Self::InvalidPayload => "serial.invalid-payload",
            Self::PayloadTooLarge { .. } => "serial.payload-too-large",
            Self::InvalidHex { .. } => "serial.invalid-hex",
            Self::InvalidLimit { .. } => "serial.invalid-limit",
            Self::InvalidCursor => "serial.invalid-cursor",
            Self::WriteFailed { .. } => "serial.write-failed",
            Self::ReadFailed { .. } => "serial.read-failed",
            Self::InvalidConfig { .. } => "serial.invalid-config",
        }
    }

    /// HTTP status for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotConfigured | Self::DeviceMissing { .. } | Self::NotConnected => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::PermissionDenied { .. } => StatusCode::FORBIDDEN,
            Self::DeviceBusy { .. } => StatusCode::CONFLICT,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::WriteFailed { .. } | Self::ReadFailed { .. } => StatusCode::BAD_GATEWAY,
            Self::InvalidDevice { .. }
            | Self::InvalidPayload
            | Self::InvalidHex { .. }
            | Self::InvalidLimit { .. }
            | Self::InvalidCursor
            | Self::InvalidConfig { .. } => StatusCode::BAD_REQUEST,
        }
    }

    /// Operator-facing remediation text.
    pub fn remediation(&self) -> &'static str {
        match self {
            Self::NotConfigured => {
                "Start the node with --serial-device PATH to enable /serial/status, /serial/write, /serial/read, and /serial/stream."
            }
            Self::InvalidDevice { .. } | Self::InvalidConfig { .. } => {
                "Use an explicit absolute device path and valid line settings; restart the node after fixing the flags."
            }
            Self::DeviceMissing { .. } => {
                "Check --serial-device; no serial device is opened or fabricated when the path is absent."
            }
            Self::PermissionDenied { .. } => {
                "Grant the node user read/write access to the serial device (for example via its group), or choose a different device."
            }
            Self::DeviceBusy { .. } => {
                "Stop the other process using this UART/USB-serial device, then restart the node."
            }
            Self::NotConnected => {
                "Read GET /serial/status.last_error; reconnect the device and restart the node."
            }
            Self::InvalidPayload | Self::InvalidHex { .. } => {
                "Send exactly one of {\"text\": \"...\"} or {\"hex\": \"0a0d\"}."
            }
            Self::PayloadTooLarge { .. } => "Send a smaller write payload.",
            Self::InvalidLimit { .. } => "Use a bounded max value within the advertised limit.",
            Self::InvalidCursor => "Use the next cursor returned by the previous read.",
            Self::WriteFailed { .. } | Self::ReadFailed { .. } => {
                "Inspect GET /serial/status.last_error and the node log; reconnect the device if it was removed."
            }
        }
    }

    fn body(&self) -> Value {
        json!({
            "ok": false,
            "supported": true,
            "configured": !matches!(self, Self::NotConfigured),
            "available": false,
            "code": self.code(),
            "error": self.to_string(),
            "remediation": self.remediation(),
        })
    }
}

impl IntoResponse for SerialError {
    fn into_response(self) -> Response {
        let status = self.status();
        let mut response = (status, Json(self.body())).into_response();
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
        response
    }
}

/// `last_error` object carried by `GET /serial/status`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SerialStatusError {
    pub code: String,
    pub message: String,
    pub remediation: String,
}

impl From<&SerialError> for SerialStatusError {
    fn from(error: &SerialError) -> Self {
        Self {
            code: error.code().to_owned(),
            message: error.to_string(),
            remediation: error.remediation().to_owned(),
        }
    }
}

/// Bounded receive-buffer facts included in status/read responses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SerialBufferStatus {
    /// Absolute offset of the oldest retained byte.
    pub start: u64,
    /// Absolute offset just past the newest received byte.
    pub end: u64,
    /// Number of bytes currently retained.
    pub bytes: usize,
    /// Maximum retained bytes.
    pub capacity: usize,
}

/// `GET /serial/status` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SerialStatus {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub connected: bool,
    pub state: &'static str,
    pub device: String,
    pub line: SerialLineSettings,
    pub read_timeout_ms: u64,
    pub bytes_in: u64,
    pub bytes_out: u64,
    pub buffer: SerialBufferStatus,
    pub limits: SerialLimits,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<SerialStatusError>,
}

/// Advertised bounds for read/write/stream.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
pub struct SerialLimits {
    pub max_read_bytes: usize,
    pub max_write_bytes: usize,
    pub default_stream_idle_ms: u64,
    pub max_stream_idle_ms: u64,
}

/// `GET /serial/read` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SerialRead {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub connected: bool,
    /// Absolute offset of the first returned byte.
    pub offset: u64,
    /// Cursor to pass as `since` to continue after this response.
    pub next: u64,
    /// Bytes returned in this response.
    pub bytes: usize,
    /// Base64 of the exact bytes.
    pub data: String,
    pub encoding: &'static str,
    /// True when the requested cursor predated the retained buffer.
    pub dropped: bool,
    /// True when more bytes are buffered after `next`.
    pub has_more: bool,
    pub buffer: SerialBufferStatus,
}

/// `POST /serial/write` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SerialWrite {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub connected: bool,
    pub bytes: usize,
    pub encoding: &'static str,
    pub bytes_out: u64,
    pub buffer: SerialBufferStatus,
}

/// Request body for `POST /serial/write`.
#[derive(Debug, Default, Deserialize)]
pub struct SerialWriteRequest {
    #[serde(default)]
    pub text: Option<String>,
    #[serde(default)]
    pub hex: Option<String>,
}

impl SerialWriteRequest {
    fn into_bytes(self, max: usize) -> Result<(Vec<u8>, &'static str), SerialError> {
        match (self.text, self.hex) {
            (Some(_), Some(_)) | (None, None) => Err(SerialError::InvalidPayload),
            (Some(text), None) => {
                let bytes = text.into_bytes();
                if bytes.len() > max {
                    return Err(SerialError::PayloadTooLarge { max });
                }
                Ok((bytes, "text"))
            }
            (None, Some(hex)) => {
                let bytes = parse_hex(&hex, max)?;
                Ok((bytes, "hex"))
            }
        }
    }
}

fn parse_hex(input: &str, max: usize) -> Result<Vec<u8>, SerialError> {
    let mut cleaned = String::with_capacity(input.len());
    for character in input.chars() {
        if character.is_ascii_whitespace() || matches!(character, ':' | '-') {
            continue;
        }
        if !character.is_ascii_hexdigit() {
            return Err(SerialError::InvalidHex {
                detail: format!("unexpected character {character:?}"),
            });
        }
        cleaned.push(character);
        if cleaned.len() > max.saturating_mul(2) {
            return Err(SerialError::PayloadTooLarge { max });
        }
    }
    if cleaned.len() % 2 != 0 {
        return Err(SerialError::InvalidHex {
            detail: "hex payload has an odd number of digits".to_owned(),
        });
    }
    let mut bytes = Vec::with_capacity(cleaned.len() / 2);
    for pair in cleaned.as_bytes().chunks_exact(2) {
        let pair = std::str::from_utf8(pair).expect("ASCII hex digits");
        let byte = u8::from_str_radix(pair, 16).map_err(|_| SerialError::InvalidHex {
            detail: "hex payload contains a non-hex pair".to_owned(),
        })?;
        bytes.push(byte);
    }
    Ok(bytes)
}

/// Fixed-capacity receive ring keyed by absolute byte offsets.
#[derive(Debug)]
struct SerialBuffer {
    data: VecDeque<u8>,
    start: u64,
    capacity: usize,
}

#[derive(Debug)]
struct BufferRead {
    bytes: Vec<u8>,
    offset: u64,
    next: u64,
    dropped: bool,
    has_more: bool,
    status: SerialBufferStatus,
}

impl SerialBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            data: VecDeque::with_capacity(capacity.min(64 * 1024)),
            start: 0,
            capacity,
        }
    }

    fn status(&self) -> SerialBufferStatus {
        SerialBufferStatus {
            start: self.start,
            end: self.start + self.data.len() as u64,
            bytes: self.data.len(),
            capacity: self.capacity,
        }
    }

    fn push(&mut self, bytes: &[u8]) {
        if self.capacity == 0 {
            return;
        }
        if bytes.len() >= self.capacity {
            let tail = &bytes[bytes.len() - self.capacity..];
            self.start += (self.data.len() + bytes.len() - self.capacity) as u64;
            self.data.clear();
            self.data.extend(tail.iter().copied());
            return;
        }
        let overflow = (self.data.len() + bytes.len()).saturating_sub(self.capacity);
        if overflow > 0 {
            let drop = overflow.min(self.data.len());
            self.data.drain(..drop);
            self.start += drop as u64;
        }
        self.data.extend(bytes.iter().copied());
    }

    fn read(&self, since: Option<u64>, max: usize) -> Result<BufferRead, SerialError> {
        let status = self.status();
        let (offset, dropped) = match since {
            None => (status.start, false),
            Some(cursor) => {
                if cursor > status.end {
                    return Err(SerialError::InvalidCursor);
                }
                (cursor.max(status.start), cursor < status.start)
            }
        };
        let index = offset.saturating_sub(status.start) as usize;
        let available = self.data.len().saturating_sub(index);
        let take = available.min(max);
        let bytes = self
            .data
            .iter()
            .skip(index)
            .take(take)
            .copied()
            .collect::<Vec<_>>();
        let next = offset + take as u64;
        Ok(BufferRead {
            bytes,
            offset,
            next,
            dropped,
            has_more: next < status.end,
            status,
        })
    }
}

struct SerialInner {
    config: SerialConfig,
    buffer: Mutex<SerialBuffer>,
    writer: Mutex<Option<Box<dyn serialport::SerialPort>>>,
    connected: AtomicBool,
    started: AtomicBool,
    shutdown: AtomicBool,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    last_error: Mutex<Option<SerialStatusError>>,
    signal: broadcast::Sender<()>,
}

/// Shared, opt-in serial provider.
#[derive(Clone)]
pub struct SerialService {
    inner: Arc<SerialInner>,
}

impl SerialService {
    /// Build a service. This never opens the device; call [`Self::start`].
    pub fn new(config: SerialConfig) -> Self {
        let (signal, _) = broadcast::channel(STREAM_SIGNAL_CAPACITY);
        Self {
            inner: Arc::new(SerialInner {
                buffer: Mutex::new(SerialBuffer::new(config.buffer_bytes)),
                config,
                writer: Mutex::new(None),
                connected: AtomicBool::new(false),
                started: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                bytes_in: AtomicU64::new(0),
                bytes_out: AtomicU64::new(0),
                last_error: Mutex::new(None),
                signal,
            }),
        }
    }

    /// Configured device path.
    pub fn device(&self) -> &Path {
        &self.inner.config.device
    }

    /// Open the configured device once and start the background reader.
    ///
    /// A failure is retained as `last_error`; the service stays installed so
    /// `GET /serial/status` can report the typed reason and the other routes
    /// return `serial.not-connected`.
    pub fn start(&self) -> Result<(), SerialError> {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        let port = match self.open() {
            Ok(port) => port,
            Err(error) => {
                self.set_error(&error);
                return Err(error);
            }
        };
        let writer = match port.try_clone() {
            Ok(writer) => writer,
            Err(error) => {
                let error = map_open_error(self.device(), &error);
                self.set_error(&error);
                return Err(error);
            }
        };
        *lock(&self.inner.writer) = Some(writer);
        *lock(&self.inner.last_error) = None;
        self.inner.connected.store(true, Ordering::SeqCst);
        let inner = self.inner.clone();
        std::thread::Builder::new()
            .name("ozma-serial".to_owned())
            .spawn(move || read_loop(inner, port))
            .map_err(|error| {
                let error = SerialError::ReadFailed {
                    detail: bounded(&error.to_string()),
                };
                self.set_error(&error);
                self.inner.connected.store(false, Ordering::SeqCst);
                *lock(&self.inner.writer) = None;
                error
            })?;
        Ok(())
    }

    /// Stop the reader after its current bounded read timeout.
    pub fn stop(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
    }

    fn open(&self) -> Result<Box<dyn serialport::SerialPort>, SerialError> {
        validate_device_path(self.device())?;
        match std::fs::metadata(self.device()) {
            Ok(metadata) if metadata.is_dir() => {
                return Err(SerialError::InvalidDevice {
                    reason: "device path is a directory".to_owned(),
                });
            }
            Ok(_) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Err(SerialError::DeviceMissing {
                    path: self.device().display().to_string(),
                });
            }
            Err(error) => {
                return Err(map_io_error(self.device(), &error));
            }
        }
        let line = self.inner.config.line;
        let data_bits = match line.data_bits {
            5 => serialport::DataBits::Five,
            6 => serialport::DataBits::Six,
            7 => serialport::DataBits::Seven,
            _ => serialport::DataBits::Eight,
        };
        let stop_bits = if line.stop_bits == 2 {
            serialport::StopBits::Two
        } else {
            serialport::StopBits::One
        };
        serialport::new(self.device().to_string_lossy(), line.baud_rate)
            .data_bits(data_bits)
            .parity(line.parity.native())
            .stop_bits(stop_bits)
            .timeout(self.inner.config.read_timeout)
            .open()
            .map_err(|error| map_open_error(self.device(), &error))
    }

    fn set_error(&self, error: &SerialError) {
        *lock(&self.inner.last_error) = Some(SerialStatusError::from(error));
    }

    /// Current device/line/buffer state.
    pub fn status(&self) -> SerialStatus {
        let connected = self.inner.connected.load(Ordering::SeqCst);
        let last_error = lock(&self.inner.last_error).clone();
        let state = if connected {
            "connected"
        } else if last_error.is_some() {
            "failed"
        } else {
            "disconnected"
        };
        SerialStatus {
            ok: true,
            supported: true,
            configured: true,
            connected,
            state,
            device: self.device().display().to_string(),
            line: self.inner.config.line,
            read_timeout_ms: self.inner.config.read_timeout.as_millis() as u64,
            bytes_in: self.inner.bytes_in.load(Ordering::SeqCst),
            bytes_out: self.inner.bytes_out.load(Ordering::SeqCst),
            buffer: lock(&self.inner.buffer).status(),
            limits: SerialLimits {
                max_read_bytes: self.inner.config.max_read_bytes,
                max_write_bytes: self.inner.config.max_write_bytes,
                default_stream_idle_ms: self.inner.config.stream_idle.as_millis() as u64,
                max_stream_idle_ms: MAX_STREAM_IDLE_MS,
            },
            last_error,
        }
    }

    /// Bounded buffered read. Reading is allowed after a disconnect so an
    /// operator can still retrieve the tail of a crashed target's console.
    pub fn read(&self, max: Option<usize>, since: Option<u64>) -> Result<SerialRead, SerialError> {
        let max = match max {
            None => self.inner.config.max_read_bytes.min(DEFAULT_READ_BYTES),
            Some(0) => {
                return Err(SerialError::InvalidLimit {
                    max: self.inner.config.max_read_bytes,
                })
            }
            Some(max) if max > self.inner.config.max_read_bytes => {
                return Err(SerialError::InvalidLimit {
                    max: self.inner.config.max_read_bytes,
                })
            }
            Some(max) => max,
        };
        let read = lock(&self.inner.buffer).read(since, max)?;
        Ok(SerialRead {
            ok: true,
            supported: true,
            configured: true,
            connected: self.inner.connected.load(Ordering::SeqCst),
            offset: read.offset,
            next: read.next,
            bytes: read.bytes.len(),
            data: BASE64.encode(&read.bytes),
            encoding: "base64",
            dropped: read.dropped,
            has_more: read.has_more,
            buffer: read.status,
        })
    }

    /// Write exactly the bytes from one bounded text or hex payload.
    pub fn write(&self, request: SerialWriteRequest) -> Result<SerialWrite, SerialError> {
        if !self.inner.connected.load(Ordering::SeqCst) {
            return Err(SerialError::NotConnected);
        }
        let (bytes, encoding) = request.into_bytes(self.inner.config.max_write_bytes)?;
        let mut writer = lock(&self.inner.writer);
        let Some(port) = writer.as_mut() else {
            return Err(SerialError::NotConnected);
        };
        port.write_all(&bytes)
            .map_err(|error| SerialError::WriteFailed {
                detail: bounded(&error.to_string()),
            })?;
        port.flush().map_err(|error| SerialError::WriteFailed {
            detail: bounded(&error.to_string()),
        })?;
        let bytes_out = self
            .inner
            .bytes_out
            .fetch_add(bytes.len() as u64, Ordering::SeqCst)
            + bytes.len() as u64;
        Ok(SerialWrite {
            ok: true,
            supported: true,
            configured: true,
            connected: true,
            bytes: bytes.len(),
            encoding,
            bytes_out,
            buffer: lock(&self.inner.buffer).status(),
        })
    }

    fn subscribe(&self) -> broadcast::Receiver<()> {
        self.inner.signal.subscribe()
    }

    fn connected(&self) -> bool {
        self.inner.connected.load(Ordering::SeqCst)
    }

    fn last_error(&self) -> Option<SerialStatusError> {
        lock(&self.inner.last_error).clone()
    }

    fn stream_cursor(&self) -> u64 {
        lock(&self.inner.buffer).status().end
    }

    fn buffer_read(&self, cursor: u64, max: usize) -> Result<BufferRead, SerialError> {
        lock(&self.inner.buffer).read(Some(cursor), max)
    }

    #[cfg(test)]
    fn fake(config: SerialConfig, buffered: &[u8], connected: bool) -> Self {
        let service = Self::new(config);
        lock(&service.inner.buffer).push(buffered);
        service.inner.connected.store(connected, Ordering::SeqCst);
        service
    }
}

fn read_loop(inner: Arc<SerialInner>, mut port: Box<dyn serialport::SerialPort>) {
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    while !inner.shutdown.load(Ordering::SeqCst) {
        match port.read(&mut chunk) {
            Ok(0) => {
                *lock(&inner.last_error) =
                    Some(SerialStatusError::from(&SerialError::ReadFailed {
                        detail: "device returned end-of-file".to_owned(),
                    }));
                break;
            }
            Ok(read) => {
                lock(&inner.buffer).push(&chunk[..read]);
                inner.bytes_in.fetch_add(read as u64, Ordering::SeqCst);
                let _ = inner.signal.send(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                *lock(&inner.last_error) =
                    Some(SerialStatusError::from(&SerialError::ReadFailed {
                        detail: bounded(&error.to_string()),
                    }));
                break;
            }
        }
    }
    inner.connected.store(false, Ordering::SeqCst);
    *lock(&inner.writer) = None;
    let _ = inner.signal.send(());
}

fn map_open_error(path: &Path, error: &serialport::Error) -> SerialError {
    use serialport::ErrorKind;
    let path = path.display().to_string();
    match error.kind {
        ErrorKind::NoDevice => {
            if Path::new(&path).exists() {
                SerialError::DeviceBusy { path }
            } else {
                SerialError::DeviceMissing { path }
            }
        }
        ErrorKind::InvalidInput => SerialError::InvalidDevice {
            reason: bounded(&error.description),
        },
        ErrorKind::Io(kind) => map_io_kind(path, kind, &error.description),
        ErrorKind::Unknown => SerialError::ReadFailed {
            detail: bounded(&error.description),
        },
    }
}

fn map_io_error(path: &Path, error: &std::io::Error) -> SerialError {
    map_io_kind(path.display().to_string(), error.kind(), &error.to_string())
}

fn map_io_kind(path: String, kind: std::io::ErrorKind, detail: &str) -> SerialError {
    match kind {
        std::io::ErrorKind::NotFound => SerialError::DeviceMissing { path },
        std::io::ErrorKind::PermissionDenied => SerialError::PermissionDenied { path },
        std::io::ErrorKind::ResourceBusy
        | std::io::ErrorKind::AlreadyExists
        | std::io::ErrorKind::WouldBlock => SerialError::DeviceBusy { path },
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::InvalidData => {
            SerialError::InvalidDevice {
                reason: bounded(detail),
            }
        }
        _ => SerialError::ReadFailed {
            detail: bounded(detail),
        },
    }
}

fn bounded(value: &str) -> String {
    value.chars().take(MAX_ERROR_DETAIL).collect()
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug, Default, Deserialize)]
struct SerialReadQuery {
    #[serde(default)]
    max: Option<usize>,
    #[serde(default)]
    since: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
struct SerialStreamQuery {
    #[serde(default)]
    since: Option<u64>,
    #[serde(default)]
    idle_ms: Option<u64>,
}

struct StreamState {
    service: SerialService,
    receiver: broadcast::Receiver<()>,
    cursor: u64,
    idle: Duration,
    gap: bool,
}

/// Router for the opt-in serial surface. Mount it in the guarded compatibility
/// router; with no service every implemented route answers a typed 503.
pub fn router(service: Option<SerialService>) -> Router {
    Router::new()
        .route("/serial/status", get(status_handler))
        .route("/serial/write", post(write_handler))
        .route("/serial/read", get(read_handler))
        .route("/serial/stream", get(stream_handler))
        .with_state(SerialRouteState { service })
}

#[derive(Clone)]
struct SerialRouteState {
    service: Option<SerialService>,
}

fn json_no_store(status: StatusCode, body: Value) -> Response {
    let mut response = (status, Json(body)).into_response();
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, "no-store".parse().unwrap());
    response
}

fn not_configured() -> Response {
    SerialError::NotConfigured.into_response()
}

async fn status_handler(State(state): State<SerialRouteState>) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match serde_json::to_value(service.status()) {
        Ok(body) => json_no_store(StatusCode::OK, body),
        Err(_) => SerialError::ReadFailed {
            detail: "could not serialize status".to_owned(),
        }
        .into_response(),
    }
}

async fn write_handler(
    State(state): State<SerialRouteState>,
    body: Option<Json<SerialWriteRequest>>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    let Some(Json(request)) = body else {
        return SerialError::InvalidPayload.into_response();
    };
    let result = tokio::task::spawn_blocking(move || service.write(request)).await;
    match result {
        Ok(Ok(write)) => match serde_json::to_value(write) {
            Ok(body) => json_no_store(StatusCode::OK, body),
            Err(_) => SerialError::WriteFailed {
                detail: "could not serialize response".to_owned(),
            }
            .into_response(),
        },
        Ok(Err(error)) => error.into_response(),
        Err(error) => SerialError::WriteFailed {
            detail: bounded(&error.to_string()),
        }
        .into_response(),
    }
}

async fn read_handler(
    State(state): State<SerialRouteState>,
    Query(query): Query<SerialReadQuery>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    match service.read(query.max, query.since) {
        Ok(read) => match serde_json::to_value(read) {
            Ok(body) => json_no_store(StatusCode::OK, body),
            Err(_) => SerialError::ReadFailed {
                detail: "could not serialize response".to_owned(),
            }
            .into_response(),
        },
        Err(error) => error.into_response(),
    }
}

async fn stream_handler(
    State(state): State<SerialRouteState>,
    Query(query): Query<SerialStreamQuery>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    let idle_ms = query
        .idle_ms
        .unwrap_or(service.inner.config.stream_idle.as_millis() as u64);
    if !(MIN_STREAM_IDLE_MS..=MAX_STREAM_IDLE_MS).contains(&idle_ms) {
        return SerialError::InvalidConfig {
            detail: format!(
                "stream idle_ms must be between {MIN_STREAM_IDLE_MS} and {MAX_STREAM_IDLE_MS}"
            ),
        }
        .into_response();
    }
    let receiver = service.subscribe();
    let end = service.stream_cursor();
    let start = service.status().buffer.start;
    let (cursor, gap) = match query.since {
        None => (end, false),
        Some(cursor) if cursor > end => return SerialError::InvalidCursor.into_response(),
        Some(cursor) => (cursor.max(start), cursor < start),
    };
    let stream = stream::unfold(
        Some(StreamState {
            service,
            receiver,
            cursor,
            idle: Duration::from_millis(idle_ms),
            gap,
        }),
        next_stream_event,
    );
    let body = Body::from_stream(stream);
    (
        [
            (header::CONTENT_TYPE, "text/event-stream"),
            (header::CACHE_CONTROL, "no-store"),
            (header::CONNECTION, "keep-alive"),
        ],
        body,
    )
        .into_response()
}

async fn next_stream_event(
    state: Option<StreamState>,
) -> Option<(Result<Bytes, Infallible>, Option<StreamState>)> {
    let mut state = state?;
    loop {
        if state.gap {
            state.gap = false;
            let status = state.service.status().buffer;
            let event = sse_event(
                "gap",
                &json!({
                    "offset": state.cursor,
                    "buffer_start": status.start,
                    "buffer_end": status.end,
                    "dropped": true,
                }),
            );
            return Some((Ok(event), Some(state)));
        }
        match state.service.buffer_read(state.cursor, STREAM_CHUNK_BYTES) {
            Ok(read) if !read.bytes.is_empty() => {
                state.cursor = read.next;
                let event = sse_event(
                    "data",
                    &json!({
                        "offset": read.offset,
                        "next": read.next,
                        "bytes": read.bytes.len(),
                        "encoding": "base64",
                        "data": BASE64.encode(&read.bytes),
                        "buffer_start": read.status.start,
                        "buffer_end": read.status.end,
                    }),
                );
                return Some((Ok(event), Some(state)));
            }
            Ok(_) => {}
            Err(error) => {
                let event = sse_event(
                    "error",
                    &json!({
                        "code": error.code(),
                        "error": error.to_string(),
                    }),
                );
                return Some((Ok(event), None));
            }
        }
        if !state.service.connected() {
            let event = sse_event(
                "disconnected",
                &json!({
                    "last_error": state.service.last_error(),
                }),
            );
            return Some((Ok(event), None));
        }
        tokio::select! {
            result = state.receiver.recv() => match result {
                Ok(()) | Err(broadcast::error::RecvError::Lagged(_)) => continue,
                Err(broadcast::error::RecvError::Closed) => return None,
            },
            () = tokio::time::sleep(state.idle) => {
                let event = sse_event(
                    "idle",
                    &json!({"idle_ms": state.idle.as_millis() as u64}),
                );
                return Some((Ok(event), None));
            }
        }
    }
}

fn sse_event(event: &str, data: &Value) -> Bytes {
    Bytes::from(format!("event: {event}\ndata: {data}\n\n"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::Request;
    use http_body_util::BodyExt;
    use std::process::{Child, Command, Stdio};
    use std::time::Instant;
    use tempfile::TempDir;
    use tower::ServiceExt;

    fn test_config(device: impl Into<PathBuf>) -> SerialConfig {
        let mut config = SerialConfig::new(
            device,
            115_200,
            8,
            SerialParity::None,
            1,
            Duration::from_millis(50),
        )
        .expect("valid test config");
        config.buffer_bytes = 64 * 1024;
        config.stream_idle = Duration::from_millis(400);
        config
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("GET")
                    .uri(uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Vec<u8>) {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(uri)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = response.status();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        (status, bytes.to_vec())
    }

    fn body_json(bytes: &[u8]) -> Value {
        serde_json::from_slice(bytes).expect("JSON body")
    }

    #[test]
    fn device_and_line_settings_are_validated_before_open() {
        let relative = SerialConfig::new(
            "ttyUSB0",
            115_200,
            8,
            SerialParity::None,
            1,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(relative.code(), "serial.invalid-device");

        let traversal = SerialConfig::new(
            "/dev/../ttyUSB0",
            115_200,
            8,
            SerialParity::None,
            1,
            Duration::from_millis(50),
        )
        .unwrap_err();
        assert_eq!(traversal.code(), "serial.invalid-device");

        for (baud, bits, stop) in [(0, 8, 1), (115_200, 9, 1), (115_200, 8, 3)] {
            let error = SerialConfig::new(
                "/dev/ttyUSB0",
                baud,
                bits,
                SerialParity::None,
                stop,
                Duration::from_millis(50),
            )
            .unwrap_err();
            assert_eq!(error.code(), "serial.invalid-config");
        }
        assert_eq!(
            "ODD".parse::<SerialParity>().unwrap(),
            SerialParity::Odd,
            "parity parsing is case-insensitive"
        );
    }

    #[test]
    fn text_and_hex_payloads_are_exact_and_bounded() {
        let (bytes, encoding) = SerialWriteRequest {
            text: Some("AT\r\n".to_owned()),
            hex: None,
        }
        .into_bytes(32)
        .unwrap();
        assert_eq!(bytes, b"AT\r\n");
        assert_eq!(encoding, "text");

        let (bytes, encoding) = SerialWriteRequest {
            text: None,
            hex: Some("41 54 0d:0a".to_owned()),
        }
        .into_bytes(32)
        .unwrap();
        assert_eq!(bytes, b"AT\r\n");
        assert_eq!(encoding, "hex");

        assert_eq!(
            SerialWriteRequest {
                text: Some("a".to_owned()),
                hex: Some("61".to_owned()),
            }
            .into_bytes(32)
            .unwrap_err()
            .code(),
            "serial.invalid-payload"
        );
        assert_eq!(
            SerialWriteRequest::default()
                .into_bytes(32)
                .unwrap_err()
                .code(),
            "serial.invalid-payload"
        );
        assert_eq!(
            SerialWriteRequest {
                text: None,
                hex: Some("abc".to_owned()),
            }
            .into_bytes(32)
            .unwrap_err()
            .code(),
            "serial.invalid-hex"
        );
        assert_eq!(
            SerialWriteRequest {
                text: Some("0123456789".to_owned()),
                hex: None,
            }
            .into_bytes(4)
            .unwrap_err()
            .code(),
            "serial.payload-too-large"
        );
    }

    #[test]
    fn ring_buffer_is_bounded_and_cursor_reads_are_exact() {
        let mut buffer = SerialBuffer::new(8);
        buffer.push(b"abcdef");
        assert_eq!(buffer.status().start, 0);
        assert_eq!(buffer.status().end, 6);
        let first = buffer.read(None, 4).unwrap();
        assert_eq!(first.bytes, b"abcd");
        assert_eq!(first.next, 4);
        assert!(first.has_more);
        let second = buffer.read(Some(first.next), 4).unwrap();
        assert_eq!(second.bytes, b"ef");
        assert!(!second.has_more);

        buffer.push(b"ghijkl");
        let status = buffer.status();
        assert_eq!(status.bytes, 8, "retained bytes never exceed capacity");
        assert_eq!(status.start, 4);
        let dropped = buffer.read(Some(0), 8).unwrap();
        assert!(dropped.dropped);
        assert_eq!(dropped.offset, status.start);
        assert_eq!(dropped.bytes, b"efghijkl");
        assert_eq!(
            buffer.read(Some(status.end + 1), 8).unwrap_err().code(),
            "serial.invalid-cursor"
        );
    }

    #[test]
    fn open_error_mapping_distinguishes_missing_permission_and_busy() {
        let missing_path = "/tmp/ozma-serial-does-not-exist";
        let missing = map_open_error(
            Path::new(missing_path),
            &serialport::Error::new(serialport::ErrorKind::NoDevice, "gone"),
        );
        assert_eq!(missing.code(), "serial.device-missing");
        assert_eq!(missing.status(), StatusCode::SERVICE_UNAVAILABLE);

        let permission = map_io_kind(
            "/dev/ttyS0".to_owned(),
            std::io::ErrorKind::PermissionDenied,
            "denied",
        );
        assert_eq!(permission.code(), "serial.permission-denied");
        assert_eq!(permission.status(), StatusCode::FORBIDDEN);

        let busy = map_io_kind(
            "/dev/ttyS0".to_owned(),
            std::io::ErrorKind::ResourceBusy,
            "busy",
        );
        assert_eq!(busy.code(), "serial.device-busy");
        assert_eq!(busy.status(), StatusCode::CONFLICT);
    }

    #[tokio::test]
    async fn not_configured_and_residual_sub_surfaces_are_typed() {
        let app = router(None);
        let (status, body) = get(&app, "/serial/status").await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(&body)["code"], "serial.not-configured");

        for (method, uri) in [
            ("POST", "/serial/write"),
            ("GET", "/serial/read"),
            ("GET", "/serial/stream"),
        ] {
            let response = app
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(uri)
                        .header("content-type", "application/json")
                        .body(Body::from("{}"))
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE, "{uri}");
        }
    }

    #[tokio::test]
    async fn fake_backend_status_read_and_write_validate_bounds() {
        let mut config = test_config("/dev/fake-serial");
        config.max_read_bytes = 8;
        config.max_write_bytes = 4;
        let service = SerialService::fake(config, b"boot-ok\n", true);
        let app = router(Some(service));

        let (status, body) = get(&app, "/serial/status").await;
        assert_eq!(status, StatusCode::OK);
        let status = body_json(&body);
        assert_eq!(status["configured"], true);
        assert_eq!(status["connected"], true);
        assert_eq!(status["device"], "/dev/fake-serial");
        assert_eq!(status["line"]["baud_rate"], 115_200);
        assert_eq!(status["bytes_in"], 0);

        let (status, body) = get(&app, "/serial/read?max=8").await;
        assert_eq!(status, StatusCode::OK);
        let read = body_json(&body);
        assert_eq!(read["bytes"], 8);
        assert_eq!(
            BASE64.decode(read["data"].as_str().unwrap()).unwrap(),
            b"boot-ok\n"
        );

        let (status, body) = get(&app, "/serial/read?max=9").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body_json(&body)["code"], "serial.invalid-limit");

        let (status, body) = get(&app, "/serial/read?since=999").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body_json(&body)["code"], "serial.invalid-cursor");

        let (status, body) = post(&app, "/serial/write", json!({"text": "toolong"})).await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(body_json(&body)["code"], "serial.payload-too-large");
    }

    struct SocatPair {
        child: Child,
        _dir: TempDir,
        left: PathBuf,
        right: PathBuf,
    }

    impl SocatPair {
        fn spawn() -> Option<Self> {
            let dir = tempfile::tempdir().ok()?;
            let left = dir.path().join("serial-a");
            let right = dir.path().join("serial-b");
            let child = Command::new("socat")
                .arg("-d")
                .arg("-d")
                .arg(format!("pty,raw,echo=0,link={}", left.display()))
                .arg(format!("pty,raw,echo=0,link={}", right.display()))
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .ok()?;
            let pair = Self {
                child,
                _dir: dir,
                left,
                right,
            };
            if pair.wait_for_links(Duration::from_secs(2)) {
                Some(pair)
            } else {
                None
            }
        }

        fn wait_for_links(&self, timeout: Duration) -> bool {
            let deadline = Instant::now() + timeout;
            while Instant::now() < deadline {
                if self.left.exists() && self.right.exists() {
                    return true;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            false
        }

        fn peer(&self) -> std::fs::File {
            std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(&self.right)
                .expect("open socat peer")
        }
    }

    impl Drop for SocatPair {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    fn wait_for(timeout: Duration, mut condition: impl FnMut() -> bool) -> bool {
        let deadline = Instant::now() + timeout;
        while Instant::now() < deadline {
            if condition() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        false
    }

    #[tokio::test]
    async fn live_socat_pty_status_write_read_stream_and_disconnect() {
        let Some(pair) = SocatPair::spawn() else {
            eprintln!("skipping live serial test: socat is unavailable");
            return;
        };
        let config = test_config(&pair.left);
        let service = SerialService::new(config);
        service.start().expect("open socat PTY");
        assert!(
            wait_for(Duration::from_secs(2), || service.status().connected),
            "service should connect to socat PTY"
        );
        let app = router(Some(service.clone()));
        let mut peer = pair.peer();

        let (status, body) = get(&app, "/serial/status").await;
        assert_eq!(status, StatusCode::OK);
        let status = body_json(&body);
        assert_eq!(status["connected"], true);
        assert_eq!(status["device"], pair.left.display().to_string());
        assert_eq!(status["line"]["data_bits"], 8);
        assert_eq!(status["line"]["parity"], "none");
        assert_eq!(status["line"]["stop_bits"], 1);

        let (status, body) = post(&app, "/serial/write", json!({"text": "ping\n"})).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        assert_eq!(body_json(&body)["bytes"], 5);
        let mut echoed = [0u8; 5];
        peer.read_exact(&mut echoed).expect("peer receives write");
        assert_eq!(&echoed, b"ping\n");

        peer.write_all(b"boot-ok\n").expect("peer writes");
        peer.flush().unwrap();
        assert!(
            wait_for(Duration::from_secs(2), || service.status().bytes_in >= 8),
            "service should read from socat PTY"
        );
        let (status, body) = get(&app, "/serial/read?max=64&since=0").await;
        assert_eq!(status, StatusCode::OK);
        let read = body_json(&body);
        assert_eq!(
            BASE64.decode(read["data"].as_str().unwrap()).unwrap(),
            b"boot-ok\n"
        );
        assert_eq!(read["next"], 8);

        // Start a live stream at the current end, then write into the peer.
        let since = read["next"].as_u64().unwrap();
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/serial/stream?since={since}&idle_ms=300"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let writer = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            peer.write_all(b"live\n").expect("peer live write");
            peer.flush().unwrap();
            peer
        });
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: data"), "{text}");
        let data_line = text
            .lines()
            .find(|line| line.starts_with("data: {") && line.contains("live"))
            .or_else(|| text.lines().find(|line| line.starts_with("data: {")))
            .expect("stream data event");
        let json: Value = serde_json::from_str(data_line.strip_prefix("data: ").unwrap()).unwrap();
        let payload = BASE64.decode(json["data"].as_str().unwrap()).unwrap();
        assert!(payload.windows(4).any(|window| window == b"live"), "{text}");
        let _peer = writer.join().unwrap();

        // Killing socat closes the device; the reader must observe it and the
        // stream must terminate with a typed disconnected event.
        drop(app);
        drop(pair);
        assert!(
            wait_for(Duration::from_secs(2), || !service.status().connected),
            "reader should mark the device disconnected"
        );
        let app = router(Some(service.clone()));
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/serial/stream?idle_ms=100")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        let bytes = response.into_body().collect().await.unwrap().to_bytes();
        let text = String::from_utf8_lossy(&bytes);
        assert!(text.contains("event: disconnected"), "{text}");
        let status = service.status();
        assert!(!status.connected);
        assert_eq!(status.state, "failed");
        assert!(status.last_error.is_some());
    }

    #[tokio::test]
    async fn missing_device_is_typed_and_status_stays_observable() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("missing-serial");
        let service = SerialService::new(test_config(&missing));
        let error = service.start().unwrap_err();
        assert_eq!(error.code(), "serial.device-missing");
        let app = router(Some(service));
        let (status, body) = get(&app, "/serial/status").await;
        assert_eq!(status, StatusCode::OK);
        let status = body_json(&body);
        assert_eq!(status["configured"], true);
        assert_eq!(status["connected"], false);
        assert_eq!(status["state"], "failed");
        assert_eq!(status["last_error"]["code"], "serial.device-missing");

        let (status, body) = post(&app, "/serial/write", json!({"hex": "00"})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(&body)["code"], "serial.not-connected");
    }
}
