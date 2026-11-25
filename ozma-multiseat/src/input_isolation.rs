// SPDX-License-Identifier: AGPL-3.0-only
//! Per-seat physical input isolation through a privileged helper.
//!
//! The supervisor links no evdev/uinput code and never opens `/dev/input`.
//! It talks to the `ozma-seat-input` helper over a line-delimited JSON
//! protocol on the helper's stdin/stdout. The helper is the only component
//! that opens `/dev/input/event*`, calls `EVIOCGRAB`, or creates `/dev/uinput`
//! devices. This crate keeps `#![forbid(unsafe_code)]`; the helper binary owns
//! its single unsafe module.
//!
//! The same module contains the side-effect-free helper state machine
//! ([`InputIsolationHelper`]) used by the binary. Tests drive it with a fake
//! [`HelperBackend`], so the default suite never opens a device.
//!
//! Isolation is only reported as applied after the helper confirms both the
//! exclusive grab and the per-seat uinput device. A failed grab, uinput
//! creation, or cross-seat conflict is a typed error and no partial assignment
//! is retained.

use std::collections::BTreeMap;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::MultiseatError;
use crate::input::{InputDevice, InputDeviceKind, InputGroup};

/// Helper binary name, resolved through `PATH` by the supervisor client.
pub const HELPER_PROGRAM: &str = "ozma-seat-input";
/// Current line protocol version.
pub const HELPER_PROTOCOL_VERSION: u32 = 1;
/// The only evdev paths the helper accepts.
pub const DEVICE_PATH_PREFIX: &str = "/dev/input/event";
/// Stable input-router name reported when the helper owns a seat.
pub const INPUT_ISOLATION_ROUTER_NAME: &str = "seat-input-helper";
/// How long the client waits for a graceful helper exit.
const HELPER_TERMINATE_GRACE: Duration = Duration::from_secs(2);

/// Which backend a helper process is using.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperBackendKind {
    /// Real evdev/uinput backend.
    Real,
    /// Test-only backend that records requests and touches no device.
    Fake,
}

impl HelperBackendKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Real => "real",
            Self::Fake => "fake",
        }
    }
}

/// One physical evdev device the helper should grab for a seat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperDevice {
    /// Absolute evdev path (`/dev/input/eventN`).
    pub path: String,
    /// Kernel/device name when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Capability class, used to build the per-seat uinput device.
    pub kind: InputDeviceKind,
}

impl HelperDevice {
    /// Build a device request.
    pub fn new(path: impl Into<String>, kind: InputDeviceKind) -> Self {
        Self {
            path: path.into(),
            name: None,
            kind,
        }
    }

    /// Attach the human-readable device name.
    pub fn with_name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }
}

impl From<&InputDevice> for HelperDevice {
    fn from(device: &InputDevice) -> Self {
        Self {
            path: device.path.clone(),
            name: Some(device.name.clone()),
            kind: device.kind,
        }
    }
}

/// Typed helper failure code. A supervisor maps these to a typed
/// [`MultiseatError::InputIsolation`] without guessing what failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HelperErrorCode {
    /// Malformed request or invalid seat id.
    InvalidRequest,
    /// Device path is not `/dev/input/event<digits>`.
    InvalidPath,
    /// The same path appeared twice in one request.
    DuplicateDevice,
    /// The path is already grabbed for another seat.
    CrossSeatConflict,
    /// The seat already owns a different device set.
    SeatAlreadyGrabbed,
    /// Opening or `EVIOCGRAB` failed.
    GrabFailed,
    /// uinput device creation or relay setup failed.
    UinputFailed,
    /// The seat has no assignment.
    NotGrabbed,
    /// Any other platform/backend failure.
    Backend,
    /// Protocol version or frame error.
    Protocol,
}

impl HelperErrorCode {
    /// Stable wire name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::InvalidRequest => "invalid_request",
            Self::InvalidPath => "invalid_path",
            Self::DuplicateDevice => "duplicate_device",
            Self::CrossSeatConflict => "cross_seat_conflict",
            Self::SeatAlreadyGrabbed => "seat_already_grabbed",
            Self::GrabFailed => "grab_failed",
            Self::UinputFailed => "uinput_failed",
            Self::NotGrabbed => "not_grabbed",
            Self::Backend => "backend",
            Self::Protocol => "protocol",
        }
    }
}

/// Typed helper error carried by a protocol response.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperError {
    /// Stable error class.
    pub code: HelperErrorCode,
    /// Operator-facing detail.
    pub message: String,
    /// Seat the error belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub seat_id: Option<String>,
    /// Device the error belongs to, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device: Option<String>,
}

impl HelperError {
    /// Build an error.
    pub fn new(code: HelperErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            seat_id: None,
            device: None,
        }
    }

    /// Attach a seat id.
    pub fn with_seat(mut self, seat_id: impl Into<String>) -> Self {
        self.seat_id = Some(seat_id.into());
        self
    }

    /// Attach a device path.
    pub fn with_device(mut self, device: impl Into<String>) -> Self {
        self.device = Some(device.into());
        self
    }
}

/// Per-seat state reported by the helper.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct HelperSeatStatus {
    /// Stable seat id.
    pub seat_id: String,
    /// Grabbed physical paths, in request order.
    pub grabbed: Vec<String>,
    /// Created uinput device names, in request order.
    pub uinput_devices: Vec<String>,
}

/// Request frame. One JSON object per line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum HelperRequest {
    /// Protocol negotiation. Must be first.
    Hello {
        /// Client protocol version.
        protocol: u32,
    },
    /// Grab a complete device set for one seat.
    Grab {
        /// Seat that will own the devices.
        seat_id: String,
        /// Devices to grab.
        devices: Vec<HelperDevice>,
    },
    /// Release one seat's devices (idempotent).
    Release {
        /// Seat to release.
        seat_id: String,
    },
    /// Report current assignments.
    Status,
    /// Release everything and exit.
    Shutdown,
}

/// Response frame. One JSON object per line.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum HelperResponse {
    /// Helper is ready. Carries the negotiated protocol and backend.
    Ready {
        /// Protocol version in use.
        protocol: u32,
        /// Helper binary name.
        helper: String,
        /// Backend in use.
        backend: HelperBackendKind,
        /// Helper process id.
        pid: u32,
    },
    /// Every requested device was grabbed and a uinput device was created.
    Grabbed {
        /// Seat id.
        seat_id: String,
        /// Grabbed paths.
        grabbed: Vec<String>,
        /// Created uinput names.
        uinput_devices: Vec<String>,
    },
    /// Devices were released (possibly none).
    Released {
        /// Seat id.
        seat_id: String,
        /// Released paths.
        released: Vec<String>,
    },
    /// Current assignments.
    Status {
        /// One entry per seat.
        seats: Vec<HelperSeatStatus>,
    },
    /// Helper is releasing everything and exiting.
    ShuttingDown,
    /// Typed failure.
    Error {
        /// Error detail.
        error: HelperError,
    },
}

/// Serialize one request without a trailing newline.
pub fn encode_request(request: &HelperRequest) -> Result<String, MultiseatError> {
    serde_json::to_string(request).map_err(|error| {
        MultiseatError::InputIsolation(format!("encoding helper request: {error}"))
    })
}

/// Decode one request line.
pub fn decode_request(line: &str) -> Result<HelperRequest, MultiseatError> {
    serde_json::from_str(line).map_err(|error| {
        MultiseatError::InputIsolation(format!("decoding helper request: {error}"))
    })
}

/// Serialize one response without a trailing newline.
pub fn encode_response(response: &HelperResponse) -> Result<String, MultiseatError> {
    serde_json::to_string(response).map_err(|error| {
        MultiseatError::InputIsolation(format!("encoding helper response: {error}"))
    })
}

/// Decode one response line.
pub fn decode_response(line: &str) -> Result<HelperResponse, MultiseatError> {
    serde_json::from_str(line).map_err(|error| {
        MultiseatError::InputIsolation(format!("decoding helper response: {error}"))
    })
}

/// Validate the only paths the helper will open.
///
/// The check is deliberately lexical: `/dev/input/event` plus decimal digits
/// and nothing else. Symlinks, `..`, by-id paths, and other device classes are
/// rejected before any backend call.
pub fn validate_device_path(path: &str) -> Result<(), HelperError> {
    let Some(suffix) = path.strip_prefix(DEVICE_PATH_PREFIX) else {
        return Err(HelperError::new(
            HelperErrorCode::InvalidPath,
            format!("device path {path:?} is not {DEVICE_PATH_PREFIX}<n>"),
        )
        .with_device(path));
    };
    if suffix.is_empty() || !suffix.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(HelperError::new(
            HelperErrorCode::InvalidPath,
            format!("device path {path:?} must end in decimal event digits"),
        )
        .with_device(path));
    }
    if suffix.len() > 8 {
        return Err(HelperError::new(
            HelperErrorCode::InvalidPath,
            format!("device path {path:?} has an implausible event number"),
        )
        .with_device(path));
    }
    Ok(())
}

fn validate_seat_id(seat_id: &str) -> Result<(), HelperError> {
    if seat_id.is_empty()
        || seat_id.len() > 128
        || !seat_id
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':'))
    {
        return Err(HelperError::new(
            HelperErrorCode::InvalidRequest,
            format!("invalid seat id {seat_id:?}"),
        )
        .with_seat(seat_id));
    }
    Ok(())
}

/// Prefix shared by every helper-created uinput device.
///
/// The private-display content filter uses this prefix to recognise devices
/// that belong to a seat and should therefore be enabled or disabled together.
pub const UINPUT_NAME_PREFIX: &str = "ozma-seat-";

/// Short capability token embedded in a uinput device name.
pub fn uinput_kind_token(kind: InputDeviceKind) -> &'static str {
    match kind {
        InputDeviceKind::Keyboard => "kbd",
        InputDeviceKind::Mouse => "mouse",
        InputDeviceKind::Gamepad => "pad",
        InputDeviceKind::Other => "other",
    }
}

/// Whether a device name was created by the seat-input helper.
pub fn is_uinput_name(name: &str) -> bool {
    name.starts_with(UINPUT_NAME_PREFIX)
}

/// Build the per-seat uinput name, for example
/// `ozma-seat-host-seat-0-kbd-event0`.
///
/// The name carries the seat id, the capability class, and the source event
/// node. The class token lets a display targeter match the seat's keyboard,
/// mouse, and gamepad devices without parsing a hyphenated seat id. The whole
/// name stays below the kernel's 80-byte limit.
pub fn default_uinput_name(seat_id: &str, kind: InputDeviceKind, device_path: &str) -> String {
    let event = device_path.rsplit('/').next().unwrap_or("event");
    let seat: String = seat_id
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || character == '-' || character == '_' {
                character
            } else {
                '_'
            }
        })
        .take(36)
        .collect();
    format!(
        "{UINPUT_NAME_PREFIX}{seat}-{}-{event}",
        uinput_kind_token(kind)
    )
}

/// One device in a successful assignment.
#[derive(Clone, Debug, PartialEq, Eq)]
struct IsolatedDevice {
    path: String,
    uinput: String,
}

/// Owned assignment for one seat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SeatIsolation {
    devices: Vec<IsolatedDevice>,
}

/// Side-effect seam for the helper: exclusive grab, uinput creation, and the
/// event relay. The real implementation lives only in the helper binary; tests
/// inject a fake.
pub trait HelperBackend {
    /// Which backend this is.
    fn backend_kind(&self) -> HelperBackendKind;

    /// Open `device.path` and issue `EVIOCGRAB`.
    fn grab(&mut self, device: &HelperDevice) -> Result<(), HelperError>;

    /// Release a grab without destroying the uinput device yet.
    fn ungrab(&mut self, path: &str) -> Result<(), HelperError>;

    /// Create a per-seat uinput device and return its name.
    fn create_uinput(
        &mut self,
        seat_id: &str,
        device: &HelperDevice,
    ) -> Result<String, HelperError>;

    /// Start relaying events from the grabbed device to the uinput device.
    fn start_relay(&mut self, device: &HelperDevice, uinput_name: &str) -> Result<(), HelperError>;

    /// Stop the relay and release the exclusive grab. Implementations must be
    /// idempotent: a device that already exited its relay is not an error.
    fn stop_relay(&mut self, path: &str) -> Result<(), HelperError>;

    /// Destroy one uinput device.
    fn destroy_uinput(&mut self, name: &str) -> Result<(), HelperError>;
}

/// Testable helper state machine.
pub struct InputIsolationHelper<B: HelperBackend> {
    backend: B,
    helper_name: String,
    pid: u32,
    seats: BTreeMap<String, SeatIsolation>,
    shutting_down: bool,
}

impl<B: HelperBackend> InputIsolationHelper<B> {
    /// Build a helper around a backend.
    pub fn new(backend: B, helper_name: impl Into<String>, pid: u32) -> Self {
        Self {
            backend,
            helper_name: helper_name.into(),
            pid,
            seats: BTreeMap::new(),
            shutting_down: false,
        }
    }

    /// Backend kind in use.
    pub fn backend_kind(&self) -> HelperBackendKind {
        self.backend.backend_kind()
    }

    /// The readiness response for this process.
    pub fn ready(&self) -> HelperResponse {
        HelperResponse::Ready {
            protocol: HELPER_PROTOCOL_VERSION,
            helper: self.helper_name.clone(),
            backend: self.backend.backend_kind(),
            pid: self.pid,
        }
    }

    /// Current assignment count.
    pub fn seat_count(&self) -> usize {
        self.seats.len()
    }

    /// Handle one request. Never panics on malformed or conflicting input.
    pub fn handle(&mut self, request: HelperRequest) -> HelperResponse {
        if self.shutting_down {
            return error_response(HelperError::new(
                HelperErrorCode::InvalidRequest,
                "helper is shutting down",
            ));
        }
        match request {
            HelperRequest::Hello { protocol } => {
                if protocol != HELPER_PROTOCOL_VERSION {
                    return error_response(HelperError::new(
                        HelperErrorCode::Protocol,
                        format!(
                            "protocol {protocol} is not supported (expected {HELPER_PROTOCOL_VERSION})"
                        ),
                    ));
                }
                self.ready()
            }
            HelperRequest::Grab { seat_id, devices } => self.grab_seat(&seat_id, devices),
            HelperRequest::Release { seat_id } => self.release_seat(&seat_id),
            HelperRequest::Status => HelperResponse::Status {
                seats: self
                    .seats
                    .iter()
                    .map(|(seat_id, assignment)| HelperSeatStatus {
                        seat_id: seat_id.clone(),
                        grabbed: assignment
                            .devices
                            .iter()
                            .map(|device| device.path.clone())
                            .collect(),
                        uinput_devices: assignment
                            .devices
                            .iter()
                            .map(|device| device.uinput.clone())
                            .collect(),
                    })
                    .collect(),
            },
            HelperRequest::Shutdown => {
                self.shutting_down = true;
                self.release_all();
                HelperResponse::ShuttingDown
            }
        }
    }

    fn grab_seat(&mut self, seat_id: &str, devices: Vec<HelperDevice>) -> HelperResponse {
        if let Err(error) = validate_seat_id(seat_id) {
            return error_response(error);
        }
        if devices.is_empty() {
            return error_response(
                HelperError::new(
                    HelperErrorCode::InvalidRequest,
                    "grab requires at least one device",
                )
                .with_seat(seat_id),
            );
        }

        let mut seen = std::collections::BTreeSet::new();
        for device in &devices {
            if let Err(mut error) = validate_device_path(&device.path) {
                error.seat_id = Some(seat_id.to_owned());
                return error_response(error);
            }
            if !seen.insert(device.path.clone()) {
                return error_response(
                    HelperError::new(
                        HelperErrorCode::DuplicateDevice,
                        format!("device {:?} appears more than once", device.path),
                    )
                    .with_seat(seat_id)
                    .with_device(&device.path),
                );
            }
        }

        if let Some(existing) = self.seats.get(seat_id) {
            if same_device_set(existing, &devices) {
                return self.grabbed_response(seat_id, existing);
            }
            return error_response(
                HelperError::new(
                    HelperErrorCode::SeatAlreadyGrabbed,
                    format!("seat {seat_id} already owns a different device set; release it first"),
                )
                .with_seat(seat_id),
            );
        }

        for other in self.seats.values() {
            for device in &devices {
                if other.devices.iter().any(|owned| owned.path == device.path) {
                    return error_response(
                        HelperError::new(
                            HelperErrorCode::CrossSeatConflict,
                            format!(
                                "device {:?} is already grabbed for another seat",
                                device.path
                            ),
                        )
                        .with_seat(seat_id)
                        .with_device(&device.path),
                    );
                }
            }
        }

        let mut completed: Vec<IsolatedDevice> = Vec::new();
        for device in &devices {
            if let Err(error) = self.backend.grab(device) {
                let rollback = self.rollback(&mut completed);
                return error_response(attach_device(error, &device.path, seat_id, rollback));
            }
            let uinput = match self.backend.create_uinput(seat_id, device) {
                Ok(name) => name,
                Err(error) => {
                    let cleanup = self.backend.ungrab(&device.path).err();
                    let rollback = self.rollback(&mut completed);
                    return error_response(attach_device(
                        error,
                        &device.path,
                        seat_id,
                        merge_rollback(rollback, cleanup.map(|e| e.message)),
                    ));
                }
            };
            if let Err(error) = self.backend.start_relay(device, &uinput) {
                let _ = self.backend.destroy_uinput(&uinput);
                let cleanup = self.backend.ungrab(&device.path).err();
                let rollback = self.rollback(&mut completed);
                return error_response(attach_device(
                    error,
                    &device.path,
                    seat_id,
                    merge_rollback(rollback, cleanup.map(|e| e.message)),
                ));
            }
            completed.push(IsolatedDevice {
                path: device.path.clone(),
                uinput,
            });
        }

        let assignment = SeatIsolation { devices: completed };
        self.seats.insert(seat_id.to_owned(), assignment.clone());
        self.grabbed_response(seat_id, &assignment)
    }

    fn grabbed_response(&self, seat_id: &str, assignment: &SeatIsolation) -> HelperResponse {
        HelperResponse::Grabbed {
            seat_id: seat_id.to_owned(),
            grabbed: assignment
                .devices
                .iter()
                .map(|device| device.path.clone())
                .collect(),
            uinput_devices: assignment
                .devices
                .iter()
                .map(|device| device.uinput.clone())
                .collect(),
        }
    }

    fn release_seat(&mut self, seat_id: &str) -> HelperResponse {
        if let Err(error) = validate_seat_id(seat_id) {
            return error_response(error);
        }
        let Some(assignment) = self.seats.remove(seat_id) else {
            // Release is idempotent so a supervisor can retry a rollback after
            // a helper restart without inventing an error.
            return HelperResponse::Released {
                seat_id: seat_id.to_owned(),
                released: Vec::new(),
            };
        };

        let mut released = Vec::new();
        let mut remaining = Vec::new();
        let mut failures = Vec::new();
        for device in assignment.devices.iter().rev() {
            if let Err(error) = self.backend.stop_relay(&device.path) {
                failures.push(format!("{}: {}", device.path, error.message));
                remaining.push(device.clone());
                continue;
            }
            if let Err(error) = self.backend.destroy_uinput(&device.uinput) {
                failures.push(format!("{}: {}", device.uinput, error.message));
                remaining.push(device.clone());
                continue;
            }
            released.push(device.path.clone());
        }
        released.reverse();

        if !failures.is_empty() {
            remaining.reverse();
            self.seats
                .insert(seat_id.to_owned(), SeatIsolation { devices: remaining });
            return error_response(
                HelperError::new(
                    HelperErrorCode::Backend,
                    format!("could not release every device: {}", failures.join("; ")),
                )
                .with_seat(seat_id),
            );
        }

        HelperResponse::Released {
            seat_id: seat_id.to_owned(),
            released,
        }
    }

    fn rollback(&mut self, completed: &mut Vec<IsolatedDevice>) -> Option<String> {
        let mut failures = Vec::new();
        for device in completed.drain(..).rev() {
            if let Err(error) = self.backend.stop_relay(&device.path) {
                failures.push(format!("{}: {}", device.path, error.message));
            }
            if let Err(error) = self.backend.destroy_uinput(&device.uinput) {
                failures.push(format!("{}: {}", device.uinput, error.message));
            }
        }
        if failures.is_empty() {
            None
        } else {
            Some(failures.join("; "))
        }
    }

    fn release_all(&mut self) {
        let seat_ids: Vec<String> = self.seats.keys().cloned().collect();
        for seat_id in seat_ids {
            let _ = self.release_seat(&seat_id);
        }
    }
}

fn same_device_set(existing: &SeatIsolation, requested: &[HelperDevice]) -> bool {
    existing.devices.len() == requested.len()
        && requested.iter().all(|device| {
            existing
                .devices
                .iter()
                .any(|owned| owned.path == device.path)
        })
}

fn attach_device(
    mut error: HelperError,
    device: &str,
    seat_id: &str,
    rollback: Option<String>,
) -> HelperError {
    if error.device.is_none() {
        error.device = Some(device.to_owned());
    }
    if error.seat_id.is_none() {
        error.seat_id = Some(seat_id.to_owned());
    }
    if let Some(rollback) = rollback {
        error.message = format!("{}; rollback: {rollback}", error.message);
    }
    error
}

fn merge_rollback(rollback: Option<String>, cleanup: Option<String>) -> Option<String> {
    match (rollback, cleanup) {
        (Some(left), Some(right)) => Some(format!("{left}; {right}")),
        (Some(value), None) | (None, Some(value)) => Some(value),
        (None, None) => None,
    }
}

fn error_response(error: HelperError) -> HelperResponse {
    HelperResponse::Error { error }
}

/// One seat's confirmed isolation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputIsolationAssignment {
    /// Seat id.
    pub seat_id: String,
    /// Grabbed physical paths.
    pub grabbed: Vec<String>,
    /// Created uinput device names.
    pub uinput_devices: Vec<String>,
}

/// Result of releasing one seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InputIsolationRelease {
    /// Seat id.
    pub seat_id: String,
    /// Released physical paths.
    pub released: Vec<String>,
}

/// Supervisor-facing seam. Implementations must never report a grab they did
/// not confirm.
pub trait SeatInputHelper: Send {
    /// Start the helper if needed and wait for its readiness response.
    fn ensure_ready(&mut self) -> Result<HelperResponse, MultiseatError>;

    /// Grab a full device set for one seat.
    fn grab(
        &mut self,
        seat_id: &str,
        devices: &[HelperDevice],
    ) -> Result<InputIsolationAssignment, MultiseatError>;

    /// Release one seat. Implementations may treat a dead helper as an empty
    /// successful release because the kernel drops grabs when its fds close.
    fn release(&mut self, seat_id: &str) -> Result<InputIsolationRelease, MultiseatError>;

    /// Current helper assignment for one seat, or `None` when the helper has
    /// no grab (or is not running).
    fn seat_status(&mut self, seat_id: &str) -> Result<Option<HelperSeatStatus>, MultiseatError>;

    /// Ask the helper to release everything and stop it.
    fn shutdown(&mut self);
}

/// Client configuration for the real helper process.
#[derive(Clone, Debug)]
pub struct HelperClientConfig {
    /// Program to execute.
    pub program: String,
    /// Arguments after the program name.
    pub args: Vec<String>,
    /// Allow a `fake` readiness response. The CLI leaves this `false`; only
    /// process-protocol tests enable it.
    pub allow_fake_backend: bool,
}

impl Default for HelperClientConfig {
    fn default() -> Self {
        Self {
            program: HELPER_PROGRAM.to_owned(),
            args: Vec::new(),
            allow_fake_backend: false,
        }
    }
}

/// Non-privileged supervisor client for [`HELPER_PROGRAM`].
///
/// The helper is spawned lazily on first use. If it exits, the next call
/// starts a fresh process; a grab is never assumed to have survived that
/// restart.
pub struct HelperClient {
    config: HelperClientConfig,
    child: Option<Child>,
    stdin: Option<ChildStdin>,
    stdout: Option<BufReader<ChildStdout>>,
    ready: Option<HelperResponse>,
}

impl HelperClient {
    /// Build a client. No process is started yet.
    pub fn new(config: HelperClientConfig) -> Self {
        Self {
            config,
            child: None,
            stdin: None,
            stdout: None,
            ready: None,
        }
    }

    /// Configuration in use.
    pub fn config(&self) -> &HelperClientConfig {
        &self.config
    }

    /// Whether the helper process is currently alive.
    pub fn is_alive(&mut self) -> bool {
        match self.child.as_mut() {
            Some(child) => matches!(child.try_wait(), Ok(None)),
            None => false,
        }
    }

    fn clear(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.kill();
            let _ = child.wait();
        }
        self.stdin = None;
        self.stdout = None;
        self.ready = None;
    }

    fn start(&mut self) -> Result<HelperResponse, MultiseatError> {
        self.clear();
        let mut command = Command::new(&self.config.program);
        command
            .args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit());
        let mut child = command.spawn().map_err(|error| {
            MultiseatError::InputIsolation(format!(
                "failed to start input helper {:?}: {error}",
                self.config.program
            ))
        })?;
        let stdin = child.stdin.take().ok_or_else(|| {
            let _ = child.kill();
            MultiseatError::InputIsolation("input helper has no stdin pipe".to_owned())
        })?;
        let stdout = child.stdout.take().ok_or_else(|| {
            let _ = child.kill();
            MultiseatError::InputIsolation("input helper has no stdout pipe".to_owned())
        })?;
        self.child = Some(child);
        self.stdin = Some(stdin);
        self.stdout = Some(BufReader::new(stdout));

        let response = self.round_trip(HelperRequest::Hello {
            protocol: HELPER_PROTOCOL_VERSION,
        })?;
        self.accept_ready(response)
    }

    fn accept_ready(&mut self, response: HelperResponse) -> Result<HelperResponse, MultiseatError> {
        match response {
            HelperResponse::Ready {
                protocol,
                helper,
                backend,
                pid,
            } => {
                if protocol != HELPER_PROTOCOL_VERSION {
                    let error = MultiseatError::InputIsolation(format!(
                        "helper {helper} speaks protocol {protocol}, expected {HELPER_PROTOCOL_VERSION}"
                    ));
                    self.clear();
                    return Err(error);
                }
                if backend == HelperBackendKind::Fake && !self.config.allow_fake_backend {
                    let error = MultiseatError::InputIsolation(
                        "input helper reported the test-only fake backend; refusing to claim \
                         input isolation"
                            .to_owned(),
                    );
                    self.clear();
                    return Err(error);
                }
                let ready = HelperResponse::Ready {
                    protocol,
                    helper,
                    backend,
                    pid,
                };
                self.ready = Some(ready.clone());
                Ok(ready)
            }
            HelperResponse::Error { error } => {
                self.clear();
                Err(helper_error(error))
            }
            other => {
                self.clear();
                Err(protocol_error(format!(
                    "expected a ready response, got {other:?}"
                )))
            }
        }
    }

    fn round_trip(&mut self, request: HelperRequest) -> Result<HelperResponse, MultiseatError> {
        let encoded = encode_request(&request)?;
        let stdin = self.stdin.as_mut().ok_or_else(|| {
            MultiseatError::InputIsolation("input helper is not running".to_owned())
        })?;
        stdin.write_all(encoded.as_bytes()).map_err(|error| {
            MultiseatError::InputIsolation(format!("writing helper request: {error}"))
        })?;
        stdin.write_all(b"\n").map_err(|error| {
            MultiseatError::InputIsolation(format!("writing helper request: {error}"))
        })?;
        stdin.flush().map_err(|error| {
            MultiseatError::InputIsolation(format!("flushing helper request: {error}"))
        })?;

        let stdout = self.stdout.as_mut().ok_or_else(|| {
            MultiseatError::InputIsolation("input helper has no stdout".to_owned())
        })?;
        let mut line = String::new();
        let read = stdout.read_line(&mut line).map_err(|error| {
            MultiseatError::InputIsolation(format!("reading helper response: {error}"))
        })?;
        if read == 0 {
            return Err(MultiseatError::InputIsolation(
                "input helper exited without a response".to_owned(),
            ));
        }
        let response = decode_response(line.trim())?;
        match response {
            HelperResponse::Error { error } => Err(helper_error(error)),
            other => Ok(other),
        }
    }
}

impl SeatInputHelper for HelperClient {
    fn ensure_ready(&mut self) -> Result<HelperResponse, MultiseatError> {
        if self.is_alive() {
            if let Some(ready) = self.ready.clone() {
                return Ok(ready);
            }
            let response = self.round_trip(HelperRequest::Hello {
                protocol: HELPER_PROTOCOL_VERSION,
            })?;
            return self.accept_ready(response);
        }
        self.start()
    }

    fn grab(
        &mut self,
        seat_id: &str,
        devices: &[HelperDevice],
    ) -> Result<InputIsolationAssignment, MultiseatError> {
        self.ensure_ready()?;
        let response = self.round_trip(HelperRequest::Grab {
            seat_id: seat_id.to_owned(),
            devices: devices.to_vec(),
        })?;
        match response {
            HelperResponse::Grabbed {
                seat_id,
                grabbed,
                uinput_devices,
            } => {
                let requested: Vec<String> =
                    devices.iter().map(|device| device.path.clone()).collect();
                if grabbed != requested {
                    self.clear();
                    return Err(protocol_error(format!(
                        "helper confirmed unexpected devices for seat {seat_id}: {grabbed:?}, \
                         expected {requested:?}"
                    )));
                }
                if uinput_devices.len() != devices.len() {
                    self.clear();
                    return Err(protocol_error(format!(
                        "helper confirmed {}/{} uinput devices for seat {seat_id}",
                        uinput_devices.len(),
                        devices.len()
                    )));
                }
                Ok(InputIsolationAssignment {
                    seat_id,
                    grabbed,
                    uinput_devices,
                })
            }
            other => {
                self.clear();
                Err(protocol_error(format!(
                    "expected a grab confirmation, got {other:?}"
                )))
            }
        }
    }

    fn release(&mut self, seat_id: &str) -> Result<InputIsolationRelease, MultiseatError> {
        if !self.is_alive() {
            self.clear();
            return Ok(InputIsolationRelease {
                seat_id: seat_id.to_owned(),
                released: Vec::new(),
            });
        }
        match self.round_trip(HelperRequest::Release {
            seat_id: seat_id.to_owned(),
        }) {
            Ok(HelperResponse::Released { seat_id, released }) => {
                Ok(InputIsolationRelease { seat_id, released })
            }
            Ok(other) => Err(protocol_error(format!(
                "expected a release confirmation, got {other:?}"
            ))),
            Err(error) => {
                // A helper that died mid-release no longer holds grabs; the
                // kernel released them when its fds closed. Report the empty
                // release rather than claiming a live assignment.
                if !self.is_alive() {
                    self.clear();
                    Ok(InputIsolationRelease {
                        seat_id: seat_id.to_owned(),
                        released: Vec::new(),
                    })
                } else {
                    Err(error)
                }
            }
        }
    }

    fn seat_status(&mut self, seat_id: &str) -> Result<Option<HelperSeatStatus>, MultiseatError> {
        if !self.is_alive() {
            self.clear();
            return Ok(None);
        }
        match self.round_trip(HelperRequest::Status) {
            Ok(HelperResponse::Status { seats }) => {
                Ok(seats.into_iter().find(|seat| seat.seat_id == seat_id))
            }
            Ok(other) => Err(protocol_error(format!(
                "expected a status response, got {other:?}"
            ))),
            Err(error) => {
                if !self.is_alive() {
                    self.clear();
                    Ok(None)
                } else {
                    Err(error)
                }
            }
        }
    }

    fn shutdown(&mut self) {
        if self.is_alive() {
            let _ = self.round_trip(HelperRequest::Shutdown);
        }
        let Some(mut child) = self.child.take() else {
            self.stdin = None;
            self.stdout = None;
            self.ready = None;
            return;
        };
        self.stdin = None;
        self.stdout = None;
        self.ready = None;

        #[cfg(unix)]
        {
            let _ = nix::sys::signal::kill(
                nix::unistd::Pid::from_raw(child.id() as i32),
                nix::sys::signal::Signal::SIGTERM,
            );
            let deadline = Instant::now() + HELPER_TERMINATE_GRACE;
            loop {
                match child.try_wait() {
                    Ok(Some(_)) => return,
                    Ok(None) => {}
                    Err(_) => break,
                }
                if Instant::now() >= deadline {
                    break;
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        }
        let _ = child.kill();
        let _ = child.wait();
    }
}

impl Drop for HelperClient {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn helper_error(error: HelperError) -> MultiseatError {
    let mut text = format!("helper error [{}]: {}", error.code.as_str(), error.message);
    if let Some(seat_id) = error.seat_id {
        text.push_str(&format!(" (seat {seat_id})"));
    }
    if let Some(device) = error.device {
        text.push_str(&format!(" (device {device})"));
    }
    MultiseatError::InputIsolation(text)
}

fn protocol_error(message: String) -> MultiseatError {
    MultiseatError::InputIsolation(format!("helper protocol error: {message}"))
}

/// Build the helper device list for one seat's input group, in deterministic
/// keyboard/mouse/gamepad/other order.
pub fn helper_devices(group: &InputGroup) -> Vec<HelperDevice> {
    let mut devices = Vec::with_capacity(group.device_count());
    for path in &group.keyboards {
        devices.push(HelperDevice::new(path, InputDeviceKind::Keyboard));
    }
    for path in &group.mice {
        devices.push(HelperDevice::new(path, InputDeviceKind::Mouse));
    }
    for path in &group.gamepads {
        devices.push(HelperDevice::new(path, InputDeviceKind::Gamepad));
    }
    for path in &group.other {
        devices.push(HelperDevice::new(path, InputDeviceKind::Other));
    }
    devices
}
