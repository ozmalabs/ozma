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
//!   There is no auto-detection, no probe of unrelated devices, and no fallback
//!   to a different path.
//! * Line settings (baud/data bits/parity/stop bits/flow control/read timeout)
//!   and optional initial DTR/RTS levels are applied on every open. A driver
//!   that cannot honour flow control or line control returns a typed error.
//! * Received bytes go into a fixed-size ring buffer. Both the retained buffer
//!   and every read response are bounded, so a noisy UART cannot grow memory
//!   without limit.
//! * `POST /serial/write` accepts exactly one bounded `text` or `hex` payload
//!   and writes those exact bytes. It never appends a newline or invents data.
//! * `GET /serial/stream` is an SSE stream over the same bounded buffer. It
//!   ends on an idle timeout or a disconnected reader, and dropping the client
//!   response only unsubscribes that client.
//! * A removal/read error closes the port and retries the same path with a
//!   bounded exponential backoff. Retained bytes are cleared on reconnect and
//!   the retry counter only resets after a stable connection.
//! * `POST /serial/control` is opt-in and can change DTR/RTS only.

use std::collections::VecDeque;
use std::convert::Infallible;
use std::io::{self, Read, Write};
use std::path::{Component, Path, PathBuf};
use std::str::FromStr;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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
/// Default reconnect attempts after a removal/read error.
pub const DEFAULT_RECONNECT_ATTEMPTS: u32 = 5;
/// Default first reconnect delay.
pub const DEFAULT_RECONNECT_MIN_MS: u64 = 1_000;
/// Default reconnect delay cap.
pub const DEFAULT_RECONNECT_MAX_MS: u64 = 30_000;
/// Largest accepted reconnect attempt count.
pub const MAX_RECONNECT_ATTEMPTS: u32 = 100;
/// A connection that lasts this long resets the reconnect attempt counter.
///
/// Without this a device that opens and immediately fails would reset the
/// counter every time and produce an unbounded reconnect storm.
const STABLE_CONNECTION: Duration = Duration::from_secs(5);
/// Granularity of the interruptible backoff sleep.
const BACKOFF_POLL: Duration = Duration::from_millis(25);
/// HARD bound for `POST /serial/control` request bodies.
const MAX_CONTROL_BODY_BYTES: usize = 1024;

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

/// Serial flow-control mode applied when the device is opened.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum SerialFlowControl {
    /// No hardware or software flow control.
    #[default]
    None,
    /// Hardware RTS/CTS flow control.
    RtsCts,
    /// Software XON/XOFF flow control.
    XonXoff,
}

impl SerialFlowControl {
    /// Stable lowercase name used by the CLI, status document, and errors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::RtsCts => "rtscts",
            Self::XonXoff => "xonxoff",
        }
    }

    fn native(self) -> serialport::FlowControl {
        match self {
            Self::None => serialport::FlowControl::None,
            Self::RtsCts => serialport::FlowControl::Hardware,
            Self::XonXoff => serialport::FlowControl::Software,
        }
    }
}

impl FromStr for SerialFlowControl {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "none" | "off" => Ok(Self::None),
            "rtscts" | "rts-cts" | "hardware" | "hw" => Ok(Self::RtsCts),
            "xonxoff" | "xon-xoff" | "software" | "sw" => Ok(Self::XonXoff),
            other => Err(format!(
                "unknown serial flow control {other:?}; expected none, rtscts, or xonxoff"
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

/// Bounded reconnect policy after a device removal or read error.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct SerialReconnectConfig {
    /// Retries after one failure. Zero disables reconnect entirely.
    pub max_attempts: u32,
    /// First retry delay; doubles for each subsequent attempt.
    pub min_delay: Duration,
    /// Upper bound for one retry delay.
    pub max_delay: Duration,
}

impl Default for SerialReconnectConfig {
    fn default() -> Self {
        Self {
            max_attempts: DEFAULT_RECONNECT_ATTEMPTS,
            min_delay: Duration::from_millis(DEFAULT_RECONNECT_MIN_MS),
            max_delay: Duration::from_millis(DEFAULT_RECONNECT_MAX_MS),
        }
    }
}

/// Validated serial provider configuration.
#[derive(Clone, Debug)]
pub struct SerialConfig {
    /// Explicit absolute device path.
    pub device: PathBuf,
    pub line: SerialLineSettings,
    /// Flow control applied on every open.
    pub flow_control: SerialFlowControl,
    /// Initial DTR level requested on open; `None` leaves it untouched.
    pub initial_dtr: Option<bool>,
    /// Initial RTS level requested on open; `None` leaves it untouched.
    pub initial_rts: Option<bool>,
    /// Opt in to `POST /serial/control` (DTR/RTS only).
    pub control_enabled: bool,
    /// Bounded reconnect policy after removal/read error.
    pub reconnect: SerialReconnectConfig,
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
            flow_control: SerialFlowControl::None,
            initial_dtr: None,
            initial_rts: None,
            control_enabled: false,
            reconnect: SerialReconnectConfig::default(),
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
        if self.reconnect.max_attempts > MAX_RECONNECT_ATTEMPTS {
            return Err(SerialError::InvalidConfig {
                detail: format!(
                    "reconnect attempts must be between 0 and {MAX_RECONNECT_ATTEMPTS}"
                ),
            });
        }
        let min_ms = self.reconnect.min_delay.as_millis() as u64;
        let max_ms = self.reconnect.max_delay.as_millis() as u64;
        if !(1..=60_000).contains(&min_ms) {
            return Err(SerialError::InvalidConfig {
                detail: "reconnect minimum delay must be between 1 and 60000 ms".to_owned(),
            });
        }
        if !(min_ms..=300_000).contains(&max_ms) {
            return Err(SerialError::InvalidConfig {
                detail:
                    "reconnect maximum delay must be at least the minimum and at most 300000 ms"
                        .to_owned(),
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
    #[error("serial flow control {flow_control} is not supported by {path}")]
    FlowControlUnsupported { path: String, flow_control: String },
    #[error("serial line control for {line} is not supported by this device")]
    LineControlUnsupported { line: String },
    #[error("serial reconnect failed after {attempts} attempts: {detail}")]
    ReconnectExhausted { attempts: u32, detail: String },
    #[error("POST /serial/control is disabled; restart with --serial-control to enable it")]
    ControlDisabled,
    #[error("serial control requires exactly one or both of `dtr` and `rts` booleans")]
    InvalidControl,
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
            Self::FlowControlUnsupported { .. } => "serial.flow-control-unsupported",
            Self::LineControlUnsupported { .. } => "serial.line-control-unsupported",
            Self::ReconnectExhausted { .. } => "serial.reconnect-exhausted",
            Self::ControlDisabled => "serial.control-disabled",
            Self::InvalidControl => "serial.invalid-control",
            Self::InvalidConfig { .. } => "serial.invalid-config",
        }
    }

    /// HTTP status for this failure.
    pub fn status(&self) -> StatusCode {
        match self {
            Self::NotConfigured | Self::DeviceMissing { .. } | Self::NotConnected => {
                StatusCode::SERVICE_UNAVAILABLE
            }
            Self::ReconnectExhausted { .. } => StatusCode::SERVICE_UNAVAILABLE,
            Self::PermissionDenied { .. } | Self::ControlDisabled => StatusCode::FORBIDDEN,
            Self::DeviceBusy { .. } => StatusCode::CONFLICT,
            Self::PayloadTooLarge { .. } => StatusCode::PAYLOAD_TOO_LARGE,
            Self::WriteFailed { .. } | Self::ReadFailed { .. } => StatusCode::BAD_GATEWAY,
            Self::FlowControlUnsupported { .. } | Self::LineControlUnsupported { .. } => {
                StatusCode::NOT_IMPLEMENTED
            }
            Self::InvalidDevice { .. }
            | Self::InvalidPayload
            | Self::InvalidHex { .. }
            | Self::InvalidLimit { .. }
            | Self::InvalidCursor
            | Self::InvalidControl
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
                "Check --serial-device; reconnect retries the same explicit path, and no other device is opened."
            }
            Self::PermissionDenied { .. } => {
                "Grant the node user read/write access to the serial device (for example via its group), or choose a different device."
            }
            Self::DeviceBusy { .. } => {
                "Stop the other process using this UART/USB-serial device; the node retries the same path on a bounded backoff."
            }
            Self::NotConnected => {
                "Read GET /serial/status.last_error and reconnect.state; the node retries the configured path on a bounded backoff."
            }
            Self::InvalidPayload | Self::InvalidHex { .. } => {
                "Send exactly one of {\"text\": \"...\"} or {\"hex\": \"0a0d\"}."
            }
            Self::PayloadTooLarge { .. } => "Send a smaller write payload.",
            Self::InvalidLimit { .. } => "Use a bounded max value within the advertised limit.",
            Self::InvalidCursor => "Use the next cursor returned by the previous read.",
            Self::WriteFailed { .. } | Self::ReadFailed { .. } => {
                "Inspect GET /serial/status.last_error and the node log; the node retries the same path after a bounded backoff."
            }
            Self::FlowControlUnsupported { .. } => {
                "Choose --serial-flow-control none, or a device/driver that implements the requested RTS/CTS or XON/XOFF mode."
            }
            Self::LineControlUnsupported { .. } => {
                "Use a device that exposes DTR/RTS modem control, or remove --serial-dtr/--serial-rts and the control request."
            }
            Self::ReconnectExhausted { .. } => {
                "Fix the explicit device path and restart the node, or raise --serial-reconnect-attempts within its advertised bound."
            }
            Self::ControlDisabled => {
                "Restart the node with --serial-control to allow POST /serial/control to change DTR/RTS."
            }
            Self::InvalidControl => "Send {\"dtr\": true|false} and/or {\"rts\": true|false}; no other fields are accepted.",
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

    /// True when retrying the same explicit path can plausibly succeed.
    fn retryable(&self) -> bool {
        matches!(
            self,
            Self::DeviceMissing { .. }
                | Self::DeviceBusy { .. }
                | Self::NotConnected
                | Self::WriteFailed { .. }
                | Self::ReadFailed { .. }
        )
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

/// Modem control-line state. Input pins are `null` when the device or driver
/// cannot report them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SerialLineStates {
    /// Last commanded DTR output level, or `null` when never set.
    pub dtr: Option<bool>,
    /// Last commanded RTS output level, or `null` when never set.
    pub rts: Option<bool>,
    /// Clear To Send input.
    pub cts: Option<bool>,
    /// Data Set Ready input.
    pub dsr: Option<bool>,
    /// Data Carrier Detect input.
    pub dcd: Option<bool>,
    /// Ring Indicator input.
    pub ri: Option<bool>,
}

/// Reconnect state included in `GET /serial/status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct SerialReconnectStatus {
    /// True when a non-zero retry budget is configured and shutdown has not
    /// stopped the reader.
    pub enabled: bool,
    /// Failures since the last stable connection.
    pub attempt: u32,
    /// Configured retry budget after one failure.
    pub max_attempts: u32,
    /// True while the reader sleeps before the next open attempt.
    pub backoff: bool,
    /// Delay of the current/most recent backoff, in milliseconds.
    pub next_retry_ms: Option<u64>,
    /// True when the retry budget was exhausted; no further opens happen.
    pub exhausted: bool,
    /// Successful opens after the initial attempt (lifetime).
    pub reconnects: u64,
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
    /// Flow control applied on every open.
    pub flow_control: SerialFlowControl,
    /// True when `POST /serial/control` is enabled.
    pub control_enabled: bool,
    /// Current modem control-line levels.
    pub lines: SerialLineStates,
    /// Reconnect state and configured retry budget.
    pub reconnect: SerialReconnectStatus,
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

/// `POST /serial/control` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct SerialControl {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub connected: bool,
    /// DTR level applied by this request, if requested.
    pub dtr: Option<bool>,
    /// RTS level applied by this request, if requested.
    pub rts: Option<bool>,
    pub lines: SerialLineStates,
}

/// Request body for `POST /serial/control`.
#[derive(Debug, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SerialControlRequest {
    #[serde(default)]
    pub dtr: Option<bool>,
    #[serde(default)]
    pub rts: Option<bool>,
}

impl SerialControlRequest {
    fn changes(&self) -> Result<(Option<bool>, Option<bool>), SerialError> {
        if self.dtr.is_none() && self.rts.is_none() {
            return Err(SerialError::InvalidControl);
        }
        Ok((self.dtr, self.rts))
    }
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

    /// Discard retained data after a reconnect while keeping offsets monotonic.
    fn reset(&mut self) {
        self.start += self.data.len() as u64;
        self.data.clear();
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

/// Minimal serial-port surface used by the provider and its fake test backend.
///
/// Keeping this narrower than `serialport::SerialPort` makes modem control and
/// reconnect behaviour testable without hardware while production still uses
/// the real crate.
trait SerialPortOps: Send {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize>;
    fn write_all(&mut self, buf: &[u8]) -> io::Result<()>;
    fn flush(&mut self) -> io::Result<()>;
    fn try_clone_box(&self) -> io::Result<Box<dyn SerialPortOps>>;
    fn set_dtr(&mut self, level: bool) -> Result<(), serialport::Error>;
    fn set_rts(&mut self, level: bool) -> Result<(), serialport::Error>;
    fn read_pin(&mut self, pin: SerialPin) -> Result<bool, serialport::Error>;
    fn clear_buffers(&mut self);
}

#[derive(Clone, Copy, Debug)]
enum SerialPin {
    Cts,
    Dsr,
    Dcd,
    Ri,
}

type OpenFn = dyn Fn(&SerialConfig) -> Result<Box<dyn SerialPortOps>, SerialError> + Send + Sync;

struct RealSerialPort {
    port: Box<dyn serialport::SerialPort>,
}

impl SerialPortOps for RealSerialPort {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.port.read(buf)
    }

    fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
        self.port.write_all(buf)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.port.flush()
    }

    fn try_clone_box(&self) -> io::Result<Box<dyn SerialPortOps>> {
        self.port
            .try_clone()
            .map(|port| Box::new(RealSerialPort { port }) as Box<dyn SerialPortOps>)
            .map_err(io::Error::from)
    }

    fn set_dtr(&mut self, level: bool) -> Result<(), serialport::Error> {
        self.port.write_data_terminal_ready(level)
    }

    fn set_rts(&mut self, level: bool) -> Result<(), serialport::Error> {
        self.port.write_request_to_send(level)
    }

    fn read_pin(&mut self, pin: SerialPin) -> Result<bool, serialport::Error> {
        match pin {
            SerialPin::Cts => self.port.read_clear_to_send(),
            SerialPin::Dsr => self.port.read_data_set_ready(),
            SerialPin::Dcd => self.port.read_carrier_detect(),
            SerialPin::Ri => self.port.read_ring_indicator(),
        }
    }

    fn clear_buffers(&mut self) {
        let _ = self.port.clear(serialport::ClearBuffer::All);
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DesiredLines {
    dtr: Option<bool>,
    rts: Option<bool>,
}

/// Mutable reconnect state shared by the worker and `GET /serial/status`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct ReconnectRuntime {
    attempt: u32,
    backoff: bool,
    next_retry_ms: Option<u64>,
    exhausted: bool,
}

struct SerialInner {
    config: SerialConfig,
    opener: Arc<OpenFn>,
    buffer: Mutex<SerialBuffer>,
    writer: Mutex<Option<Box<dyn SerialPortOps>>>,
    desired_lines: Mutex<DesiredLines>,
    reconnect: Mutex<ReconnectRuntime>,
    worker: Mutex<Option<std::thread::JoinHandle<()>>>,
    connected: AtomicBool,
    started: AtomicBool,
    shutdown: AtomicBool,
    bytes_in: AtomicU64,
    bytes_out: AtomicU64,
    reconnects: AtomicU64,
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
        Self::build(config, Arc::new(open_real))
    }

    fn build(config: SerialConfig, opener: Arc<OpenFn>) -> Self {
        let (signal, _) = broadcast::channel(STREAM_SIGNAL_CAPACITY);
        let desired = DesiredLines {
            dtr: config.initial_dtr,
            rts: config.initial_rts,
        };
        Self {
            inner: Arc::new(SerialInner {
                buffer: Mutex::new(SerialBuffer::new(config.buffer_bytes)),
                config,
                opener,
                writer: Mutex::new(None),
                desired_lines: Mutex::new(desired),
                reconnect: Mutex::new(ReconnectRuntime::default()),
                worker: Mutex::new(None),
                connected: AtomicBool::new(false),
                started: AtomicBool::new(false),
                shutdown: AtomicBool::new(false),
                bytes_in: AtomicU64::new(0),
                bytes_out: AtomicU64::new(0),
                reconnects: AtomicU64::new(0),
                last_error: Mutex::new(None),
                signal,
            }),
        }
    }

    /// Configured device path.
    pub fn device(&self) -> &Path {
        &self.inner.config.device
    }

    /// True when `POST /serial/control` was opted into.
    pub fn control_enabled(&self) -> bool {
        self.inner.config.control_enabled
    }

    /// Open the configured device once and start the background reader.
    ///
    /// A retryable open failure is retained as `last_error`; when reconnect is
    /// enabled the worker retries the same explicit path on a bounded backoff.
    /// The service stays installed so `GET /serial/status` can report the typed
    /// reason and the other routes return `serial.not-connected`.
    pub fn start(&self) -> Result<(), SerialError> {
        if self.inner.started.swap(true, Ordering::SeqCst) {
            return Ok(());
        }
        if self.inner.shutdown.load(Ordering::SeqCst) {
            return Err(SerialError::NotConnected);
        }

        let mut initial_reader = None;
        let mut initial_error = None;
        match (self.inner.opener)(&self.inner.config) {
            Ok(port) => match install_session(&self.inner, port, false) {
                Ok(reader) => initial_reader = Some(reader),
                Err(error) => {
                    set_error_inner(&self.inner, &error);
                    initial_error = Some(error);
                }
            },
            Err(error) => {
                set_error_inner(&self.inner, &error);
                initial_error = Some(error);
            }
        }

        let retry = initial_error.as_ref().is_some_and(SerialError::retryable)
            && self.inner.config.reconnect.max_attempts > 0;
        let should_spawn = initial_reader.is_some() || retry;
        if should_spawn {
            let inner = self.inner.clone();
            let failed = initial_error.is_some();
            match std::thread::Builder::new()
                .name("ozma-serial".to_owned())
                .spawn(move || serial_worker(inner, initial_reader, failed))
            {
                Ok(handle) => *lock(&self.inner.worker) = Some(handle),
                Err(error) => {
                    let error = SerialError::ReadFailed {
                        detail: bounded(&error.to_string()),
                    };
                    self.inner.connected.store(false, Ordering::SeqCst);
                    *lock(&self.inner.writer) = None;
                    set_error_inner(&self.inner, &error);
                    return Err(error);
                }
            }
        }

        match initial_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    /// Stop the reader promptly and wait for the worker thread to exit.
    ///
    /// The backoff sleep is interruptible, so shutdown does not wait for the
    /// next retry interval. A blocking read is bounded by the configured
    /// read timeout.
    pub fn stop(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        let _ = self.inner.signal.send(());
        if let Some(handle) = lock(&self.inner.worker).take() {
            let _ = handle.join();
        }
        self.inner.connected.store(false, Ordering::SeqCst);
        *lock(&self.inner.writer) = None;
        let mut reconnect = lock(&self.inner.reconnect);
        reconnect.backoff = false;
        reconnect.next_retry_ms = None;
    }

    /// Current device/line/buffer state.
    pub fn status(&self) -> SerialStatus {
        let connected = self.inner.connected.load(Ordering::SeqCst);
        let last_error = lock(&self.inner.last_error).clone();
        let runtime = *lock(&self.inner.reconnect);
        let reconnect = SerialReconnectStatus {
            enabled: self.inner.config.reconnect.max_attempts > 0
                && !self.inner.shutdown.load(Ordering::SeqCst),
            attempt: runtime.attempt,
            max_attempts: self.inner.config.reconnect.max_attempts,
            backoff: runtime.backoff,
            next_retry_ms: runtime.next_retry_ms,
            exhausted: runtime.exhausted,
            reconnects: self.inner.reconnects.load(Ordering::SeqCst),
        };
        let state = if connected {
            "connected"
        } else if self.inner.shutdown.load(Ordering::SeqCst) {
            "stopped"
        } else if reconnect.exhausted {
            "failed"
        } else if reconnect.attempt > 0 || reconnect.backoff {
            "reconnecting"
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
            flow_control: self.inner.config.flow_control,
            control_enabled: self.inner.config.control_enabled,
            lines: self.line_states(),
            reconnect,
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

    /// Current modem control-line levels. DTR/RTS report the last commanded
    /// output level; input pins are read from the open device when possible.
    fn line_states(&self) -> SerialLineStates {
        let desired = *lock(&self.inner.desired_lines);
        let mut lines = SerialLineStates {
            dtr: desired.dtr,
            rts: desired.rts,
            ..SerialLineStates::default()
        };
        if let Some(port) = lock(&self.inner.writer).as_mut() {
            lines.cts = port.read_pin(SerialPin::Cts).ok();
            lines.dsr = port.read_pin(SerialPin::Dsr).ok();
            lines.dcd = port.read_pin(SerialPin::Dcd).ok();
            lines.ri = port.read_pin(SerialPin::Ri).ok();
        }
        lines
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

    /// Set DTR and/or RTS on the open device. Opt-in via `--serial-control`.
    pub fn control(&self, request: SerialControlRequest) -> Result<SerialControl, SerialError> {
        if !self.inner.config.control_enabled {
            return Err(SerialError::ControlDisabled);
        }
        let (dtr, rts) = request.changes()?;
        if !self.inner.connected.load(Ordering::SeqCst) {
            return Err(SerialError::NotConnected);
        }
        let mut applied = DesiredLines::default();
        let mut failure = None;
        {
            let mut writer = lock(&self.inner.writer);
            let port = writer.as_mut().ok_or(SerialError::NotConnected)?;
            if let Some(level) = dtr {
                match port.set_dtr(level) {
                    Ok(()) => applied.dtr = Some(level),
                    Err(error) => failure = Some(map_line_error("DTR", &error)),
                }
            }
            if failure.is_none() {
                if let Some(level) = rts {
                    match port.set_rts(level) {
                        Ok(()) => applied.rts = Some(level),
                        Err(error) => failure = Some(map_line_error("RTS", &error)),
                    }
                }
            }
        }
        {
            let mut desired = lock(&self.inner.desired_lines);
            if applied.dtr.is_some() {
                desired.dtr = applied.dtr;
            }
            if applied.rts.is_some() {
                desired.rts = applied.rts;
            }
        }
        if let Some(error) = failure {
            return Err(error);
        }
        Ok(SerialControl {
            ok: true,
            supported: true,
            configured: true,
            connected: true,
            dtr,
            rts,
            lines: self.line_states(),
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

    #[cfg(test)]
    fn with_opener(config: SerialConfig, opener: Arc<OpenFn>) -> Self {
        Self::build(config, opener)
    }
}

fn install_session(
    inner: &SerialInner,
    mut port: Box<dyn SerialPortOps>,
    reconnect: bool,
) -> Result<Box<dyn SerialPortOps>, SerialError> {
    port.clear_buffers();
    let desired = *lock(&inner.desired_lines);
    apply_serial_lines(port.as_mut(), desired.dtr, desired.rts)?;
    let writer = port
        .try_clone_box()
        .map_err(|error| SerialError::ReadFailed {
            detail: bounded(&error.to_string()),
        })?;
    *lock(&inner.writer) = Some(writer);
    *lock(&inner.last_error) = None;
    inner.connected.store(true, Ordering::SeqCst);
    {
        let mut runtime = lock(&inner.reconnect);
        runtime.attempt = 0;
        runtime.backoff = false;
        runtime.next_retry_ms = None;
        runtime.exhausted = false;
    }
    if reconnect {
        lock(&inner.buffer).reset();
        inner.reconnects.fetch_add(1, Ordering::SeqCst);
    }
    let _ = inner.signal.send(());
    Ok(port)
}

fn serial_worker(
    inner: Arc<SerialInner>,
    initial: Option<Box<dyn SerialPortOps>>,
    initial_failed: bool,
) {
    let mut reader = initial;
    let mut failures = u32::from(initial_failed);
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let mut port = match reader.take() {
            Some(port) => port,
            None => {
                let max_attempts = inner.config.reconnect.max_attempts;
                if failures > max_attempts {
                    mark_exhausted(&inner, failures - 1, max_attempts);
                    break;
                }
                let delay = backoff_delay(&inner.config.reconnect, failures.max(1));
                {
                    let mut runtime = lock(&inner.reconnect);
                    runtime.attempt = failures;
                    runtime.backoff = true;
                    runtime.next_retry_ms = Some(delay.as_millis() as u64);
                }
                let _ = inner.signal.send(());
                let woke = sleep_interruptible(&inner, delay);
                {
                    let mut runtime = lock(&inner.reconnect);
                    runtime.backoff = false;
                    runtime.next_retry_ms = None;
                }
                let _ = inner.signal.send(());
                if !woke || inner.shutdown.load(Ordering::SeqCst) {
                    break;
                }
                match (inner.opener)(&inner.config) {
                    Ok(port) => match install_session(&inner, port, true) {
                        Ok(port) => {
                            failures = 0;
                            port
                        }
                        Err(error) => {
                            set_error_inner(&inner, &error);
                            if !error.retryable() {
                                mark_stopped(&inner);
                                break;
                            }
                            failures += 1;
                            continue;
                        }
                    },
                    Err(error) => {
                        set_error_inner(&inner, &error);
                        if !error.retryable() {
                            mark_stopped(&inner);
                            break;
                        }
                        failures += 1;
                        continue;
                    }
                }
            }
        };

        let started = Instant::now();
        let error = read_until_error(&inner, port.as_mut());
        inner.connected.store(false, Ordering::SeqCst);
        *lock(&inner.writer) = None;
        let _ = inner.signal.send(());
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        if let Some(error) = error {
            set_error_inner(&inner, &error);
        }
        if started.elapsed() >= STABLE_CONNECTION {
            failures = 0;
        }
        failures += 1;
    }

    {
        let mut runtime = lock(&inner.reconnect);
        runtime.backoff = false;
        runtime.next_retry_ms = None;
    }
    let _ = inner.signal.send(());
}

fn read_until_error(inner: &SerialInner, port: &mut dyn SerialPortOps) -> Option<SerialError> {
    let mut chunk = vec![0u8; READ_CHUNK_BYTES];
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return None;
        }
        match port.read(&mut chunk) {
            Ok(0) => {
                return Some(SerialError::ReadFailed {
                    detail: "device returned end-of-file".to_owned(),
                })
            }
            Ok(read) => {
                lock(&inner.buffer).push(&chunk[..read]);
                inner.bytes_in.fetch_add(read as u64, Ordering::SeqCst);
                let _ = inner.signal.send(());
            }
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::TimedOut
                        | io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                return Some(SerialError::ReadFailed {
                    detail: bounded(&error.to_string()),
                })
            }
        }
    }
}

fn backoff_delay(policy: &SerialReconnectConfig, attempt: u32) -> Duration {
    let exponent = attempt.saturating_sub(1).min(31);
    let factor = 1u64 << exponent;
    let min_ms = policy.min_delay.as_millis() as u64;
    let max_ms = policy.max_delay.as_millis() as u64;
    Duration::from_millis(min_ms.saturating_mul(factor).min(max_ms))
}

fn sleep_interruptible(inner: &SerialInner, duration: Duration) -> bool {
    let deadline = Instant::now() + duration;
    loop {
        if inner.shutdown.load(Ordering::SeqCst) {
            return false;
        }
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return true;
        }
        std::thread::sleep(remaining.min(BACKOFF_POLL));
    }
}

fn mark_exhausted(inner: &SerialInner, attempts: u32, max_attempts: u32) {
    let error = SerialError::ReconnectExhausted {
        attempts: attempts.max(max_attempts),
        detail: "no further open attempts are scheduled".to_owned(),
    };
    set_error_inner(inner, &error);
    let mut runtime = lock(&inner.reconnect);
    runtime.attempt = attempts.max(max_attempts);
    runtime.backoff = false;
    runtime.next_retry_ms = None;
    runtime.exhausted = true;
    drop(runtime);
    let _ = inner.signal.send(());
}

fn mark_stopped(inner: &SerialInner) {
    let mut runtime = lock(&inner.reconnect);
    runtime.backoff = false;
    runtime.next_retry_ms = None;
    runtime.exhausted = true;
    drop(runtime);
    let _ = inner.signal.send(());
}

fn set_error_inner(inner: &SerialInner, error: &SerialError) {
    *lock(&inner.last_error) = Some(SerialStatusError::from(error));
}

fn apply_serial_lines(
    port: &mut dyn SerialPortOps,
    dtr: Option<bool>,
    rts: Option<bool>,
) -> Result<(), SerialError> {
    if let Some(level) = dtr {
        port.set_dtr(level)
            .map_err(|error| map_line_error("DTR", &error))?;
    }
    if let Some(level) = rts {
        port.set_rts(level)
            .map_err(|error| map_line_error("RTS", &error))?;
    }
    Ok(())
}

fn map_line_error(line: &str, error: &serialport::Error) -> SerialError {
    match error.kind {
        serialport::ErrorKind::NoDevice => SerialError::NotConnected,
        _ => SerialError::LineControlUnsupported {
            line: line.to_owned(),
        },
    }
}

fn open_real(config: &SerialConfig) -> Result<Box<dyn SerialPortOps>, SerialError> {
    let device = config.device.as_path();
    validate_device_path(device)?;
    match std::fs::metadata(device) {
        Ok(metadata) if metadata.is_dir() => {
            return Err(SerialError::InvalidDevice {
                reason: "device path is a directory".to_owned(),
            });
        }
        Ok(_) => {}
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return Err(SerialError::DeviceMissing {
                path: device.display().to_string(),
            });
        }
        Err(error) => {
            return Err(map_io_error(device, &error));
        }
    }
    let line = config.line;
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
    let port = serialport::new(device.to_string_lossy(), line.baud_rate)
        .data_bits(data_bits)
        .parity(line.parity.native())
        .stop_bits(stop_bits)
        .flow_control(config.flow_control.native())
        .timeout(config.read_timeout)
        .open()
        .map_err(|error| map_open_error_with_flow(device, config.flow_control, &error))?;

    // Verify the driver actually accepted the requested mode. A driver may
    // silently ignore a flag it cannot implement.
    if config.flow_control != SerialFlowControl::None {
        match port.flow_control() {
            Ok(actual) if actual == config.flow_control.native() => {}
            _ => {
                return Err(SerialError::FlowControlUnsupported {
                    path: device.display().to_string(),
                    flow_control: config.flow_control.as_str().to_owned(),
                })
            }
        }
    }
    let _ = port.clear(serialport::ClearBuffer::All);
    Ok(Box::new(RealSerialPort { port }))
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

fn map_open_error_with_flow(
    path: &Path,
    flow_control: SerialFlowControl,
    error: &serialport::Error,
) -> SerialError {
    if flow_control != SerialFlowControl::None
        && matches!(
            error.kind,
            serialport::ErrorKind::InvalidInput
                | serialport::ErrorKind::Io(io::ErrorKind::InvalidInput)
                | serialport::ErrorKind::Io(io::ErrorKind::Unsupported)
        )
    {
        return SerialError::FlowControlUnsupported {
            path: path.display().to_string(),
            flow_control: flow_control.as_str().to_owned(),
        };
    }
    map_open_error(path, error)
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
        .route(
            "/serial/control",
            post(control_handler)
                .layer(axum::extract::DefaultBodyLimit::max(MAX_CONTROL_BODY_BYTES)),
        )
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

async fn control_handler(
    State(state): State<SerialRouteState>,
    body: Result<Json<SerialControlRequest>, axum::extract::rejection::JsonRejection>,
) -> Response {
    let Some(service) = state.service else {
        return not_configured();
    };
    let Json(request) = match body {
        Ok(body) => body,
        Err(rejection) if rejection.status() == StatusCode::PAYLOAD_TOO_LARGE => {
            return SerialError::PayloadTooLarge {
                max: MAX_CONTROL_BODY_BYTES,
            }
            .into_response()
        }
        Err(_) => return SerialError::InvalidControl.into_response(),
    };
    let result = tokio::task::spawn_blocking(move || service.control(request)).await;
    match result {
        Ok(Ok(control)) => match serde_json::to_value(control) {
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
        // Most tests exercise a single open; reconnect-specific tests opt in.
        config.reconnect.max_attempts = 0;
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

    #[derive(Clone, Copy, Debug, Default)]
    enum FakeEnd {
        #[default]
        Block,
        Error,
    }

    #[derive(Clone, Debug, Default)]
    struct FakePortState {
        reads: VecDeque<Vec<u8>>,
        end: FakeEnd,
        dtr: Option<bool>,
        rts: Option<bool>,
        cts: Option<bool>,
        dsr: Option<bool>,
        dcd: Option<bool>,
        ri: Option<bool>,
        reject_dtr: bool,
        reject_rts: bool,
        written: Vec<u8>,
        clears: u32,
    }

    #[derive(Clone)]
    struct FakePort {
        state: Arc<Mutex<FakePortState>>,
    }

    impl SerialPortOps for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            let mut state = lock(&self.state);
            if let Some(chunk) = state.reads.pop_front() {
                let take = chunk.len().min(buf.len());
                buf[..take].copy_from_slice(&chunk[..take]);
                if take < chunk.len() {
                    state.reads.push_front(chunk[take..].to_vec());
                }
                return Ok(take);
            }
            let end = state.end;
            drop(state);
            match end {
                FakeEnd::Block => {
                    std::thread::sleep(Duration::from_millis(2));
                    Err(io::Error::new(io::ErrorKind::TimedOut, "fake idle"))
                }
                FakeEnd::Error => Err(io::Error::new(io::ErrorKind::BrokenPipe, "fake removal")),
            }
        }

        fn write_all(&mut self, buf: &[u8]) -> io::Result<()> {
            lock(&self.state).written.extend_from_slice(buf);
            Ok(())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }

        fn try_clone_box(&self) -> io::Result<Box<dyn SerialPortOps>> {
            Ok(Box::new(Self {
                state: self.state.clone(),
            }))
        }

        fn set_dtr(&mut self, level: bool) -> Result<(), serialport::Error> {
            let mut state = lock(&self.state);
            if state.reject_dtr {
                return Err(serialport::Error::new(
                    serialport::ErrorKind::InvalidInput,
                    "fake DTR unsupported",
                ));
            }
            state.dtr = Some(level);
            Ok(())
        }

        fn set_rts(&mut self, level: bool) -> Result<(), serialport::Error> {
            let mut state = lock(&self.state);
            if state.reject_rts {
                return Err(serialport::Error::new(
                    serialport::ErrorKind::InvalidInput,
                    "fake RTS unsupported",
                ));
            }
            state.rts = Some(level);
            Ok(())
        }

        fn read_pin(&mut self, pin: SerialPin) -> Result<bool, serialport::Error> {
            let state = lock(&self.state);
            let value = match pin {
                SerialPin::Cts => state.cts,
                SerialPin::Dsr => state.dsr,
                SerialPin::Dcd => state.dcd,
                SerialPin::Ri => state.ri,
            };
            value.ok_or_else(|| {
                serialport::Error::new(serialport::ErrorKind::InvalidInput, "fake pin unavailable")
            })
        }

        fn clear_buffers(&mut self) {
            lock(&self.state).clears += 1;
        }
    }

    #[derive(Clone, Debug)]
    struct FakeOpen {
        path: PathBuf,
        flow_control: SerialFlowControl,
    }

    #[derive(Clone)]
    struct FakeSpec {
        error: Option<SerialError>,
        state: FakePortState,
    }

    impl FakeSpec {
        fn ok(state: FakePortState) -> Self {
            Self { error: None, state }
        }

        fn error(error: SerialError) -> Self {
            Self {
                error: Some(error),
                state: FakePortState::default(),
            }
        }
    }

    #[derive(Clone, Default)]
    struct FakeFactory {
        inner: Arc<Mutex<FakeFactoryInner>>,
    }

    #[derive(Default)]
    struct FakeFactoryInner {
        specs: VecDeque<FakeSpec>,
        default: Option<FakeSpec>,
        opens: Vec<FakeOpen>,
        ports: Vec<Arc<Mutex<FakePortState>>>,
    }

    impl FakeFactory {
        fn new(specs: Vec<FakeSpec>, default: Option<FakeSpec>) -> Self {
            Self {
                inner: Arc::new(Mutex::new(FakeFactoryInner {
                    specs: specs.into(),
                    default,
                    ..FakeFactoryInner::default()
                })),
            }
        }

        fn opener(&self) -> Arc<OpenFn> {
            let factory = self.clone();
            Arc::new(move |config: &SerialConfig| {
                let mut inner = lock(&factory.inner);
                let spec = inner
                    .specs
                    .pop_front()
                    .or_else(|| inner.default.clone())
                    .unwrap_or_else(|| FakeSpec::ok(FakePortState::default()));
                inner.opens.push(FakeOpen {
                    path: config.device.clone(),
                    flow_control: config.flow_control,
                });
                if let Some(error) = spec.error {
                    return Err(error);
                }
                let state = Arc::new(Mutex::new(spec.state));
                inner.ports.push(state.clone());
                Ok(Box::new(FakePort { state }) as Box<dyn SerialPortOps>)
            })
        }

        fn opens(&self) -> Vec<FakeOpen> {
            lock(&self.inner).opens.clone()
        }

        fn open_count(&self) -> usize {
            lock(&self.inner).opens.len()
        }

        fn port(&self, index: usize) -> Option<FakePortState> {
            let inner = lock(&self.inner);
            inner.ports.get(index).map(|state| lock(state).clone())
        }
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
        assert_eq!(
            "RTSCTS".parse::<SerialFlowControl>().unwrap(),
            SerialFlowControl::RtsCts
        );
        assert_eq!(
            "software".parse::<SerialFlowControl>().unwrap(),
            SerialFlowControl::XonXoff
        );
        assert!(
            "bad".parse::<SerialFlowControl>().is_err(),
            "unknown flow control is rejected"
        );
    }

    #[test]
    fn reconnect_policy_is_validated_and_backoff_is_capped() {
        let policy = SerialReconnectConfig::default();
        assert_eq!(policy.max_attempts, 5);
        assert_eq!(policy.min_delay, Duration::from_secs(1));
        assert_eq!(policy.max_delay, Duration::from_secs(30));
        let delays: Vec<u64> = (1..=6)
            .map(|attempt| backoff_delay(&policy, attempt).as_millis() as u64)
            .collect();
        assert_eq!(delays, vec![1_000, 2_000, 4_000, 8_000, 16_000, 30_000]);

        let mut config = test_config("/dev/fake-serial");
        config.reconnect.max_attempts = MAX_RECONNECT_ATTEMPTS + 1;
        assert_eq!(
            config.validate().unwrap_err().code(),
            "serial.invalid-config"
        );

        let mut config = test_config("/dev/fake-serial");
        config.reconnect.min_delay = Duration::ZERO;
        assert_eq!(
            config.validate().unwrap_err().code(),
            "serial.invalid-config"
        );

        let mut config = test_config("/dev/fake-serial");
        config.reconnect.min_delay = Duration::from_secs(2);
        config.reconnect.max_delay = Duration::from_secs(1);
        assert_eq!(
            config.validate().unwrap_err().code(),
            "serial.invalid-config"
        );
    }

    #[test]
    fn flow_control_initial_lines_and_status_are_reported() {
        let factory = FakeFactory::new(
            vec![FakeSpec::ok(FakePortState {
                cts: Some(true),
                dsr: Some(false),
                dcd: Some(true),
                ri: Some(false),
                ..FakePortState::default()
            })],
            None,
        );
        let mut config = test_config("/dev/fake-serial");
        config.flow_control = SerialFlowControl::RtsCts;
        config.initial_dtr = Some(true);
        config.initial_rts = Some(false);
        let service = SerialService::with_opener(config, factory.opener());
        service.start().expect("fake open");
        assert!(wait_for(Duration::from_secs(1), || service
            .status()
            .connected));

        let status = service.status();
        assert_eq!(status.flow_control, SerialFlowControl::RtsCts);
        assert_eq!(status.lines.dtr, Some(true));
        assert_eq!(status.lines.rts, Some(false));
        assert_eq!(status.lines.cts, Some(true));
        assert_eq!(status.lines.dsr, Some(false));
        assert_eq!(status.lines.dcd, Some(true));
        assert_eq!(status.lines.ri, Some(false));
        assert!(!status.control_enabled);
        assert_eq!(status.reconnect.max_attempts, 0);

        let opens = factory.opens();
        assert_eq!(opens.len(), 1);
        assert_eq!(opens[0].path, PathBuf::from("/dev/fake-serial"));
        assert_eq!(opens[0].flow_control, SerialFlowControl::RtsCts);
        let port = factory.port(0).expect("fake port state");
        assert_eq!(port.dtr, Some(true));
        assert_eq!(port.rts, Some(false));
        assert_eq!(port.clears, 1, "kernel buffers are cleared on open");
        service.stop();
    }

    #[test]
    fn unsupported_flow_control_and_initial_line_are_typed() {
        let factory = FakeFactory::new(
            vec![FakeSpec::error(SerialError::FlowControlUnsupported {
                path: "/dev/fake-serial".to_owned(),
                flow_control: "rtscts".to_owned(),
            })],
            None,
        );
        let mut config = test_config("/dev/fake-serial");
        config.flow_control = SerialFlowControl::RtsCts;
        let service = SerialService::with_opener(config, factory.opener());
        assert_eq!(
            service.start().unwrap_err().code(),
            "serial.flow-control-unsupported"
        );
        assert_eq!(
            service.status().last_error.unwrap().code,
            "serial.flow-control-unsupported"
        );
        service.stop();

        let factory = FakeFactory::new(
            vec![FakeSpec::ok(FakePortState {
                reject_dtr: true,
                ..FakePortState::default()
            })],
            None,
        );
        let mut config = test_config("/dev/fake-serial");
        config.initial_dtr = Some(true);
        let service = SerialService::with_opener(config, factory.opener());
        assert_eq!(
            service.start().unwrap_err().code(),
            "serial.line-control-unsupported"
        );
        service.stop();
    }

    #[tokio::test]
    async fn control_endpoint_is_opt_in_and_bounded() {
        let factory = FakeFactory::new(vec![FakeSpec::ok(FakePortState::default())], None);
        let config = test_config("/dev/fake-serial");
        let disabled = SerialService::with_opener(config, factory.opener());
        disabled.start().unwrap();
        let app = router(Some(disabled.clone()));
        let (status, body) = post(&app, "/serial/control", json!({"dtr": true})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body_json(&body)["code"], "serial.control-disabled");
        disabled.stop();

        let mut config = test_config("/dev/fake-serial");
        config.control_enabled = true;
        let service = SerialService::with_opener(config, factory.opener());
        service.start().unwrap();
        let app = router(Some(service.clone()));

        let (status, body) = post(&app, "/serial/control", json!({"dtr": true})).await;
        assert_eq!(status, StatusCode::OK, "{}", String::from_utf8_lossy(&body));
        let control = body_json(&body);
        assert_eq!(control["dtr"], true);
        assert_eq!(control["lines"]["dtr"], true);
        assert_eq!(factory.port(1).unwrap().dtr, Some(true));

        let (status, _) = post(&app, "/serial/control", json!({"rts": false})).await;
        assert_eq!(status, StatusCode::OK);

        let (status, body) = post(&app, "/serial/control", json!({})).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body_json(&body)["code"], "serial.invalid-control");

        let (status, body) = post(
            &app,
            "/serial/control",
            json!({"dtr": true, "other": "nope"}),
        )
        .await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body_json(&body)["code"], "serial.invalid-control");

        let (status, _) = post(
            &app,
            "/serial/control",
            json!({"dtr": true, "padding": "x".repeat(2000)}),
        )
        .await;
        assert_eq!(status, StatusCode::PAYLOAD_TOO_LARGE);

        let (status, body) = post(&app, "/serial/control", json!({"dtr": false})).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body_json(&body)["lines"]["dtr"], false);
        service.stop();

        // Enabled but the fake driver rejects DTR: typed 501, no fabricated
        // success and no state change.
        let factory = FakeFactory::new(
            vec![FakeSpec::ok(FakePortState {
                reject_dtr: true,
                ..FakePortState::default()
            })],
            None,
        );
        let mut config = test_config("/dev/fake-serial");
        config.control_enabled = true;
        let service = SerialService::with_opener(config, factory.opener());
        service.start().unwrap();
        let app = router(Some(service.clone()));
        let (status, body) = post(&app, "/serial/control", json!({"dtr": true})).await;
        assert_eq!(status, StatusCode::NOT_IMPLEMENTED);
        assert_eq!(body_json(&body)["code"], "serial.line-control-unsupported");
        assert_eq!(service.status().lines.dtr, None);
        service.stop();

        // Enabled but not connected: typed 503 rather than a fake pin change.
        let mut config = test_config("/dev/fake-serial");
        config.control_enabled = true;
        let service = SerialService::with_opener(config, factory.opener());
        let app = router(Some(service.clone()));
        let (status, body) = post(&app, "/serial/control", json!({"dtr": true})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(&body)["code"], "serial.not-connected");
        service.stop();
    }

    #[test]
    fn reconnect_backoff_reopens_same_path_and_resets_buffer() {
        let factory = FakeFactory::new(
            vec![
                FakeSpec::ok(FakePortState {
                    reads: VecDeque::from([b"boot-ok\n".to_vec()]),
                    end: FakeEnd::Error,
                    ..FakePortState::default()
                }),
                FakeSpec::ok(FakePortState {
                    end: FakeEnd::Block,
                    ..FakePortState::default()
                }),
            ],
            None,
        );
        let mut config = test_config("/dev/fake-serial");
        config.reconnect = SerialReconnectConfig {
            max_attempts: 5,
            min_delay: Duration::from_millis(10),
            max_delay: Duration::from_millis(40),
        };
        let service = SerialService::with_opener(config, factory.opener());
        service.start().expect("initial fake open");
        assert!(
            wait_for(Duration::from_secs(2), || {
                let status = service.status();
                status.connected && status.reconnect.reconnects >= 1
            }),
            "worker should reopen the same path after the read error"
        );

        let status = service.status();
        assert_eq!(status.state, "connected");
        assert_eq!(status.reconnect.attempt, 0);
        assert_eq!(status.reconnect.max_attempts, 5);
        assert!(!status.reconnect.backoff);
        assert!(!status.reconnect.exhausted);
        assert_eq!(status.buffer.bytes, 0, "reconnect clears retained bytes");
        assert!(status.last_error.is_none());

        let opens = factory.opens();
        assert_eq!(opens.len(), 2);
        assert!(opens
            .iter()
            .all(|open| open.path == PathBuf::from("/dev/fake-serial")));
        assert_eq!(status.bytes_in, 8, "lifetime counter survives reconnect");
        service.stop();
    }

    #[test]
    fn reconnect_cap_stops_and_does_not_storm() {
        let factory = FakeFactory::new(
            Vec::new(),
            Some(FakeSpec::error(SerialError::DeviceMissing {
                path: "/dev/fake-serial".to_owned(),
            })),
        );
        let mut config = test_config("/dev/fake-serial");
        config.reconnect = SerialReconnectConfig {
            max_attempts: 2,
            min_delay: Duration::from_millis(5),
            max_delay: Duration::from_millis(10),
        };
        let service = SerialService::with_opener(config, factory.opener());
        assert_eq!(service.start().unwrap_err().code(), "serial.device-missing");
        assert!(
            wait_for(Duration::from_secs(2), || service
                .status()
                .reconnect
                .exhausted),
            "retry budget should be exhausted"
        );
        let status = service.status();
        assert_eq!(status.state, "failed");
        assert_eq!(status.reconnect.max_attempts, 2);
        assert_eq!(status.reconnect.attempt, 2);
        assert_eq!(
            status.last_error.unwrap().code,
            "serial.reconnect-exhausted"
        );
        assert_eq!(
            factory.open_count(),
            3,
            "one initial open plus exactly two retries"
        );
        std::thread::sleep(Duration::from_millis(80));
        assert_eq!(
            factory.open_count(),
            3,
            "no reconnect storm after exhaustion"
        );
        service.stop();
    }

    #[test]
    fn stop_during_backoff_is_prompt_and_leaves_no_worker() {
        let factory = FakeFactory::new(
            Vec::new(),
            Some(FakeSpec::error(SerialError::DeviceMissing {
                path: "/dev/fake-serial".to_owned(),
            })),
        );
        let mut config = test_config("/dev/fake-serial");
        config.reconnect = SerialReconnectConfig {
            max_attempts: 5,
            min_delay: Duration::from_secs(60),
            max_delay: Duration::from_secs(60),
        };
        let service = SerialService::with_opener(config, factory.opener());
        assert!(service.start().is_err());
        assert!(
            wait_for(Duration::from_secs(1), || service
                .status()
                .reconnect
                .backoff),
            "worker should be sleeping in backoff"
        );
        let started = Instant::now();
        service.stop();
        assert!(
            started.elapsed() < Duration::from_secs(1),
            "stop must interrupt the backoff sleep"
        );
        let status = service.status();
        assert_eq!(status.state, "stopped");
        assert!(!status.reconnect.backoff);
        assert_eq!(status.reconnect.next_retry_ms, None);
        assert_eq!(factory.open_count(), 1);
        std::thread::sleep(Duration::from_millis(50));
        assert_eq!(
            factory.open_count(),
            1,
            "stopped worker must not reopen the device"
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
            ("POST", "/serial/control"),
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

    struct SocatAt {
        child: Child,
        left: PathBuf,
        right: PathBuf,
    }

    impl SocatAt {
        fn spawn(left: &Path, right: &Path) -> Option<Self> {
            let _ = std::fs::remove_file(left);
            let _ = std::fs::remove_file(right);
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
                left: left.to_path_buf(),
                right: right.to_path_buf(),
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

    impl Drop for SocatAt {
        fn drop(&mut self) {
            let _ = self.child.kill();
            let _ = self.child.wait();
            let _ = std::fs::remove_file(&self.left);
            let _ = std::fs::remove_file(&self.right);
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
    async fn live_socat_pty_removal_and_readd_reconnects() {
        let dir = match tempfile::tempdir() {
            Ok(dir) => dir,
            Err(_) => return,
        };
        let left = dir.path().join("reconnect-a");
        let right = dir.path().join("reconnect-b");
        let Some(first) = SocatAt::spawn(&left, &right) else {
            eprintln!("skipping live reconnect test: socat is unavailable");
            return;
        };
        let mut config = test_config(&left);
        config.reconnect = SerialReconnectConfig {
            max_attempts: 20,
            min_delay: Duration::from_millis(50),
            max_delay: Duration::from_millis(200),
        };
        let service = SerialService::new(config);
        service.start().expect("open first socat PTY");
        assert!(
            wait_for(Duration::from_secs(2), || service.status().connected),
            "service should connect to the first socat PTY"
        );

        let mut peer = first.peer();
        peer.write_all(b"before\n").expect("peer writes");
        peer.flush().unwrap();
        assert!(
            wait_for(Duration::from_secs(2), || service.status().bytes_in >= 7),
            "service should read from the first PTY"
        );
        drop(peer);
        drop(first);

        assert!(
            wait_for(Duration::from_secs(2), || !service.status().connected),
            "removing the first PTY should disconnect the reader"
        );
        let Some(second) = SocatAt::spawn(&left, &right) else {
            eprintln!("skipping live re-add assertion: socat could not replace the PTY");
            service.stop();
            return;
        };
        assert!(
            wait_for(Duration::from_secs(5), || {
                let status = service.status();
                status.connected && status.reconnect.reconnects >= 1
            }),
            "service should reconnect to the re-added path"
        );
        let status = service.status();
        assert_eq!(status.device, left.display().to_string());
        assert_eq!(status.reconnect.max_attempts, 20);
        assert_eq!(status.buffer.bytes, 0, "reconnect clears retained bytes");

        let mut peer = second.peer();
        peer.write_all(b"after\n")
            .expect("peer writes after re-add");
        peer.flush().unwrap();
        assert!(
            wait_for(Duration::from_secs(2), || service.status().bytes_in >= 13),
            "service should read from the re-added PTY"
        );
        let read = service.read(None, None).expect("buffered read");
        let bytes = BASE64.decode(&read.data).unwrap();
        assert!(bytes.ends_with(b"after\n"), "{bytes:?}");
        service.stop();
        drop(second);
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
