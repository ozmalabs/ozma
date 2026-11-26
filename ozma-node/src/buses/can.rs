// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, allowlisted SocketCAN surface.
//!
//! The allowlist is the complete set of exact interface names. Read-only
//! status/capture is available for every listed interface; transmission is
//! opted in per interface (`--can-allow IFACE:send` or
//! `--can-send-allow IFACE`). There is no default interface enumeration, no
//! broadcast, no bus-off/restart operation, and no send without the explicit
//! per-interface opt-in.
//!
//! Capture starts lazily on the first frames request for that interface and
//! keeps one bounded SocketCAN receive socket per allowlisted interface. The
//! buffer is bounded by `--can-max-frames` and the response by `?limit=N`.
//! Status is read from `ip -details -statistics -json` with a deadline; the
//! node never runs `ip link set` and never restarts a CAN controller.

use std::collections::{BTreeMap, VecDeque};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use axum::extract::rejection::JsonRejection;
use axum::extract::{DefaultBodyLimit, Path as AxumPath, Query, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::{
    invalid_request, io_failed, json_response, unavailable, BusError, BusLimits, BusState,
    MAX_CAN_FRAMES, MAX_ERROR_DETAIL, MAX_REQUEST_BODY_BYTES,
};

/// Largest classic CAN payload (CAN 2.0).
pub const MAX_CLASSIC_DATA_BYTES: usize = 8;
/// Largest CAN FD payload.
pub const MAX_FD_DATA_BYTES: usize = 64;
/// Largest standard 11-bit CAN identifier.
pub const MAX_STANDARD_ID: u32 = 0x7ff;
/// Largest extended 29-bit CAN identifier.
pub const MAX_EXTENDED_ID: u32 = 0x1fff_ffff;
/// `ip` status deadline.
pub const STATUS_TIMEOUT_MS: u64 = 2_000;
/// Bounded `ip` stdout/stderr reads.
const MAX_IP_OUTPUT_BYTES: usize = 128 * 1024;
/// SocketCAN capture read timeout; also bounds Drop/join latency.
const CAPTURE_READ_TIMEOUT_MS: u64 = 100;

/// Exact CAN interface allowlist and per-interface send opt-in.
#[derive(Clone, Debug, Default)]
pub struct CanConfig {
    allow: BTreeMap<String, CanIfaceConfig>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct CanIfaceConfig {
    send: bool,
}

impl CanConfig {
    /// Parse the interface allowlist plus an optional separate send allowlist.
    ///
    /// Entries may be comma/space separated. In `entries`, an exact
    /// `IFACE:send` suffix enables transmission for that interface. Every name
    /// in `send_entries` must already appear in `entries`, so a send allowlist
    /// can narrow but never widen the interface allowlist.
    pub fn from_allow_entries<I, S, J, T>(entries: I, send_entries: J) -> Result<Self, BusError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
        J: IntoIterator<Item = T>,
        T: AsRef<str>,
    {
        let mut config = Self::default();
        for entry in entries {
            for token in split_entries(entry.as_ref()) {
                let (iface, send) = match token.split_once(':') {
                    None => (token, false),
                    Some((iface, "send")) => (iface, true),
                    Some(_) => {
                        return Err(invalid_request(format!(
                            "invalid --can-allow entry {token:?}; only the :send suffix is accepted"
                        )))
                    }
                };
                let iface = validate_iface(iface)?;
                let slot = config.allow.entry(iface).or_default();
                slot.send |= send;
            }
        }
        for entry in send_entries {
            for token in split_entries(entry.as_ref()) {
                if token.split_once(':').is_some() {
                    return Err(invalid_request(format!(
                        "invalid --can-send-allow entry {token:?}; expected an exact interface name"
                    )));
                }
                let iface = validate_iface(token)?;
                let Some(slot) = config.allow.get_mut(&iface) else {
                    return Err(invalid_request(format!(
                        "--can-send-allow {iface} is not present in --can-allow; add it to the interface allowlist first"
                    )));
                };
                slot.send = true;
            }
        }
        config.validate()?;
        Ok(config)
    }

    /// Convenience constructor for a read-only allowlist.
    pub fn from_entries<I, S>(entries: I) -> Result<Self, BusError>
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        Self::from_allow_entries(entries, std::iter::empty::<&str>())
    }

    /// Validate the absolute interface ceiling and names.
    pub fn validate(&self) -> Result<(), BusError> {
        if self.allow.len() > super::MAX_DEVICES {
            return Err(invalid_request(format!(
                "CAN allowlist contains {} interfaces, above the {} absolute bound",
                self.allow.len(),
                super::MAX_DEVICES
            )));
        }
        for iface in self.allow.keys() {
            if validate_iface(iface)? != *iface {
                return Err(invalid_request(format!(
                    "CAN interface {iface:?} is not normalized"
                )));
            }
        }
        Ok(())
    }

    pub fn is_configured(&self) -> bool {
        !self.allow.is_empty()
    }

    /// Exact interface allowlist check before any socket or `ip` call.
    pub fn allows(&self, iface: &str) -> bool {
        self.allow.contains_key(iface)
    }

    /// True only when this exact interface was explicitly opted into send.
    pub fn send_allowed(&self, iface: &str) -> bool {
        self.allow.get(iface).is_some_and(|config| config.send)
    }

    /// Allowlisted interfaces in ascending order.
    pub fn ifaces(&self) -> impl Iterator<Item = &String> {
        self.allow.keys()
    }

    /// Interfaces explicitly opted into send.
    pub fn send_ifaces(&self) -> impl Iterator<Item = &String> {
        self.allow
            .iter()
            .filter(|(_, config)| config.send)
            .map(|(iface, _)| iface)
    }

    pub fn len(&self) -> usize {
        self.allow.len()
    }

    pub fn is_empty(&self) -> bool {
        self.allow.is_empty()
    }
}

fn split_entries(input: &str) -> impl Iterator<Item = &str> {
    input
        .split(|character: char| character == ',' || character.is_whitespace())
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

/// Validate an exact Linux interface name without inventing one.
///
/// `IFNAMSIZ` is 16 bytes including the NUL, so names are 1–15 bytes. `/`,
/// whitespace, control characters, and `:` are rejected so the name can be a
/// route segment and a sysfs component without escaping either.
pub fn validate_iface(raw: &str) -> Result<String, BusError> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err(invalid_request("CAN interface name is empty"));
    }
    if raw.len() > 15 {
        return Err(invalid_request(format!(
            "CAN interface {raw:?} is longer than the 15-byte IFNAMSIZ limit"
        )));
    }
    if raw.starts_with('.') || raw.starts_with('-') {
        return Err(invalid_request(format!(
            "CAN interface {raw:?} must not start with '.' or '-'"
        )));
    }
    if !raw
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid_request(format!(
            "CAN interface {raw:?} contains characters outside ASCII letters, digits, '_', '-', and '.'"
        )));
    }
    Ok(raw.to_owned())
}

/// True when `/sys/class/net/<iface>` exists. No socket is opened.
pub fn interface_present(iface: &str) -> bool {
    std::path::Path::new("/sys/class/net").join(iface).exists()
}

/// CAN error counters parsed from `ip -details -json` when the driver exposes
/// them; absent values stay `null` rather than being invented as zero.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CanErrorCounters {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_warning: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_warning: Option<u64>,
}

/// Link statistics from `ip -statistics -json`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CanLinkStats {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_packets: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_packets: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_dropped: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_over_errors: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rx_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tx_bytes: Option<u64>,
}

/// Backend link status, before `ok`/`configured` wrapping.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct CanLinkStatus {
    pub available: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operstate: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u64>,
    /// `linkinfo.info_kind`, e.g. `can` or `vcan`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bitrate: Option<u64>,
    /// Controller state reported by the driver, when present.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub state: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub restart_ms: Option<u64>,
    pub error_counters: CanErrorCounters,
    pub stats: CanLinkStats,
}

/// One captured frame.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanFrameInfo {
    pub id: u32,
    pub extended: bool,
    pub rtr: bool,
    pub fd: bool,
    pub error: bool,
    pub dlc: usize,
    pub bytes: usize,
    /// Payload as lower-case hex (at most 64 bytes).
    pub data: String,
    /// Userspace arrival time, milliseconds since the Unix epoch.
    pub timestamp_ms: u64,
}

/// Bounded snapshot returned by a capture backend.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CanFrameSnapshot {
    pub frames: Vec<CanFrameInfo>,
    /// Frames in the buffer before the `limit` slice.
    pub matched: usize,
    /// Frames evicted by the buffer bound.
    pub dropped: u64,
    /// Total frames captured since the socket opened.
    pub total: u64,
    pub capture_started: bool,
    pub last_error: Option<String>,
}

/// Normalized outbound frame handed to a backend.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CanSendOp {
    pub id: u32,
    pub extended: bool,
    pub rtr: bool,
    pub dlc: u8,
    pub fd: bool,
    pub brs: bool,
    pub data: Vec<u8>,
}

/// `POST /buses/can/{iface}/send` body.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CanSendRequest {
    /// 11-bit ID (0..=0x7ff) or 29-bit ID (0..=0x1fff_ffff).
    pub id: u32,
    #[serde(default)]
    pub extended: bool,
    /// Payload as bounded hex. At most 8 bytes classic, 64 with `fd`.
    #[serde(default)]
    pub data: Option<String>,
    /// Explicit remote-transmission-request frame.
    #[serde(default)]
    pub rtr: bool,
    /// Requested data length for an RTR frame (0–8). Rejected otherwise.
    #[serde(default)]
    pub dlc: Option<u8>,
    /// CAN FD frame (64-byte payload).
    #[serde(default)]
    pub fd: bool,
    /// CAN FD bit-rate switch; only valid with `fd`.
    #[serde(default)]
    pub brs: bool,
}

impl CanSendRequest {
    fn into_op(self) -> Result<CanSendOp, BusError> {
        if self.extended {
            if self.id > MAX_EXTENDED_ID {
                return Err(invalid_request(format!(
                    "extended CAN id 0x{:x} exceeds 0x{MAX_EXTENDED_ID:x}",
                    self.id
                )));
            }
        } else if self.id > MAX_STANDARD_ID {
            return Err(invalid_request(format!(
                "standard CAN id 0x{:x} exceeds 0x{MAX_STANDARD_ID:x}; set extended:true for a 29-bit id",
                self.id
            )));
        }
        if self.fd && self.rtr {
            return Err(invalid_request(
                "CAN FD has no remote-transmission-request form; rtr and fd are mutually exclusive",
            ));
        }
        if self.brs && !self.fd {
            return Err(invalid_request("CAN brs requires fd:true"));
        }
        if self.rtr {
            if self
                .dlc
                .is_some_and(|dlc| dlc > MAX_CLASSIC_DATA_BYTES as u8)
            {
                return Err(invalid_request(
                    "RTR dlc must be between 0 and 8 for a classic CAN frame",
                ));
            }
            let data = self.data.unwrap_or_default();
            if !data.trim().is_empty() {
                return Err(invalid_request(
                    "RTR frames carry no data payload; omit `data`",
                ));
            }
            return Ok(CanSendOp {
                id: self.id,
                extended: self.extended,
                rtr: true,
                dlc: self.dlc.unwrap_or(0),
                fd: false,
                brs: false,
                data: Vec::new(),
            });
        }
        if self.dlc.is_some() {
            return Err(invalid_request("`dlc` is only valid with rtr:true"));
        }
        let max = if self.fd {
            MAX_FD_DATA_BYTES
        } else {
            MAX_CLASSIC_DATA_BYTES
        };
        let data = parse_hex_data(self.data.as_deref().unwrap_or(""), max)?;
        Ok(CanSendOp {
            id: self.id,
            extended: self.extended,
            rtr: false,
            dlc: 0,
            fd: self.fd,
            brs: self.brs,
            data,
        })
    }
}

fn parse_hex_data(input: &str, max: usize) -> Result<Vec<u8>, BusError> {
    let bytes = super::parse_hex_bytes(input, max)?;
    if bytes.len() > max {
        return Err(invalid_request(format!(
            "CAN payload exceeds the {max}-byte bound"
        )));
    }
    Ok(bytes)
}

/// CAN backend abstraction. Production uses [`RealCanBackend`]; tests inject a
/// fake so allowlist/send-opt-in enforcement and bounds are observable without
/// touching a real interface.
pub trait CanBackend: Send + Sync {
    fn status(&self, iface: &str) -> Result<CanLinkStatus, BusError>;
    fn send(&self, iface: &str, op: &CanSendOp) -> Result<(), BusError>;
    fn frames(
        &self,
        iface: &str,
        limit: usize,
        capacity: usize,
    ) -> Result<CanFrameSnapshot, BusError>;
}

/// Real Linux SocketCAN backend.
#[derive(Default)]
pub struct RealCanBackend {
    captures: Mutex<BTreeMap<String, Arc<RealCapture>>>,
}

impl CanBackend for RealCanBackend {
    fn status(&self, iface: &str) -> Result<CanLinkStatus, BusError> {
        #[cfg(target_os = "linux")]
        {
            status_real(iface)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = iface;
            Err(unsupported())
        }
    }

    fn send(&self, iface: &str, op: &CanSendOp) -> Result<(), BusError> {
        #[cfg(target_os = "linux")]
        {
            send_real(iface, op)
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (iface, op);
            Err(unsupported())
        }
    }

    fn frames(
        &self,
        iface: &str,
        limit: usize,
        capacity: usize,
    ) -> Result<CanFrameSnapshot, BusError> {
        #[cfg(target_os = "linux")]
        {
            let capture = {
                let mut captures = self
                    .captures
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner());
                if let Some(capture) = captures.get(iface) {
                    capture.clone()
                } else {
                    let capture = Arc::new(RealCapture::start(iface, capacity)?);
                    captures.insert(iface.to_owned(), capture.clone());
                    capture
                }
            };
            Ok(capture.snapshot(limit))
        }
        #[cfg(not(target_os = "linux"))]
        {
            let _ = (iface, limit, capacity);
            Err(unsupported())
        }
    }
}

#[allow(dead_code)]
fn unsupported() -> BusError {
    BusError::Unsupported {
        detail: "SocketCAN is only implemented on Linux".to_owned(),
    }
}

/// One interface's bounded capture socket and worker.
struct RealCapture {
    shared: Arc<CaptureShared>,
    stop: Arc<AtomicBool>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

impl RealCapture {
    #[cfg(target_os = "linux")]
    fn start(iface: &str, capacity: usize) -> Result<Self, BusError> {
        let socket = match CaptureSocket::open(iface) {
            Ok(socket) => socket,
            Err(error) => {
                return Err(map_can_io(
                    &format!("opening a CAN capture socket on {iface}"),
                    error,
                ))
            }
        };
        socket
            .set_read_timeout(Some(Duration::from_millis(CAPTURE_READ_TIMEOUT_MS)))
            .map_err(|error| {
                map_can_io(&format!("setting the capture timeout on {iface}"), error)
            })?;

        let shared = Arc::new(CaptureShared::new(capacity));
        let stop = Arc::new(AtomicBool::new(false));
        let thread_shared = shared.clone();
        let thread_stop = stop.clone();
        let handle = std::thread::Builder::new()
            .name(format!("ozma-can-{iface}"))
            .spawn(move || capture_loop(socket, thread_shared, thread_stop))
            .map_err(|error| {
                io_failed(format!("could not start CAN capture for {iface}: {error}"))
            })?;
        Ok(Self {
            shared,
            stop,
            handle: Mutex::new(Some(handle)),
        })
    }

    #[cfg(not(target_os = "linux"))]
    fn start(iface: &str, capacity: usize) -> Result<Self, BusError> {
        let _ = (iface, capacity);
        Err(unsupported())
    }

    fn snapshot(&self, limit: usize) -> CanFrameSnapshot {
        self.shared.snapshot(limit)
    }
}

impl Drop for RealCapture {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self
            .handle
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .take()
        {
            let _ = handle.join();
        }
    }
}

struct CaptureShared {
    buffer: Mutex<CaptureBuffer>,
    total: AtomicU64,
}

struct CaptureBuffer {
    frames: VecDeque<CanFrameInfo>,
    capacity: usize,
    dropped: u64,
    last_error: Option<String>,
}

impl CaptureShared {
    fn new(capacity: usize) -> Self {
        Self {
            buffer: Mutex::new(CaptureBuffer {
                frames: VecDeque::new(),
                capacity: capacity.max(1),
                dropped: 0,
                last_error: None,
            }),
            total: AtomicU64::new(0),
        }
    }

    fn push(&self, frame: CanFrameInfo) {
        let mut buffer = self
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if buffer.frames.len() >= buffer.capacity {
            buffer.frames.pop_front();
            buffer.dropped = buffer.dropped.saturating_add(1);
        }
        buffer.frames.push_back(frame);
        self.total.fetch_add(1, Ordering::Relaxed);
    }

    fn fail(&self, detail: impl Into<String>) {
        let mut buffer = self
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        buffer.last_error = Some(bounded_detail(&detail.into()));
    }

    fn snapshot(&self, limit: usize) -> CanFrameSnapshot {
        let buffer = self
            .buffer
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let matched = buffer.frames.len();
        let start = matched.saturating_sub(limit);
        let frames = buffer.frames.iter().skip(start).cloned().collect();
        CanFrameSnapshot {
            frames,
            matched,
            dropped: buffer.dropped,
            total: self.total.load(Ordering::Relaxed),
            capture_started: true,
            last_error: buffer.last_error.clone(),
        }
    }
}

#[cfg(target_os = "linux")]
enum CaptureSocket {
    Fd(socketcan::CanFdSocket),
    Classic(socketcan::CanSocket),
}

#[cfg(target_os = "linux")]
impl CaptureSocket {
    fn open(iface: &str) -> std::io::Result<Self> {
        use socketcan::Socket;
        // Prefer an FD socket so a capture sees both classic and FD frames.
        // Fall back to a classic socket when the kernel/driver lacks FD mode.
        match socketcan::CanFdSocket::open(iface) {
            Ok(socket) => Ok(Self::Fd(socket)),
            Err(_) => socketcan::CanSocket::open(iface).map(Self::Classic),
        }
    }

    fn set_read_timeout(&self, timeout: Option<Duration>) -> std::io::Result<()> {
        use socketcan::Socket;
        match self {
            Self::Fd(socket) => socket.set_read_timeout(timeout),
            Self::Classic(socket) => socket.set_read_timeout(timeout),
        }
    }
}

#[cfg(target_os = "linux")]
fn capture_loop(socket: CaptureSocket, shared: Arc<CaptureShared>, stop: Arc<AtomicBool>) {
    use socketcan::Socket;
    loop {
        if stop.load(Ordering::Relaxed) {
            break;
        }
        let result = match &socket {
            CaptureSocket::Fd(socket) => socket.read_frame().map(|frame| any_frame_info(&frame)),
            CaptureSocket::Classic(socket) => {
                socket.read_frame().map(|frame| classic_frame_info(&frame))
            }
        };
        match result {
            Ok(frame) => shared.push(frame),
            Err(error)
                if matches!(
                    error.kind(),
                    std::io::ErrorKind::WouldBlock
                        | std::io::ErrorKind::TimedOut
                        | std::io::ErrorKind::Interrupted
                ) => {}
            Err(error) => {
                shared.fail(format!("CAN capture on this interface stopped: {error}"));
                break;
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn any_frame_info(frame: &socketcan::CanAnyFrame) -> CanFrameInfo {
    use socketcan::EmbeddedFrame;
    let error = matches!(frame, socketcan::CanAnyFrame::Error(_));
    let rtr = frame.is_remote_frame();
    let fd = matches!(frame, socketcan::CanAnyFrame::Fd(_));
    let id = embedded_id(frame.id());
    let data = frame.data();
    CanFrameInfo {
        id,
        extended: frame.is_extended(),
        rtr,
        fd,
        error,
        dlc: frame.dlc(),
        bytes: data.len(),
        data: super::encode_hex(data),
        timestamp_ms: now_millis(),
    }
}

#[cfg(target_os = "linux")]
fn classic_frame_info(frame: &socketcan::CanFrame) -> CanFrameInfo {
    use socketcan::EmbeddedFrame;
    let error = matches!(frame, socketcan::CanFrame::Error(_));
    let rtr = frame.is_remote_frame();
    let id = embedded_id(frame.id());
    let data = frame.data();
    CanFrameInfo {
        id,
        extended: frame.is_extended(),
        rtr,
        fd: false,
        error,
        dlc: frame.dlc(),
        bytes: data.len(),
        data: super::encode_hex(data),
        timestamp_ms: now_millis(),
    }
}

#[cfg(target_os = "linux")]
fn embedded_id(id: socketcan::Id) -> u32 {
    match id {
        socketcan::Id::Standard(id) => u32::from(id.as_raw()),
        socketcan::Id::Extended(id) => id.as_raw(),
    }
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis().min(u128::from(u64::MAX)) as u64)
        .unwrap_or(0)
}

#[cfg(target_os = "linux")]
fn send_real(iface: &str, op: &CanSendOp) -> Result<(), BusError> {
    use socketcan::{
        CanDataFrame, CanFdFrame, CanFdSocket, CanRemoteFrame, CanSocket, EmbeddedFrame,
        ExtendedId, Socket, StandardId,
    };

    let id = if op.extended {
        socketcan::Id::Extended(
            ExtendedId::new(op.id)
                .ok_or_else(|| invalid_request(format!("invalid extended CAN id 0x{:x}", op.id)))?,
        )
    } else {
        socketcan::Id::Standard(
            StandardId::new(op.id as u16)
                .ok_or_else(|| invalid_request(format!("invalid standard CAN id 0x{:x}", op.id)))?,
        )
    };

    if op.fd {
        let mut frame = CanFdFrame::new(id, &op.data)
            .ok_or_else(|| invalid_request("invalid CAN FD payload"))?;
        if op.brs {
            frame.set_brs(true);
        }
        let socket = CanFdSocket::open(iface)
            .map_err(|error| map_can_io(&format!("opening an FD socket on {iface}"), error))?;
        socket
            .set_write_timeout(Some(Duration::from_millis(1_000)))
            .map_err(|error| map_can_io(&format!("setting write timeout on {iface}"), error))?;
        socket
            .write_frame(&frame)
            .map_err(|error| map_can_io(&format!("sending an FD frame on {iface}"), error))
    } else if op.rtr {
        let frame = CanRemoteFrame::new_remote(id, op.dlc as usize)
            .ok_or_else(|| invalid_request("invalid CAN RTR request"))?;
        let socket = CanSocket::open(iface)
            .map_err(|error| map_can_io(&format!("opening a socket on {iface}"), error))?;
        socket
            .set_write_timeout(Some(Duration::from_millis(1_000)))
            .map_err(|error| map_can_io(&format!("setting write timeout on {iface}"), error))?;
        socket
            .write_frame(&frame)
            .map_err(|error| map_can_io(&format!("sending an RTR frame on {iface}"), error))
    } else {
        let frame = CanDataFrame::new(id, &op.data)
            .ok_or_else(|| invalid_request("invalid classic CAN payload"))?;
        let socket = CanSocket::open(iface)
            .map_err(|error| map_can_io(&format!("opening a socket on {iface}"), error))?;
        socket
            .set_write_timeout(Some(Duration::from_millis(1_000)))
            .map_err(|error| map_can_io(&format!("setting write timeout on {iface}"), error))?;
        socket
            .write_frame(&frame)
            .map_err(|error| map_can_io(&format!("sending a frame on {iface}"), error))
    }
}

#[cfg(target_os = "linux")]
fn status_real(iface: &str) -> Result<CanLinkStatus, BusError> {
    if !interface_present(iface) {
        return Err(unavailable(format!(
            "CAN interface {iface} is not present in /sys/class/net"
        )));
    }
    let output = run_ip_status(iface)?;
    let value: Value = serde_json::from_slice(&output).map_err(|error| {
        io_failed(format!(
            "could not parse `ip -details -json` output for {iface}: {error}"
        ))
    })?;
    let entry = value
        .as_array()
        .and_then(|entries| entries.first())
        .ok_or_else(|| unavailable(format!("`ip` returned no link for {iface}")))?;
    Ok(parse_ip_status(entry))
}

#[cfg(target_os = "linux")]
fn run_ip_status(iface: &str) -> Result<Vec<u8>, BusError> {
    let mut child = Command::new("ip")
        .args([
            "-details",
            "-statistics",
            "-json",
            "link",
            "show",
            "dev",
            iface,
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| match error.kind() {
            std::io::ErrorKind::NotFound => BusError::Unsupported {
                detail: "`ip` was not found on PATH; install iproute2".to_owned(),
            },
            _ => io_failed(format!("could not start `ip` for {iface}: {error}")),
        })?;

    let timeout = Duration::from_millis(STATUS_TIMEOUT_MS);
    let deadline = Instant::now() + timeout;
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status,
            Ok(None) => {
                if Instant::now() >= deadline {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(BusError::Timeout {
                        detail: format!("`ip` status for {iface} exceeded {STATUS_TIMEOUT_MS} ms"),
                    });
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(io_failed(format!(
                    "could not wait for `ip` status on {iface}: {error}"
                )));
            }
        }
    };

    let mut stdout = Vec::new();
    if let Some(pipe) = child.stdout.take() {
        use std::io::Read as _;
        let _ = pipe
            .take(MAX_IP_OUTPUT_BYTES as u64)
            .read_to_end(&mut stdout);
    }
    let mut stderr = Vec::new();
    if let Some(pipe) = child.stderr.take() {
        use std::io::Read as _;
        let _ = pipe.take(8 * 1024).read_to_end(&mut stderr);
    }
    if !status.success() {
        let text = String::from_utf8_lossy(&stderr);
        let detail = bounded_detail(text.trim());
        if text.contains("does not exist")
            || text.contains("Cannot find device")
            || text.contains("No such device")
        {
            return Err(unavailable(format!(
                "CAN interface {iface} is not available: {detail}"
            )));
        }
        return Err(io_failed(format!(
            "`ip` status for {iface} exited with {status}: {detail}"
        )));
    }
    Ok(stdout)
}

fn parse_ip_status(entry: &Value) -> CanLinkStatus {
    let linkinfo = entry.get("linkinfo");
    let info_data = linkinfo.and_then(|link| link.get("info_data"));
    let stats = entry.get("stats64").or_else(|| entry.get("stats"));
    let counters = info_data.and_then(|data| data.get("berr_counter"));

    CanLinkStatus {
        available: true,
        operstate: entry
            .get("operstate")
            .and_then(Value::as_str)
            .map(str::to_owned),
        mtu: entry.get("mtu").and_then(Value::as_u64),
        kind: linkinfo
            .and_then(|link| link.get("info_kind"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        bitrate: info_data
            .and_then(|data| data.get("bittiming"))
            .and_then(|timing| timing.get("bitrate"))
            .or_else(|| info_data.and_then(|data| data.get("bitrate")))
            .and_then(Value::as_u64),
        state: info_data
            .and_then(|data| data.get("state"))
            .and_then(Value::as_str)
            .map(str::to_owned),
        restart_ms: info_data
            .and_then(|data| data.get("restart_ms"))
            .and_then(Value::as_u64),
        error_counters: CanErrorCounters {
            tx: counter_value(counters, &["tx", "txerr", "tx_errors"]),
            rx: counter_value(counters, &["rx", "rxerr", "rx_errors"]),
            tx_warning: counter_value(counters, &["txwarn", "tx_warning"]),
            rx_warning: counter_value(counters, &["rxwarn", "rx_warning"]),
        },
        stats: CanLinkStats {
            rx_packets: stat_value(stats, "rx", &["packets"]),
            tx_packets: stat_value(stats, "tx", &["packets"]),
            rx_errors: stat_value(stats, "rx", &["errors"]),
            tx_errors: stat_value(stats, "tx", &["errors"]),
            rx_dropped: stat_value(stats, "rx", &["dropped"]),
            tx_dropped: stat_value(stats, "tx", &["dropped"]),
            rx_over_errors: stat_value(stats, "rx", &["over_errors"]),
            rx_bytes: stat_value(stats, "rx", &["bytes"]),
            tx_bytes: stat_value(stats, "tx", &["bytes"]),
        },
    }
}

fn counter_value(container: Option<&Value>, keys: &[&str]) -> Option<u64> {
    keys.iter().find_map(|key| {
        container
            .and_then(|value| value.get(*key))
            .and_then(Value::as_u64)
    })
}

fn stat_value(stats: Option<&Value>, direction: &str, keys: &[&str]) -> Option<u64> {
    let table = stats.and_then(|value| value.get(direction));
    counter_value(table, keys)
}

#[cfg(target_os = "linux")]
fn map_can_io(context: &str, error: std::io::Error) -> BusError {
    let detail = bounded_detail(&format!("{context}: {error}"));
    match error.kind() {
        std::io::ErrorKind::PermissionDenied => BusError::PermissionDenied { detail },
        std::io::ErrorKind::NotFound => BusError::Unavailable { detail },
        std::io::ErrorKind::WouldBlock => BusError::Busy { detail },
        std::io::ErrorKind::TimedOut => BusError::Timeout { detail },
        _ => {
            if matches!(
                error.raw_os_error(),
                Some(libc::ENODEV) | Some(libc::ENXIO) | Some(libc::ENETDOWN)
            ) {
                BusError::Unavailable { detail }
            } else if matches!(error.raw_os_error(), Some(libc::EACCES) | Some(libc::EPERM)) {
                BusError::PermissionDenied { detail }
            } else {
                BusError::IoFailed { detail }
            }
        }
    }
}

fn bounded_detail(value: &str) -> String {
    value.chars().take(MAX_ERROR_DETAIL).collect()
}

/// One interface in `GET /buses/can`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanIfaceSummary {
    pub iface: String,
    pub available: bool,
    pub send: bool,
    pub capture_buffer: usize,
}

/// `GET /buses/can` response. No socket is opened and `ip` is not run.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanListResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub count: usize,
    pub limit: usize,
    pub truncated: bool,
    pub interfaces: Vec<CanIfaceSummary>,
    pub limits: BusLimits,
}

/// `GET /buses/can/{iface}/status` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanStatusResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub iface: String,
    pub source: &'static str,
    #[serde(flatten)]
    pub link: CanLinkStatus,
}

/// `GET /buses/can/{iface}/frames` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanFramesResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub iface: String,
    pub limit: usize,
    pub count: usize,
    pub matched: usize,
    pub truncated: bool,
    pub dropped: u64,
    pub total: u64,
    pub capture_started: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    pub frames: Vec<CanFrameInfo>,
}

/// `POST /buses/can/{iface}/send` response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct CanSendResponse {
    pub ok: bool,
    pub supported: bool,
    pub configured: bool,
    pub iface: String,
    pub id: u32,
    pub extended: bool,
    pub rtr: bool,
    pub fd: bool,
    pub brs: bool,
    pub dlc: u8,
    pub bytes: usize,
    pub data: String,
    pub encoding: &'static str,
}

impl BusState {
    pub(super) fn can_list(&self) -> Result<CanListResponse, BusError> {
        if !self.inner.config.can.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let limit = self.inner.config.max_devices;
        let mut interfaces: Vec<CanIfaceSummary> = self
            .inner
            .config
            .can
            .ifaces()
            .map(|iface| CanIfaceSummary {
                iface: iface.clone(),
                available: interface_present(iface),
                send: self.inner.config.can.send_allowed(iface),
                capture_buffer: self.inner.config.max_can_frames,
            })
            .collect();
        let matched = interfaces.len();
        let truncated = matched > limit;
        interfaces.truncate(limit);
        Ok(CanListResponse {
            ok: true,
            supported: true,
            configured: true,
            count: interfaces.len(),
            limit,
            truncated,
            interfaces,
            limits: self.limits(),
        })
    }

    pub(super) fn can_status(&self, iface_raw: &str) -> Result<CanStatusResponse, BusError> {
        if !self.inner.config.can.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let iface = validate_iface(iface_raw)?;
        if !self.inner.config.can.allows(&iface) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "CAN interface {iface} is not allowlisted; add --can-allow {iface}"
                ),
            });
        }
        let link = self.inner.can.status(&iface)?;
        Ok(CanStatusResponse {
            ok: true,
            supported: true,
            configured: true,
            iface,
            source: "ip -details -statistics -json",
            link,
        })
    }

    pub(super) fn can_frames(
        &self,
        iface_raw: &str,
        requested_limit: Option<usize>,
    ) -> Result<CanFramesResponse, BusError> {
        if !self.inner.config.can.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let iface = validate_iface(iface_raw)?;
        if !self.inner.config.can.allows(&iface) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "CAN interface {iface} is not allowlisted; add --can-allow {iface}"
                ),
            });
        }
        let requested = requested_limit.unwrap_or(self.inner.config.max_can_frames);
        if requested == 0 {
            return Err(invalid_request("frames limit must be at least 1"));
        }
        if requested > MAX_CAN_FRAMES {
            return Err(invalid_request(format!(
                "frames limit must not exceed {MAX_CAN_FRAMES}"
            )));
        }
        let limit = requested.min(self.inner.config.max_can_frames);
        let snapshot = self
            .inner
            .can
            .frames(&iface, limit, self.inner.config.max_can_frames)?;
        let count = snapshot.frames.len();
        Ok(CanFramesResponse {
            ok: true,
            supported: true,
            configured: true,
            iface,
            limit,
            count,
            matched: snapshot.matched,
            truncated: snapshot.matched > count,
            dropped: snapshot.dropped,
            total: snapshot.total,
            capture_started: snapshot.capture_started,
            last_error: snapshot.last_error,
            frames: snapshot.frames,
        })
    }

    pub(super) fn can_send(
        &self,
        iface_raw: &str,
        request: CanSendRequest,
    ) -> Result<CanSendResponse, BusError> {
        if !self.inner.config.can.is_configured() {
            return Err(BusError::NotConfigured);
        }
        let iface = validate_iface(iface_raw)?;
        if !self.inner.config.can.allows(&iface) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "CAN interface {iface} is not allowlisted; add --can-allow {iface}"
                ),
            });
        }
        if !self.inner.config.can.send_allowed(&iface) {
            return Err(BusError::NotAllowed {
                detail: format!(
                    "CAN interface {iface} is read-only; use --can-allow {iface}:send or --can-send-allow {iface} to opt in"
                ),
            });
        }
        let op = request.into_op()?;
        let _guard = self.lock_can_send(&iface)?;
        self.inner.can.send(&iface, &op)?;
        Ok(CanSendResponse {
            ok: true,
            supported: true,
            configured: true,
            iface,
            id: op.id,
            extended: op.extended,
            rtr: op.rtr,
            fd: op.fd,
            brs: op.brs,
            dlc: if op.rtr { op.dlc } else { op.data.len() as u8 },
            bytes: op.data.len(),
            data: super::encode_hex(&op.data),
            encoding: "hex",
        })
    }
}

#[derive(Debug, Deserialize)]
struct CanFramesQuery {
    limit: Option<String>,
}

pub(super) fn router(state: BusState) -> Router {
    Router::new()
        .route("/buses/can", get(list_handler))
        .route("/buses/can/:iface/status", get(status_handler))
        .route("/buses/can/:iface/frames", get(frames_handler))
        .route(
            "/buses/can/:iface/send",
            post(send_handler).layer(DefaultBodyLimit::max(MAX_REQUEST_BODY_BYTES)),
        )
        .with_state(state)
}

async fn list_handler(State(state): State<BusState>) -> Response {
    match tokio::task::spawn_blocking(move || state.can_list()).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "CAN list task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn status_handler(
    State(state): State<BusState>,
    AxumPath(iface): AxumPath<String>,
) -> Response {
    match tokio::task::spawn_blocking(move || state.can_status(&iface)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "CAN status task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn frames_handler(
    State(state): State<BusState>,
    AxumPath(iface): AxumPath<String>,
    Query(query): Query<CanFramesQuery>,
) -> Response {
    let limit = match query.limit {
        Some(raw) => match raw.trim().parse::<usize>() {
            Ok(value) => Some(value),
            Err(_) => {
                return error_response(invalid_request(format!(
                    "invalid frames limit {raw:?}; expected a positive integer"
                )))
            }
        },
        None => None,
    };
    match tokio::task::spawn_blocking(move || state.can_frames(&iface, limit)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "CAN frames task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

async fn send_handler(
    State(state): State<BusState>,
    AxumPath(iface): AxumPath<String>,
    body: Result<axum::Json<CanSendRequest>, JsonRejection>,
) -> Response {
    let request = match body {
        Ok(axum::Json(request)) => request,
        Err(rejection) => return rejection_response(rejection),
    };
    match tokio::task::spawn_blocking(move || state.can_send(&iface, request)).await {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(error)) => error_response(error),
        Err(error) => error_response(io_failed(format!(
            "CAN send task failed: {}",
            bounded_detail(&error.to_string())
        ))),
    }
}

fn rejection_response(rejection: JsonRejection) -> Response {
    let status = rejection.status();
    let detail = if status == StatusCode::PAYLOAD_TOO_LARGE {
        format!("request body exceeds the {MAX_REQUEST_BODY_BYTES}-byte bound")
    } else {
        format!("invalid JSON body: {}", rejection.body_text())
    };
    error_response(invalid_request(detail))
}

fn error_response(error: BusError) -> Response {
    axum::response::IntoResponse::into_response(error)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::buses::gpio::RealGpioBackend;
    use crate::buses::i2c::RealI2cBackend;
    use crate::buses::spi::RealSpiBackend;
    use crate::buses::BusConfig;
    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use http_body_util::BodyExt;
    use serde_json::{json, Value};
    use tower::ServiceExt;

    #[derive(Clone, Debug, PartialEq, Eq)]
    enum Call {
        Status(String),
        Send(String, CanSendOp),
        Frames(String, usize, usize),
    }

    struct FakeInner {
        calls: Vec<Call>,
        status: CanLinkStatus,
        send_results: VecDeque<Result<(), BusError>>,
        frame_results: VecDeque<Result<CanFrameSnapshot, BusError>>,
    }

    #[derive(Clone)]
    struct FakeCan {
        inner: Arc<Mutex<FakeInner>>,
    }

    impl Default for FakeCan {
        fn default() -> Self {
            Self {
                inner: Arc::new(Mutex::new(FakeInner {
                    calls: Vec::new(),
                    status: CanLinkStatus {
                        available: true,
                        operstate: Some("UP".to_owned()),
                        mtu: Some(16),
                        kind: Some("vcan".to_owned()),
                        bitrate: None,
                        state: Some("ERROR_ACTIVE".to_owned()),
                        restart_ms: None,
                        error_counters: CanErrorCounters::default(),
                        stats: CanLinkStats::default(),
                    },
                    send_results: VecDeque::new(),
                    frame_results: VecDeque::new(),
                })),
            }
        }
    }

    impl FakeCan {
        fn calls(&self) -> Vec<Call> {
            self.inner.lock().unwrap().calls.clone()
        }

        fn queue_frames(&self, snapshot: CanFrameSnapshot) {
            self.inner
                .lock()
                .unwrap()
                .frame_results
                .push_back(Ok(snapshot));
        }
    }

    impl CanBackend for FakeCan {
        fn status(&self, iface: &str) -> Result<CanLinkStatus, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Status(iface.to_owned()));
            Ok(inner.status.clone())
        }

        fn send(&self, iface: &str, op: &CanSendOp) -> Result<(), BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner.calls.push(Call::Send(iface.to_owned(), op.clone()));
            inner.send_results.pop_front().unwrap_or(Ok(()))
        }

        fn frames(
            &self,
            iface: &str,
            limit: usize,
            capacity: usize,
        ) -> Result<CanFrameSnapshot, BusError> {
            let mut inner = self.inner.lock().unwrap();
            inner
                .calls
                .push(Call::Frames(iface.to_owned(), limit, capacity));
            inner
                .frame_results
                .pop_front()
                .unwrap_or_else(|| Ok(CanFrameSnapshot::default()))
        }
    }

    fn config(entries: &[&str], send: &[&str]) -> BusConfig {
        BusConfig::new()
            .with_can_allow_and_send(
                &entries
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
                &send
                    .iter()
                    .map(|entry| (*entry).to_owned())
                    .collect::<Vec<_>>(),
            )
            .expect("valid CAN allowlist")
    }

    fn state(config: BusConfig, fake: FakeCan) -> BusState {
        BusState::with_all_backends(
            config,
            Arc::new(RealI2cBackend),
            Arc::new(RealSpiBackend),
            Arc::new(RealGpioBackend::default()),
            Arc::new(fake),
        )
        .expect("valid bus state")
    }

    async fn get(app: &Router, uri: &str) -> (StatusCode, Value) {
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
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    async fn post(app: &Router, uri: &str, body: Value) -> (StatusCode, Value) {
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
        (status, serde_json::from_slice(&bytes).unwrap())
    }

    #[test]
    fn allowlist_is_exact_and_send_is_opt_in() {
        let config =
            CanConfig::from_allow_entries(["vcan0:send, can1", "vcan0"], ["can1"]).unwrap();
        assert!(config.allows("vcan0"));
        assert!(config.allows("can1"));
        assert!(!config.allows("can0"));
        assert!(config.send_allowed("vcan0"));
        assert!(config.send_allowed("can1"));
        assert_eq!(config.len(), 2);

        let read_only =
            CanConfig::from_allow_entries(["vcan0"], std::iter::empty::<&str>()).unwrap();
        assert!(read_only.allows("vcan0"));
        assert!(!read_only.send_allowed("vcan0"));

        for (entries, send, why) in [
            (vec!["vcan0:ro"], vec![], "unknown suffix"),
            (vec!["bad/name"], vec![], "slash"),
            (vec!["bad\u{7}name"], vec![], "control"),
            (vec!["this-name-is-far-too-long"], vec![], "long"),
            (vec!["vcan0"], vec!["vcan1"], "send widens"),
        ] {
            assert!(
                CanConfig::from_allow_entries(entries, send).is_err(),
                "{why}"
            );
        }
    }

    #[test]
    fn interface_names_are_strict_and_path_safe() {
        for good in ["vcan0", "can0", "slcan0", "can.1", "can_1"] {
            assert_eq!(validate_iface(good).unwrap(), good);
        }
        for bad in ["", "can/0", "can 0", "-can0", ".can0", "can:0"] {
            assert!(validate_iface(bad).is_err(), "{bad}");
        }
    }

    #[tokio::test]
    async fn list_only_reads_configuration() {
        let fake = FakeCan::default();
        let app = router(state(config(&["vcan0:send", "can1"], &[]), fake.clone()));
        let (status, body) = get(&app, "/buses/can").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["configured"], true);
        assert_eq!(body["count"], 2);
        assert_eq!(body["interfaces"][0]["iface"], "can1");
        assert_eq!(body["interfaces"][0]["send"], false);
        assert_eq!(body["interfaces"][1]["iface"], "vcan0");
        assert_eq!(body["interfaces"][1]["send"], true);
        assert!(fake.calls().is_empty(), "listing must not open sockets");
    }

    #[tokio::test]
    async fn status_is_allowlisted_and_typed() {
        let fake = FakeCan::default();
        let app = router(state(config(&["vcan0"], &[]), fake.clone()));
        let (status, body) = get(&app, "/buses/can/vcan0/status").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["iface"], "vcan0");
        assert_eq!(body["operstate"], "UP");
        assert_eq!(body["state"], "ERROR_ACTIVE");
        assert_eq!(body["source"], "ip -details -statistics -json");
        assert_eq!(fake.calls(), vec![Call::Status("vcan0".to_owned())]);

        let (status, body) = get(&app, "/buses/can/can0/status").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");
        assert_eq!(
            fake.calls().len(),
            1,
            "unlisted status never reaches backend"
        );

        let (status, body) = get(&app, "/buses/can/bad%2Fname/status").await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(body["code"], "bus.invalid-request");
    }

    #[tokio::test]
    async fn send_requires_the_exact_send_opt_in_and_valid_frame() {
        let fake = FakeCan::default();
        let app = router(state(config(&["vcan0", "can1:send"], &[]), fake.clone()));

        let (status, body) = post(
            &app,
            "/buses/can/can1/send",
            json!({"id": 291, "data": "de ad be ef"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["id"], 291);
        assert_eq!(body["extended"], false);
        assert_eq!(body["bytes"], 4);
        assert_eq!(body["data"], "deadbeef");

        let (status, _body) = post(
            &app,
            "/buses/can/can1/send",
            json!({"id": 291, "data": "de ad be ef"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let calls = fake.calls();
        assert_eq!(
            calls[0],
            Call::Send(
                "can1".to_owned(),
                CanSendOp {
                    id: 291,
                    extended: false,
                    rtr: false,
                    dlc: 0,
                    fd: false,
                    brs: false,
                    data: vec![0xde, 0xad, 0xbe, 0xef],
                }
            )
        );

        let before = fake.calls().len();
        let (status, body) = post(
            &app,
            "/buses/can/vcan0/send",
            json!({"id": 1, "data": "00"}),
        )
        .await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");
        assert_eq!(fake.calls().len(), before);

        for (body, why) in [
            (json!({"id": 0x800}), "standard id too large"),
            (
                json!({"id": 0x2000_0000, "extended": true}),
                "extended id too large",
            ),
            (
                json!({"id": 1, "data": "00".repeat(9)}),
                "classic payload too large",
            ),
            (
                json!({"id": 1, "fd": true, "data": "00".repeat(65)}),
                "fd payload too large",
            ),
            (json!({"id": 1, "fd": true, "rtr": true}), "fd+rtr"),
            (json!({"id": 1, "rtr": true, "data": "00"}), "rtr data"),
            (json!({"id": 1, "rtr": true, "dlc": 9}), "rtr dlc"),
            (json!({"id": 1, "dlc": 1}), "dlc without rtr"),
            (json!({"id": 1, "brs": true}), "brs without fd"),
            (json!({"id": 1, "data": "0"}), "odd hex"),
            (json!({"id": 1, "data": "zz"}), "non-hex"),
            (json!({"id": 1, "unknown": true}), "unknown field"),
        ] {
            let (status, response) = post(&app, "/buses/can/can1/send", body).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{why}");
            assert_eq!(response["code"], "bus.invalid-request", "{why}");
        }
        assert_eq!(fake.calls().len(), before, "rejections never reach backend");
    }

    #[tokio::test]
    async fn fd_rtr_and_extended_frames_are_normalized() {
        let fake = FakeCan::default();
        let app = router(state(config(&["can1:send"], &[]), fake.clone()));

        let (status, _) = post(
            &app,
            "/buses/can/can1/send",
            json!({"id": 0x1abcdef, "extended": true, "fd": true, "brs": true, "data": "01 02"}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        let (status, body) = post(
            &app,
            "/buses/can/can1/send",
            json!({"id": 0x123, "rtr": true, "dlc": 4}),
        )
        .await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["rtr"], true);
        assert_eq!(body["dlc"], 4);

        let calls = fake.calls();
        assert_eq!(
            calls[0],
            Call::Send(
                "can1".to_owned(),
                CanSendOp {
                    id: 0x1abcdef,
                    extended: true,
                    rtr: false,
                    dlc: 0,
                    fd: true,
                    brs: true,
                    data: vec![1, 2],
                }
            )
        );
        assert_eq!(
            calls[1],
            Call::Send(
                "can1".to_owned(),
                CanSendOp {
                    id: 0x123,
                    extended: false,
                    rtr: true,
                    dlc: 4,
                    fd: false,
                    brs: false,
                    data: Vec::new(),
                }
            )
        );
    }

    #[tokio::test]
    async fn frames_are_bounded_and_limit_is_validated() {
        let fake = FakeCan::default();
        let frame = CanFrameInfo {
            id: 0x123,
            extended: false,
            rtr: false,
            fd: false,
            error: false,
            dlc: 2,
            bytes: 2,
            data: "aabb".to_owned(),
            timestamp_ms: 1,
        };
        fake.queue_frames(CanFrameSnapshot {
            frames: vec![frame],
            matched: 3,
            dropped: 1,
            total: 4,
            capture_started: true,
            last_error: None,
        });
        let mut bus_config = config(&["vcan0"], &[]);
        bus_config.max_can_frames = 16;
        let app = router(state(bus_config, fake.clone()));

        let (status, body) = get(&app, "/buses/can/vcan0/frames?limit=32").await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["limit"], 16, "request is clamped to the buffer bound");
        assert_eq!(body["count"], 1);
        assert_eq!(body["matched"], 3);
        assert_eq!(body["truncated"], true);
        assert_eq!(body["dropped"], 1);
        assert_eq!(body["frames"][0]["data"], "aabb");
        assert_eq!(fake.calls(), vec![Call::Frames("vcan0".to_owned(), 16, 16)]);

        for query in ["limit=0", "limit=1025", "limit=nope", "limit=-1"] {
            let (status, response) = get(&app, &format!("/buses/can/vcan0/frames?{query}")).await;
            assert_eq!(status, StatusCode::BAD_REQUEST, "{query}");
            assert_eq!(response["code"], "bus.invalid-request", "{query}");
        }

        let (status, body) = get(&app, "/buses/can/can0/frames").await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.not-allowed");
    }

    #[tokio::test]
    async fn backend_errors_are_typed() {
        let fake = FakeCan::default();
        fake.inner
            .lock()
            .unwrap()
            .send_results
            .push_back(Err(BusError::Busy {
                detail: "tx queue full".to_owned(),
            }));
        let app = router(state(config(&["vcan0:send"], &[]), fake));
        let (status, body) = post(&app, "/buses/can/vcan0/send", json!({"id": 1})).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert_eq!(body["code"], "bus.busy");

        let fake = FakeCan::default();
        fake.inner
            .lock()
            .unwrap()
            .send_results
            .push_back(Err(BusError::PermissionDenied {
                detail: "EACCES".to_owned(),
            }));
        let app = router(state(config(&["vcan0:send"], &[]), fake));
        let (status, body) = post(&app, "/buses/can/vcan0/send", json!({"id": 1})).await;
        assert_eq!(status, StatusCode::FORBIDDEN);
        assert_eq!(body["code"], "bus.permission-denied");
    }

    #[tokio::test]
    async fn unconfigured_state_is_typed() {
        let fake = FakeCan::default();
        let app = router(state(BusConfig::default(), fake.clone()));
        for uri in [
            "/buses/can",
            "/buses/can/vcan0/status",
            "/buses/can/vcan0/frames",
        ] {
            let (status, body) = get(&app, uri).await;
            assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE, "{uri}");
            assert_eq!(body["code"], "bus.not-configured", "{uri}");
        }
        let (status, body) = post(&app, "/buses/can/vcan0/send", json!({"id": 1})).await;
        assert_eq!(status, StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body["code"], "bus.not-configured");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn ip_json_is_parsed_without_inventing_fields() {
        let entry = json!({
            "ifname": "can0",
            "operstate": "UP",
            "mtu": 16,
            "linkinfo": {
                "info_kind": "can",
                "info_data": {
                    "state": "ERROR_WARNING",
                    "restart_ms": 100,
                    "bittiming": {"bitrate": 500000},
                    "berr_counter": {"tx": 3, "rx": 7, "txwarn": 1, "rxwarn": 2}
                }
            },
            "stats64": {
                "rx": {"packets": 10, "errors": 1, "dropped": 2, "over_errors": 3, "bytes": 80},
                "tx": {"packets": 9, "errors": 4, "dropped": 5, "bytes": 72}
            }
        });
        let status = parse_ip_status(&entry);
        assert!(status.available);
        assert_eq!(status.operstate.as_deref(), Some("UP"));
        assert_eq!(status.kind.as_deref(), Some("can"));
        assert_eq!(status.bitrate, Some(500_000));
        assert_eq!(status.state.as_deref(), Some("ERROR_WARNING"));
        assert_eq!(status.restart_ms, Some(100));
        assert_eq!(status.error_counters.tx, Some(3));
        assert_eq!(status.error_counters.rx, Some(7));
        assert_eq!(status.stats.rx_packets, Some(10));
        assert_eq!(status.stats.tx_packets, Some(9));
        assert_eq!(status.stats.rx_over_errors, Some(3));

        let bare = parse_ip_status(&json!({"ifname": "vcan0", "linkinfo": {"info_kind": "vcan"}}));
        assert_eq!(bare.bitrate, None);
        assert_eq!(bare.error_counters, CanErrorCounters::default());
        assert_eq!(bare.stats, CanLinkStats::default());
    }

    /// Live evidence against a Linux `vcan` interface.
    ///
    /// Ignored by default. Create the interface explicitly; never point this
    /// test at a physical CAN controller:
    ///
    /// ```text
    /// sudo modprobe vcan
    /// sudo ip link add dev vcan0 type vcan
    /// sudo ip link set up vcan0
    /// sudo env OZMA_LIVE_CAN_IFACE=vcan0 cargo test -p ozma-node --lib \
    ///   buses::can::tests::live_vcan_send_capture_status -- --ignored --nocapture
    /// sudo ip link del dev vcan0
    /// sudo modprobe -r vcan
    /// ```
    #[test]
    #[ignore = "requires a live Linux vcan interface; set OZMA_LIVE_CAN_IFACE"]
    fn live_vcan_send_capture_status() {
        let iface = std::env::var("OZMA_LIVE_CAN_IFACE").unwrap_or_else(|_| "vcan0".to_owned());
        let allow = format!("{iface}:send");
        let config = BusConfig::new()
            .with_can_allow(&[allow])
            .expect("valid allowlist");
        let state = BusState::new(config).expect("valid state");

        let status = state.can_status(&iface).expect("status");
        assert!(status.link.available);
        // vcan reports UNKNOWN while its flags (set by `ip link set up`) are
        // UP; a physical controller reports UP here.
        assert!(
            matches!(
                status.link.operstate.as_deref(),
                Some("UP") | Some("UNKNOWN")
            ),
            "unexpected operstate: {:?}",
            status.link.operstate
        );
        assert_eq!(status.link.kind.as_deref(), Some("vcan"));

        // Start the capture before sending so the receive socket is bound.
        let _ = state.can_frames(&iface, Some(32)).expect("initial frames");

        state
            .can_send(
                &iface,
                CanSendRequest {
                    id: 0x123,
                    extended: false,
                    data: Some("de ad be ef".to_owned()),
                    rtr: false,
                    dlc: None,
                    fd: false,
                    brs: false,
                },
            )
            .expect("classic send");
        state
            .can_send(
                &iface,
                CanSendRequest {
                    id: 0x1abcdef,
                    extended: true,
                    data: Some("01 02 03".to_owned()),
                    rtr: false,
                    dlc: None,
                    fd: false,
                    brs: false,
                },
            )
            .expect("extended send");

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut seen_classic = false;
        let mut seen_extended = false;
        while Instant::now() < deadline {
            let frames = state.can_frames(&iface, Some(64)).expect("frames");
            for frame in &frames.frames {
                if frame.id == 0x123 && !frame.extended && frame.data == "deadbeef" {
                    seen_classic = true;
                }
                if frame.id == 0x1abcdef && frame.extended && frame.data == "010203" {
                    seen_extended = true;
                }
            }
            if seen_classic && seen_extended {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(seen_classic, "classic frame was not captured");
        assert!(seen_extended, "extended frame was not captured");

        let frames = state.can_frames(&iface, Some(64)).expect("final frames");
        assert!(frames.capture_started);
        assert!(frames.total >= 2);
    }
}
