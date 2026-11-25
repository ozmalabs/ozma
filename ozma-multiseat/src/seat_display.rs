// SPDX-License-Identifier: AGPL-3.0-only
//! Per-seat display server backends.
//!
//! The default backend is [`SeatDisplayBackend::Shared`]: every seat draws on
//! the host's one X root, and ownership/routing only move outputs. The opt-in
//! [`SeatDisplayBackend::Xvfb`] backend starts a private `Xvfb` server per seat
//! and exports its `DISPLAY` (plus an `XAUTHORITY` cookie when `xauth` is
//! installed). [`SeatDisplayBackend::Xephyr`] is the nested-window variant: a
//! private X server whose root is a window inside the shared display.
//! [`SeatDisplayBackend::Weston`] starts a private headless Weston compositor
//! with its own runtime directory and `WAYLAND_DISPLAY` socket.
//!
//! Readiness is a real protocol/client handshake, never "the process exists".
//! X backends complete an X11 connection setup request against the server's
//! Unix socket; Weston is probed by connecting to its Wayland socket and, when
//! `wayland-info` is installed, by running that client with the seat's private
//! `XDG_RUNTIME_DIR`/`WAYLAND_DISPLAY`. Missing binaries, an exhausted
//! display-number range, startup timeouts, and crashes have typed errors.
//!
//! Nothing here touches the operator's desktop. `Xvfb` listens only on its
//! Unix socket (`-nolisten tcp`); the cookie is scoped to that display. When
//! `xauth` is unavailable the server falls back to `-ac` on the same
//! Unix-socket-only listener, and the handle reports `auth_disabled()` so the
//! supervisor can record an honest note. Weston's headless backend renders to
//! memory with the software renderer inside a throwaway runtime directory; it
//! does not open DRM or an existing Wayland/X display.

use std::collections::BTreeSet;
use std::fmt;
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::str::FromStr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::MultiseatError;

/// First display number considered by the default allocator.
pub const DEFAULT_DISPLAY_NUMBER_START: u32 = 90;
/// Last display number considered by the default allocator.
pub const DEFAULT_DISPLAY_NUMBER_END: u32 = 199;
/// Default nested display size.
pub const DEFAULT_SEAT_DISPLAY_WIDTH: u32 = 1280;
/// Default nested display height.
pub const DEFAULT_SEAT_DISPLAY_HEIGHT: u32 = 800;
/// Default budget for a spawned X server to answer the X11 handshake.
pub const DEFAULT_DISPLAY_READY_TIMEOUT: Duration = Duration::from_secs(10);
/// Default interval between X11 handshake probes.
pub const DEFAULT_DISPLAY_READY_POLL: Duration = Duration::from_millis(100);
/// How long an X server gets to exit after SIGTERM before it is killed.
pub const DISPLAY_TERMINATE_GRACE: Duration = Duration::from_secs(5);

/// Which X server strategy a seat uses.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatDisplayBackend {
    /// Current behavior: one shared X root for every seat.
    #[default]
    Shared,
    /// One private `Xvfb` server per seat.
    Xvfb,
    /// One private `Xephyr` server nested inside the shared display.
    Xephyr,
    /// One private real `Xorg` server per seat with a caller-supplied config.
    ///
    /// This is the backend that can consume helper-created uinput devices via
    /// the libinput input driver. It needs a config whose `InputDevice`
    /// identifiers are the helper's `ozma-seat-...` names, and it must run with
    /// whatever privilege the host's `Xorg` wrapper requires (often root or the
    /// console user). Without a config it is a typed unsupported error rather
    /// than an implicit GPU probe.
    Xorg,
    /// One private headless Weston compositor per seat.
    ///
    /// The compositor uses the `headless` backend with the software renderer,
    /// a throwaway `XDG_RUNTIME_DIR`, a generated `weston.ini`, and a unique
    /// `WAYLAND_DISPLAY` socket. It renders to memory: no DRM device, GPU
    /// mode-setting, physical output, or existing X/Wayland session is used.
    /// The headless backend has no libinput seat, so physical input isolation
    /// is never claimed for this backend. Weston does not implement the wlroots
    /// virtual-pointer protocol, so synthetic pointer injection is unavailable.
    Weston,
    /// One private headless `sway` (wlroots) compositor per seat.
    ///
    /// Like [`Self::Weston`] it uses the headless backend, the pixman software
    /// renderer, a throwaway runtime directory, and a private socket. It
    /// additionally exposes `zwp_virtual_keyboard_manager_v1` and
    /// `zwlr_virtual_pointer_manager_v1`, which the opt-in
    /// `--seat-input wayland` injector binds. No DRM, GPU, physical output, or
    /// operator session is touched.
    Sway,
}

impl SeatDisplayBackend {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Shared => "shared",
            Self::Xvfb => "xvfb",
            Self::Xephyr => "xephyr",
            Self::Xorg => "xorg",
            Self::Weston => "weston",
            Self::Sway => "sway",
        }
    }

    /// Whether this backend starts a private display/compositor.
    pub fn is_nested(self) -> bool {
        !matches!(self, Self::Shared)
    }

    /// Whether this backend exposes a Wayland socket instead of an X display.
    pub fn is_wayland(self) -> bool {
        matches!(self, Self::Weston | Self::Sway)
    }
}

impl fmt::Display for SeatDisplayBackend {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SeatDisplayBackend {
    type Err = MultiseatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "shared" => Ok(Self::Shared),
            "xvfb" => Ok(Self::Xvfb),
            "xephyr" => Ok(Self::Xephyr),
            "xorg" => Ok(Self::Xorg),
            "weston" | "wayland" => Ok(Self::Weston),
            "sway" | "wlroots" => Ok(Self::Sway),
            other => Err(MultiseatError::SeatDisplay(format!(
                "unknown seat display backend {other:?}; expected shared, xvfb, xephyr, xorg, \
                 weston, or sway"
            ))),
        }
    }
}

/// Geometry of the private X screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatDisplaySize {
    /// Screen width in pixels.
    pub width: u32,
    /// Screen height in pixels.
    pub height: u32,
}

impl Default for SeatDisplaySize {
    fn default() -> Self {
        Self {
            width: DEFAULT_SEAT_DISPLAY_WIDTH,
            height: DEFAULT_SEAT_DISPLAY_HEIGHT,
        }
    }
}

impl SeatDisplaySize {
    /// Build a validated size.
    pub fn new(width: u32, height: u32) -> Result<Self, MultiseatError> {
        if width == 0 || height == 0 {
            return Err(MultiseatError::SeatDisplay(format!(
                "seat display size must be greater than zero, got {width}x{height}"
            )));
        }
        Ok(Self { width, height })
    }

    /// `WxH`.
    pub fn as_str(self) -> String {
        format!("{}x{}", self.width, self.height)
    }

    /// Depth-24 X screen geometry, for example `1280x800x24`.
    pub fn x_screen(self) -> String {
        format!("{}x{}x24", self.width, self.height)
    }
}

impl fmt::Display for SeatDisplaySize {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}x{}", self.width, self.height)
    }
}

impl FromStr for SeatDisplaySize {
    type Err = MultiseatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let trimmed = value.trim();
        let (width, height) = trimmed.split_once('x').ok_or_else(|| {
            MultiseatError::SeatDisplay(format!(
                "invalid seat display size {trimmed:?}; expected WxH, for example 1280x800"
            ))
        })?;
        let width = width.parse::<u32>().map_err(|_| {
            MultiseatError::SeatDisplay(format!("invalid seat display width {width:?}"))
        })?;
        let height = height.parse::<u32>().map_err(|_| {
            MultiseatError::SeatDisplay(format!("invalid seat display height {height:?}"))
        })?;
        Self::new(width, height)
    }
}

/// Kind of a typed display status note.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatDisplayNoteKind {
    /// xrandr ownership was requested but has no meaning on a private X root.
    OwnershipUnsupported,
    /// xinput mapping was requested but has no physical pointer inside the
    /// private X server.
    XinputRoutingUnsupported,
    /// The private display could not be restricted to the seat's uinput
    /// devices. Full display input isolation is not claimed.
    InputTargetingFailed,
    /// Input targeting was requested for a private display but no targeter or
    /// targeting policy was configured.
    InputTargetingUnavailable,
    /// The compositor does not expose the virtual keyboard/pointer protocols,
    /// or no injector was configured. Synthetic input is never claimed.
    InputInjectionUnavailable,
    /// `xauth` was unavailable, so the server used `-ac` on its Unix socket.
    AuthDisabled,
}

impl SeatDisplayNoteKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::OwnershipUnsupported => "ownership_unsupported",
            Self::XinputRoutingUnsupported => "xinput_routing_unsupported",
            Self::InputTargetingFailed => "input_targeting_failed",
            Self::InputTargetingUnavailable => "input_targeting_unavailable",
            Self::InputInjectionUnavailable => "input_injection_unavailable",
            Self::AuthDisabled => "auth_disabled",
        }
    }
}

/// One honest limitation note attached to a private display.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatDisplayNote {
    /// Note kind.
    pub kind: SeatDisplayNoteKind,
    /// Operator-facing detail.
    pub message: String,
}

impl SeatDisplayNote {
    /// Build a note.
    pub fn new(kind: SeatDisplayNoteKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

/// Typed display failure reported in status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatDisplayError {
    /// Stable error kind (`missing_binary`, `number_exhausted`,
    /// `startup_timeout`, `crashed`, `unsupported`, `error`).
    pub kind: String,
    /// Full typed error message.
    pub message: String,
}

impl SeatDisplayError {
    /// Build from a typed error.
    pub fn from_error(error: &MultiseatError) -> Self {
        Self {
            kind: display_error_kind(error).to_owned(),
            message: error.to_string(),
        }
    }
}

/// Per-seat private display state, flattened into seat status.
///
/// The whole flat surface is omitted while the backend is `shared`, so status
/// JSON for the default backend stays byte-for-byte compatible with earlier
/// releases.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatDisplayStatus {
    /// Backend in use. Omitted for the default `shared` backend.
    #[serde(
        rename = "display_backend",
        default,
        skip_serializing_if = "is_shared_backend"
    )]
    pub display_backend: SeatDisplayBackend,
    /// Private display name. For X backends this is an X display such as
    /// `:90`; for the Weston backend it is the `WAYLAND_DISPLAY` socket name.
    #[serde(rename = "display", default, skip_serializing_if = "Option::is_none")]
    pub display: Option<String>,
    /// Private display server/compositor pid.
    #[serde(
        rename = "display_pid",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_pid: Option<u32>,
    /// Whether the protocol handshake succeeded and the server is still alive.
    #[serde(rename = "display_ready", default, skip_serializing_if = "is_false")]
    pub display_ready: bool,
    /// Typed limitations for this display.
    #[serde(
        rename = "display_notes",
        default,
        skip_serializing_if = "Vec::is_empty"
    )]
    pub display_notes: Vec<SeatDisplayNote>,
    /// Most recent typed display failure.
    #[serde(
        rename = "display_error",
        default,
        skip_serializing_if = "Option::is_none"
    )]
    pub display_error: Option<SeatDisplayError>,
}

fn is_shared_backend(backend: &SeatDisplayBackend) -> bool {
    *backend == SeatDisplayBackend::Shared
}

fn is_false(value: &bool) -> bool {
    !*value
}

impl SeatDisplayStatus {
    /// A shared-display no-op status.
    pub fn shared() -> Self {
        Self::default()
    }

    /// Configure the requested backend before a private server exists.
    pub fn for_backend(backend: SeatDisplayBackend) -> Self {
        Self {
            display_backend: backend,
            ..Self::default()
        }
    }

    /// Whether the private server has a live pid and a successful handshake.
    pub fn is_ready(&self) -> bool {
        self.display_ready && self.display.is_some()
    }

    /// Add one typed note, deduplicating identical entries.
    pub fn push_note(&mut self, note: SeatDisplayNote) {
        if !self
            .display_notes
            .iter()
            .any(|existing| existing.kind == note.kind && existing.message == note.message)
        {
            self.display_notes.push(note);
        }
    }
}

/// Supervisor configuration for per-seat display servers.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatDisplayOptions {
    /// Backend selection. `Shared` is the default and starts nothing.
    pub backend: SeatDisplayBackend,
    /// Private X screen size.
    pub size: SeatDisplaySize,
    /// First display number considered.
    pub number_start: u32,
    /// Last display number considered.
    pub number_end: u32,
    /// Budget for the spawned server to answer the X11 handshake.
    pub ready_timeout: Duration,
    /// Interval between handshake probes.
    pub ready_poll: Duration,
    /// Shared display used as the parent for `Xephyr`. `None` uses the seat's
    /// planned X11 capture display.
    pub parent_display: Option<String>,
    /// Xorg config file for the `Xorg` backend. The config must define the
    /// dummy/video device and the libinput `InputDevice` sections whose
    /// identifiers match the helper's uinput names. `None` makes `Xorg` a
    /// typed unsupported error instead of probing the host GPU.
    pub xorg_config: Option<PathBuf>,
}

impl Default for SeatDisplayOptions {
    fn default() -> Self {
        Self {
            backend: SeatDisplayBackend::Shared,
            size: SeatDisplaySize::default(),
            number_start: DEFAULT_DISPLAY_NUMBER_START,
            number_end: DEFAULT_DISPLAY_NUMBER_END,
            ready_timeout: DEFAULT_DISPLAY_READY_TIMEOUT,
            ready_poll: DEFAULT_DISPLAY_READY_POLL,
            parent_display: None,
            xorg_config: None,
        }
    }
}

impl SeatDisplayOptions {
    /// Options for one backend with default geometry and number range.
    pub fn for_backend(backend: SeatDisplayBackend) -> Self {
        Self {
            backend,
            ..Self::default()
        }
    }
}

/// One request to start a private X server for a seat.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatDisplayRequest {
    /// Seat that will own the display.
    pub seat_id: String,
    /// Backend to start.
    pub backend: SeatDisplayBackend,
    /// Private X screen size.
    pub size: SeatDisplaySize,
    /// Shared display used as the outer window for `Xephyr`.
    pub parent_display: Option<String>,
}

/// Exit information for a private X server.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DisplayExit {
    /// Whether the server exited successfully.
    pub success: bool,
    /// Exit code, or `None` when it was terminated by a signal.
    pub code: Option<i32>,
}

impl fmt::Display for DisplayExit {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self.code {
            Some(code) => write!(formatter, "exit code {code}"),
            None => formatter.write_str(if self.success { "success" } else { "signal" }),
        }
    }
}

/// A started private display server or compositor.
///
/// `is_ready` performs a real protocol handshake (X11 connection setup or a
/// Wayland client connection). `terminate` is idempotent, waits for the server
/// to exit, and releases the display number and any private runtime files.
pub trait SeatDisplayHandle: Send {
    /// Backend that was started.
    fn backend(&self) -> SeatDisplayBackend;
    /// Display name: an X display such as `:90`, or a `WAYLAND_DISPLAY` name.
    fn display(&self) -> &str;
    /// Numeric display/socket number.
    fn display_number(&self) -> u32;
    /// Server process id.
    fn pid(&self) -> Option<u32>;
    /// Cookie file exported to the seat's applications, when auth is enabled.
    fn xauthority(&self) -> Option<&Path>;
    /// Whether the server was started with `-ac` because `xauth` was absent or
    /// failed. The listener is still Unix-socket-only.
    fn auth_disabled(&self) -> bool;
    /// Wayland environment for a compositor backend, otherwise `None`.
    fn wayland(&self) -> Option<&SeatWaylandEnv> {
        None
    }
    /// Perform one readiness probe.
    fn is_ready(&self) -> Result<bool, MultiseatError>;
    /// Non-blocking exit check.
    fn try_wait(&mut self) -> std::io::Result<Option<DisplayExit>>;
    /// Stop the server, wait for it, and release its display number/files.
    fn terminate(&mut self) -> Result<(), MultiseatError>;
}

/// Spawn/stop seam for private X servers. Tests inject a fake.
pub trait SeatDisplayLauncher: Send + Sync {
    /// Spawn and allocate a display for one seat.
    fn spawn(
        &self,
        request: &SeatDisplayRequest,
    ) -> Result<Box<dyn SeatDisplayHandle>, MultiseatError>;
}

/// Programs used by [`ProcessSeatDisplayLauncher`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatDisplayPrograms {
    /// `Xvfb` binary name or path.
    pub xvfb: PathBuf,
    /// `Xephyr` binary name or path.
    pub xephyr: PathBuf,
    /// `Xorg` binary name or path. The distro wrapper may restrict non-console
    /// users, so a deployment can point this at a privileged wrapper.
    pub xorg: PathBuf,
    /// `weston` binary name or path for the headless Wayland backend.
    pub weston: PathBuf,
    /// `sway` binary name or path for the headless wlroots backend.
    pub sway: PathBuf,
    /// `xauth` binary name or path.
    pub xauth: PathBuf,
}

impl Default for SeatDisplayPrograms {
    fn default() -> Self {
        Self {
            xvfb: PathBuf::from("Xvfb"),
            xephyr: PathBuf::from("Xephyr"),
            xorg: PathBuf::from("Xorg"),
            weston: PathBuf::from("weston"),
            sway: PathBuf::from("sway"),
            xauth: PathBuf::from("xauth"),
        }
    }
}

/// Allocates and reuses X display numbers.
///
/// A number is never handed out twice while a display is alive. The production
/// allocator also skips numbers whose `/tmp/.X11-unix/XN` socket or
/// `/tmp/.XN-lock` file already exists, so it cannot steal an unrelated
/// server. [`Self::without_host_check`] is for deterministic tests.
#[derive(Clone, Debug)]
pub struct DisplayNumberAllocator {
    start: u32,
    end: u32,
    in_use: BTreeSet<u32>,
    busy: BTreeSet<u32>,
    check_host: bool,
}

impl DisplayNumberAllocator {
    /// Allocate from `start..=end`, checking the host's X sockets.
    pub fn new(start: u32, end: u32) -> Self {
        Self {
            start,
            end: end.max(start),
            in_use: BTreeSet::new(),
            busy: BTreeSet::new(),
            check_host: true,
        }
    }

    /// Allocate from `start..=end` without inspecting `/tmp`. Tests use this so
    /// host sockets cannot make an assertion flaky.
    pub fn without_host_check(start: u32, end: u32) -> Self {
        Self {
            check_host: false,
            ..Self::new(start, end)
        }
    }

    /// Mark numbers that must be treated as already taken.
    pub fn with_busy<I>(mut self, numbers: I) -> Self
    where
        I: IntoIterator<Item = u32>,
    {
        self.busy.extend(numbers);
        self
    }

    /// Configured range.
    pub fn range(&self) -> (u32, u32) {
        (self.start, self.end)
    }

    /// Numbers currently allocated by this allocator.
    pub fn in_use(&self) -> Vec<u32> {
        self.in_use.iter().copied().collect()
    }

    /// Allocate the first free number. Numbers are reused after release.
    pub fn allocate(&mut self) -> Result<u32, MultiseatError> {
        for number in self.start..=self.end {
            if self.in_use.contains(&number)
                || self.busy.contains(&number)
                || (self.check_host && host_display_in_use(number))
            {
                continue;
            }
            self.in_use.insert(number);
            return Ok(number);
        }
        Err(MultiseatError::SeatDisplayNumberExhausted {
            start: self.start,
            end: self.end,
        })
    }

    /// Release a number for reuse. Unknown numbers are ignored.
    pub fn release(&mut self, number: u32) {
        self.in_use.remove(&number);
    }
}

fn host_display_in_use(number: u32) -> bool {
    let socket = PathBuf::from(format!("/tmp/.X11-unix/X{number}"));
    let lock = PathBuf::from(format!("/tmp/.X{number}-lock"));
    socket.exists() || lock.exists()
}

/// X11 display string for a number.
pub fn display_name(number: u32) -> String {
    format!(":{number}")
}

/// Unique `WAYLAND_DISPLAY` name for a seat number.
///
/// The name is scoped by the seat's private `XDG_RUNTIME_DIR`, so it cannot
/// collide with the operator's compositor even if it uses the same socket name.
pub fn wayland_display_name(number: u32) -> String {
    format!("ozma-weston-{number}")
}

/// Unique `WAYLAND_DISPLAY` name for a sway seat.
///
/// Sway creates its own `wayland-N` socket inside the private runtime directory
/// and ignores an inherited name. The launcher creates an `ozma-sway-N` symlink
/// to that socket so the seat still has a stable, private display name.
pub fn sway_display_name(number: u32) -> String {
    format!("ozma-sway-{number}")
}

/// Environment that points a seat child at a private Wayland compositor.
///
/// The compositor socket is `XDG_RUNTIME_DIR/WAYLAND_DISPLAY`. The runtime
/// directory is created and owned by the display handle, so it can only reach
/// this seat's compositor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatWaylandEnv {
    /// Value exported as `WAYLAND_DISPLAY`.
    pub wayland_display: String,
    /// Value exported as `XDG_RUNTIME_DIR`.
    pub xdg_runtime_dir: PathBuf,
    /// Absolute socket path (`XDG_RUNTIME_DIR/WAYLAND_DISPLAY`).
    pub socket: PathBuf,
}

impl SeatWaylandEnv {
    /// Build the environment for a display name and private runtime directory.
    pub fn new(wayland_display: impl Into<String>, xdg_runtime_dir: impl Into<PathBuf>) -> Self {
        let wayland_display = wayland_display.into();
        let xdg_runtime_dir = xdg_runtime_dir.into();
        let socket = xdg_runtime_dir.join(&wayland_display);
        Self {
            wayland_display,
            xdg_runtime_dir,
            socket,
        }
    }
}

/// Parse the numeric part of a local X display string.
pub fn parse_display_number(display: &str) -> Option<u32> {
    display
        .trim()
        .strip_prefix(':')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// Production launcher for `Xvfb` and `Xephyr`.
#[derive(Clone, Debug)]
pub struct ProcessSeatDisplayLauncher {
    options: SeatDisplayOptions,
    programs: SeatDisplayPrograms,
    allocator: Arc<Mutex<DisplayNumberAllocator>>,
}

impl ProcessSeatDisplayLauncher {
    /// Build a launcher with the default programs and configured number range.
    pub fn new(options: SeatDisplayOptions) -> Self {
        let allocator = DisplayNumberAllocator::new(options.number_start, options.number_end);
        Self {
            options,
            programs: SeatDisplayPrograms::default(),
            allocator: Arc::new(Mutex::new(allocator)),
        }
    }

    /// Override the external programs. Missing-binary tests point at a
    /// nonexistent path; production keeps the defaults.
    pub fn with_programs(mut self, programs: SeatDisplayPrograms) -> Self {
        self.programs = programs;
        self
    }

    /// Configured options.
    pub fn options(&self) -> &SeatDisplayOptions {
        &self.options
    }

    /// Allocated display numbers, for diagnostics.
    pub fn in_use(&self) -> Vec<u32> {
        self.allocator
            .lock()
            .expect("display allocator lock")
            .in_use()
    }

    fn spawn_weston(
        &self,
        request: &SeatDisplayRequest,
    ) -> Result<Box<dyn SeatDisplayHandle>, MultiseatError> {
        let program = match resolve_program(&self.programs.weston) {
            Some(program) => program,
            None => {
                return Err(MultiseatError::SeatDisplayTool {
                    backend: "weston".to_owned(),
                    program: self.programs.weston.display().to_string(),
                    reason: "not found on PATH or not executable".to_owned(),
                })
            }
        };

        let number = self
            .allocator
            .lock()
            .expect("display allocator lock")
            .allocate()?;
        let display = wayland_display_name(number);

        let sequence = AUTH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "ozma-seat-weston-{}-{sequence}",
            std::process::id()
        ));
        let runtime = dir.join("runtime");
        let setup = fs::create_dir_all(&runtime)
            .map_err(|error| {
                MultiseatError::SeatDisplay(format!(
                    "creating Weston runtime directory {}: {error}",
                    runtime.display()
                ))
            })
            .and_then(|()| {
                restrict_directory(&dir);
                restrict_directory(&runtime);
                let config = dir.join("weston.ini");
                fs::write(&config, weston_config(request.size)).map_err(|error| {
                    MultiseatError::SeatDisplay(format!(
                        "writing Weston config {}: {error}",
                        config.display()
                    ))
                })?;
                Ok(config)
            });
        let config = match setup {
            Ok(config) => config,
            Err(error) => {
                release_number(&self.allocator, number);
                let _ = fs::remove_dir_all(&dir);
                return Err(error);
            }
        };

        let args = weston_server_args(&display, request.size, &config);
        let mut command = Command::new(&program);
        command
            .args(&args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                release_number(&self.allocator, number);
                let _ = fs::remove_dir_all(&dir);
                return Err(map_spawn_error(
                    SeatDisplayBackend::Weston,
                    &program,
                    &display,
                    error,
                ));
            }
        };

        let wayland = SeatWaylandEnv::new(display, runtime);
        Ok(Box::new(WaylandDisplayHandle {
            child,
            backend: SeatDisplayBackend::Weston,
            number,
            wayland,
            dir,
            allocator: Arc::clone(&self.allocator),
            terminated: false,
            released: false,
        }))
    }

    #[cfg(unix)]
    fn spawn_sway(
        &self,
        request: &SeatDisplayRequest,
    ) -> Result<Box<dyn SeatDisplayHandle>, MultiseatError> {
        let program = match resolve_program(&self.programs.sway) {
            Some(program) => program,
            None => {
                return Err(MultiseatError::SeatDisplayTool {
                    backend: "sway".to_owned(),
                    program: self.programs.sway.display().to_string(),
                    reason: "not found on PATH or not executable".to_owned(),
                })
            }
        };

        let number = self
            .allocator
            .lock()
            .expect("display allocator lock")
            .allocate()?;
        let display = sway_display_name(number);

        let sequence = AUTH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("ozma-seat-sway-{}-{sequence}", std::process::id()));
        let runtime = dir.join("runtime");
        let setup = fs::create_dir_all(&runtime)
            .map_err(|error| {
                MultiseatError::SeatDisplay(format!(
                    "creating sway runtime directory {}: {error}",
                    runtime.display()
                ))
            })
            .and_then(|()| {
                restrict_directory(&dir);
                restrict_directory(&runtime);
                let config = dir.join("sway.conf");
                fs::write(&config, sway_config(request.size)).map_err(|error| {
                    MultiseatError::SeatDisplay(format!(
                        "writing sway config {}: {error}",
                        config.display()
                    ))
                })?;
                Ok(config)
            });
        let config = match setup {
            Ok(config) => config,
            Err(error) => {
                release_number(&self.allocator, number);
                let _ = fs::remove_dir_all(&dir);
                return Err(error);
            }
        };

        let args = sway_server_args(&config);
        let mut command = Command::new(&program);
        command
            .args(&args)
            .env("XDG_RUNTIME_DIR", &runtime)
            .env("WLR_BACKENDS", "headless")
            .env("WLR_RENDERER", "pixman")
            .env("WLR_LIBINPUT_NO_DEVICES", "1")
            .env("WLR_NO_HARDWARE_CURSORS", "1")
            .env("WLR_HEADLESS_OUTPUTS", "1")
            .env_remove("WAYLAND_DISPLAY")
            .env_remove("DISPLAY")
            .env_remove("XAUTHORITY")
            .env_remove("SWAYSOCK")
            .env_remove("I3SOCK")
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());

        let mut child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                release_number(&self.allocator, number);
                let _ = fs::remove_dir_all(&dir);
                return Err(map_spawn_error(
                    SeatDisplayBackend::Sway,
                    &program,
                    &display,
                    error,
                ));
            }
        };

        // Sway creates its own `wayland-N` socket; it ignores an inherited
        // WAYLAND_DISPLAY. Wait for it, then expose the stable per-seat name as
        // a symlink inside the same private runtime directory.
        let socket = match wait_for_sway_socket(&runtime, &display, self.options.ready_timeout) {
            Ok(socket) => socket,
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                release_number(&self.allocator, number);
                let _ = fs::remove_dir_all(&dir);
                return Err(error);
            }
        };
        let symlink = runtime.join(&display);
        let link_name = socket
            .file_name()
            .map(std::ffi::OsString::from)
            .unwrap_or_else(|| std::ffi::OsString::from("wayland-1"));
        if let Err(error) = std::os::unix::fs::symlink(&link_name, &symlink) {
            let _ = child.kill();
            let _ = child.wait();
            release_number(&self.allocator, number);
            let _ = fs::remove_dir_all(&dir);
            return Err(MultiseatError::SeatDisplay(format!(
                "creating private sway socket link {} -> {}: {error}",
                symlink.display(),
                link_name.to_string_lossy()
            )));
        }

        let wayland = SeatWaylandEnv::new(display, runtime);
        Ok(Box::new(WaylandDisplayHandle {
            child,
            backend: SeatDisplayBackend::Sway,
            number,
            wayland,
            dir,
            allocator: Arc::clone(&self.allocator),
            terminated: false,
            released: false,
        }))
    }

    #[cfg(not(unix))]
    fn spawn_sway(
        &self,
        _request: &SeatDisplayRequest,
    ) -> Result<Box<dyn SeatDisplayHandle>, MultiseatError> {
        Err(MultiseatError::Unsupported(
            "the sway backend requires a Unix host".to_owned(),
        ))
    }
}

impl SeatDisplayLauncher for ProcessSeatDisplayLauncher {
    fn spawn(
        &self,
        request: &SeatDisplayRequest,
    ) -> Result<Box<dyn SeatDisplayHandle>, MultiseatError> {
        if request.backend == SeatDisplayBackend::Weston {
            return self.spawn_weston(request);
        }
        if request.backend == SeatDisplayBackend::Sway {
            return self.spawn_sway(request);
        }
        let (program, backend_name) = match request.backend {
            SeatDisplayBackend::Shared => {
                return Err(MultiseatError::Unsupported(
                    "the shared display backend does not spawn an X server".to_owned(),
                ))
            }
            SeatDisplayBackend::Xvfb => (self.programs.xvfb.clone(), "xvfb"),
            SeatDisplayBackend::Xephyr => (self.programs.xephyr.clone(), "xephyr"),
            SeatDisplayBackend::Xorg => (self.programs.xorg.clone(), "xorg"),
            SeatDisplayBackend::Weston | SeatDisplayBackend::Sway => {
                unreachable!("Wayland handled above")
            }
        };

        let program = match resolve_program(&program) {
            Some(program) => program,
            None => {
                let error = MultiseatError::SeatDisplayTool {
                    backend: backend_name.to_owned(),
                    program: program.display().to_string(),
                    reason: "not found on PATH or not executable".to_owned(),
                };
                // Xephyr is the optional nested backend: report it as typed
                // unsupported rather than pretending a nested window exists.
                return if request.backend == SeatDisplayBackend::Xephyr {
                    Err(MultiseatError::Unsupported(format!(
                        "seat display backend xephyr requires Xephyr, which is unavailable: {error}"
                    )))
                } else {
                    Err(error)
                };
            }
        };

        if request.backend == SeatDisplayBackend::Xorg && self.options.xorg_config.is_none() {
            return Err(MultiseatError::Unsupported(
                "seat display backend xorg needs an explicit config file (`xorg_config`); \
                 refusing to probe the host GPU with the default Xorg configuration"
                    .to_owned(),
            ));
        }

        let parent = match request.backend {
            SeatDisplayBackend::Xephyr => {
                let parent = request
                    .parent_display
                    .as_deref()
                    .map(str::trim)
                    .filter(|display| !display.is_empty())
                    .map(str::to_owned)
                    .or_else(|| self.options.parent_display.clone())
                    .ok_or_else(|| {
                        MultiseatError::Unsupported(
                            "seat display backend xephyr needs a parent X display for its nested \
                             window"
                                .to_owned(),
                        )
                    })?;
                Some(parent)
            }
            _ => None,
        };

        let number = self
            .allocator
            .lock()
            .expect("display allocator lock")
            .allocate()?;
        let display = display_name(number);

        let mut auth = match create_xauthority(&self.programs.xauth, &display) {
            Ok(auth) => auth,
            Err(_) => None,
        };
        let auth_disabled = auth.is_none();

        let args = display_server_args(
            request.backend,
            &display,
            request.size,
            auth.as_ref().map(|auth| auth.file.as_path()),
            &request.seat_id,
            self.options.xorg_config.as_deref(),
        );

        let mut command = Command::new(&program);
        command
            .args(&args)
            .stdin(Stdio::null())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit());
        if let Some(parent) = parent.as_deref() {
            command.env("DISPLAY", parent);
        }

        let child = match command.spawn() {
            Ok(child) => child,
            Err(error) => {
                release_number(&self.allocator, number);
                if let Some(mut auth) = auth.take() {
                    auth.remove();
                }
                return Err(map_spawn_error(request.backend, &program, &display, error));
            }
        };

        let cookie = auth.as_ref().map(|auth| auth.cookie.clone());
        let xauthority = auth.as_ref().map(|auth| auth.file.clone());
        Ok(Box::new(ProcessDisplayHandle {
            child,
            backend: request.backend,
            display,
            number,
            xauthority,
            cookie,
            auth,
            auth_disabled,
            allocator: Arc::clone(&self.allocator),
            terminated: false,
            released: false,
        }))
    }
}

fn map_spawn_error(
    backend: SeatDisplayBackend,
    program: &Path,
    display: &str,
    error: std::io::Error,
) -> MultiseatError {
    match error.kind() {
        std::io::ErrorKind::NotFound => MultiseatError::SeatDisplayTool {
            backend: backend.as_str().to_owned(),
            program: program.display().to_string(),
            reason: "not found on PATH or not executable".to_owned(),
        },
        _ => MultiseatError::SeatDisplay(format!(
            "failed to spawn {} for display {display}: {error}",
            program.display()
        )),
    }
}

struct ProcessDisplayHandle {
    child: Child,
    backend: SeatDisplayBackend,
    display: String,
    number: u32,
    xauthority: Option<PathBuf>,
    cookie: Option<Vec<u8>>,
    auth: Option<Xauthority>,
    auth_disabled: bool,
    allocator: Arc<Mutex<DisplayNumberAllocator>>,
    terminated: bool,
    released: bool,
}

impl SeatDisplayHandle for ProcessDisplayHandle {
    fn backend(&self) -> SeatDisplayBackend {
        self.backend
    }

    fn display(&self) -> &str {
        &self.display
    }

    fn display_number(&self) -> u32 {
        self.number
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn xauthority(&self) -> Option<&Path> {
        self.xauthority.as_deref()
    }

    fn auth_disabled(&self) -> bool {
        self.auth_disabled
    }

    fn is_ready(&self) -> Result<bool, MultiseatError> {
        probe_x11_display(
            &self.display,
            self.cookie.as_deref(),
            Duration::from_millis(500),
        )
    }

    fn try_wait(&mut self) -> std::io::Result<Option<DisplayExit>> {
        self.child.try_wait().map(|status| {
            status.map(|status| DisplayExit {
                success: status.success(),
                code: status.code(),
            })
        })
    }

    fn terminate(&mut self) -> Result<(), MultiseatError> {
        if !self.terminated {
            self.terminated = true;
            if self
                .child
                .try_wait()
                .map_err(|error| MultiseatError::SeatDisplay(format!("polling X server: {error}")))?
                .is_none()
            {
                #[cfg(unix)]
                {
                    let pid = self.child.id();
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                    let deadline = Instant::now() + DISPLAY_TERMINATE_GRACE;
                    loop {
                        match self.child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => {}
                            Err(error) => {
                                return Err(MultiseatError::SeatDisplay(format!(
                                    "polling X server for shutdown: {error}"
                                )))
                            }
                        }
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                if self
                    .child
                    .try_wait()
                    .map_err(|error| {
                        MultiseatError::SeatDisplay(format!("polling X server: {error}"))
                    })?
                    .is_none()
                {
                    self.child.kill().map_err(|error| {
                        MultiseatError::SeatDisplay(format!("killing X server: {error}"))
                    })?;
                    self.child.wait().map_err(|error| {
                        MultiseatError::SeatDisplay(format!("reaping X server: {error}"))
                    })?;
                }
            }
        }
        self.release_resources();
        Ok(())
    }
}

impl ProcessDisplayHandle {
    fn release_resources(&mut self) {
        if !self.released {
            self.released = true;
            release_number(&self.allocator, self.number);
        }
        if let Some(mut auth) = self.auth.take() {
            auth.remove();
        }
    }
}

impl Drop for ProcessDisplayHandle {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

fn release_number(allocator: &Arc<Mutex<DisplayNumberAllocator>>, number: u32) {
    allocator
        .lock()
        .expect("display allocator lock")
        .release(number);
}

struct WaylandDisplayHandle {
    child: Child,
    backend: SeatDisplayBackend,
    number: u32,
    wayland: SeatWaylandEnv,
    dir: PathBuf,
    allocator: Arc<Mutex<DisplayNumberAllocator>>,
    terminated: bool,
    released: bool,
}

impl SeatDisplayHandle for WaylandDisplayHandle {
    fn backend(&self) -> SeatDisplayBackend {
        self.backend
    }

    fn display(&self) -> &str {
        &self.wayland.wayland_display
    }

    fn display_number(&self) -> u32 {
        self.number
    }

    fn pid(&self) -> Option<u32> {
        Some(self.child.id())
    }

    fn xauthority(&self) -> Option<&Path> {
        None
    }

    fn auth_disabled(&self) -> bool {
        false
    }

    fn wayland(&self) -> Option<&SeatWaylandEnv> {
        Some(&self.wayland)
    }

    fn is_ready(&self) -> Result<bool, MultiseatError> {
        probe_wayland_display(&self.wayland)
    }

    fn try_wait(&mut self) -> std::io::Result<Option<DisplayExit>> {
        self.child.try_wait().map(|status| {
            status.map(|status| DisplayExit {
                success: status.success(),
                code: status.code(),
            })
        })
    }

    fn terminate(&mut self) -> Result<(), MultiseatError> {
        if !self.terminated {
            self.terminated = true;
            if self
                .child
                .try_wait()
                .map_err(|error| {
                    MultiseatError::SeatDisplay(format!(
                        "polling {} compositor: {error}",
                        self.backend
                    ))
                })?
                .is_none()
            {
                #[cfg(unix)]
                {
                    let pid = self.child.id();
                    let _ = nix::sys::signal::kill(
                        nix::unistd::Pid::from_raw(pid as i32),
                        nix::sys::signal::Signal::SIGTERM,
                    );
                    let deadline = Instant::now() + DISPLAY_TERMINATE_GRACE;
                    loop {
                        match self.child.try_wait() {
                            Ok(Some(_)) => break,
                            Ok(None) => {}
                            Err(error) => {
                                return Err(MultiseatError::SeatDisplay(format!(
                                    "polling {} compositor for shutdown: {error}",
                                    self.backend
                                )))
                            }
                        }
                        if Instant::now() >= deadline {
                            break;
                        }
                        std::thread::sleep(Duration::from_millis(25));
                    }
                }
                if self
                    .child
                    .try_wait()
                    .map_err(|error| {
                        MultiseatError::SeatDisplay(format!(
                            "polling {} compositor: {error}",
                            self.backend
                        ))
                    })?
                    .is_none()
                {
                    self.child.kill().map_err(|error| {
                        MultiseatError::SeatDisplay(format!(
                            "killing {} compositor: {error}",
                            self.backend
                        ))
                    })?;
                    self.child.wait().map_err(|error| {
                        MultiseatError::SeatDisplay(format!(
                            "reaping {} compositor: {error}",
                            self.backend
                        ))
                    })?;
                }
            }
        }
        self.release_resources();
        Ok(())
    }
}

impl WaylandDisplayHandle {
    fn release_resources(&mut self) {
        if !self.released {
            self.released = true;
            release_number(&self.allocator, self.number);
        }
        // The runtime directory contains the compositor socket, its lock file,
        // and the generated config. Removing it is the last cleanup step.
        let _ = fs::remove_dir_all(&self.dir);
    }
}

impl Drop for WaylandDisplayHandle {
    fn drop(&mut self) {
        let _ = self.terminate();
    }
}

/// Generate the per-seat `weston.ini`.
///
/// The output geometry is passed on the command line; the config disables
/// idle blanking and the desktop-shell panel so the compositor is a plain
/// headless surface rather than an interactive desktop.
pub fn weston_config(size: SeatDisplaySize) -> String {
    format!(
        "# Generated by ozma-multiseat for a throwaway {}x{} headless seat.\n\
         [core]\n\
         idle-time=0\n\
         \n\
         [shell]\n\
         panel-position=none\n\
         background-color=0x000000\n",
        size.width, size.height
    )
}

/// Build the argv for a per-seat headless Weston compositor.
///
/// The software renderer is explicit so a seat never depends on a GPU or opens
/// DRM. `--socket` and `XDG_RUNTIME_DIR` together define the private
/// `WAYLAND_DISPLAY` socket.
pub fn weston_server_args(display: &str, size: SeatDisplaySize, config: &Path) -> Vec<String> {
    vec![
        "--backend=headless".to_owned(),
        "--renderer=pixman".to_owned(),
        format!("--socket={display}"),
        "--idle-time=0".to_owned(),
        format!("--config={}", config.display()),
        format!("--width={}", size.width),
        format!("--height={}", size.height),
    ]
}

/// Generate the per-seat minimal `sway` config.
///
/// There is no bar, background process, or `exec`; the config is a single
/// headless output with no borders and Xwayland disabled.
pub fn sway_config(size: SeatDisplaySize) -> String {
    format!(
        "# Generated by ozma-multiseat for a throwaway {}x{} headless seat.\n\
         output * resolution {}x{}\n\
         default_border none\n\
         xwayland disable\n",
        size.width, size.height, size.width, size.height
    )
}

/// Build the argv for a per-seat headless sway compositor.
///
/// The backend renderer and input policy come from the environment
/// (`WLR_BACKENDS=headless`, `WLR_RENDERER=pixman`,
/// `WLR_LIBINPUT_NO_DEVICES=1`), so a seat never probes DRM or the host's
/// existing session.
pub fn sway_server_args(config: &Path) -> Vec<String> {
    vec!["--config".to_owned(), config.display().to_string()]
}

/// Wait for sway to create its Wayland socket inside the private runtime dir.
#[cfg(unix)]
fn wait_for_sway_socket(
    runtime: &Path,
    display: &str,
    timeout: Duration,
) -> Result<PathBuf, MultiseatError> {
    use std::os::unix::fs::FileTypeExt;
    let deadline = Instant::now() + timeout;
    loop {
        if let Ok(entries) = fs::read_dir(runtime) {
            for entry in entries.flatten() {
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if !name.starts_with("wayland-") || name.ends_with(".lock") {
                    continue;
                }
                let path = entry.path();
                if entry
                    .file_type()
                    .map(|kind| kind.is_socket())
                    .unwrap_or(false)
                {
                    return Ok(path);
                }
            }
        }
        if Instant::now() >= deadline {
            return Err(MultiseatError::SeatDisplayStartupTimeout {
                display: display.to_owned(),
                timeout_ms: u64::try_from(timeout.as_millis()).unwrap_or(u64::MAX),
                detail: "sway did not create a Wayland socket in the private runtime directory"
                    .to_owned(),
            });
        }
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct Xauthority {
    dir: PathBuf,
    file: PathBuf,
    cookie: Vec<u8>,
}

impl Xauthority {
    fn remove(&mut self) {
        let _ = fs::remove_dir_all(&self.dir);
    }
}

static AUTH_SEQUENCE: AtomicU64 = AtomicU64::new(0);

fn create_xauthority(xauth: &Path, display: &str) -> Result<Option<Xauthority>, MultiseatError> {
    let Some(xauth_program) = resolve_program(xauth) else {
        return Ok(None);
    };
    let cookie = random_cookie();
    let hex = hex_encode(&cookie);
    let sequence = AUTH_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "ozma-seat-display-{}-{sequence}",
        std::process::id()
    ));
    fs::create_dir_all(&dir).map_err(|error| {
        MultiseatError::SeatDisplay(format!(
            "creating Xauthority directory {}: {error}",
            dir.display()
        ))
    })?;
    restrict_directory(&dir);
    let file = dir.join("Xauthority");
    let output = Command::new(&xauth_program)
        .arg("-f")
        .arg(&file)
        .arg("add")
        .arg(display)
        .arg("MIT-MAGIC-COOKIE-1")
        .arg(&hex)
        .stdin(Stdio::null())
        .output();
    match output {
        Ok(output) if output.status.success() => Ok(Some(Xauthority { dir, file, cookie })),
        Ok(output) => {
            let _ = fs::remove_dir_all(&dir);
            Err(MultiseatError::SeatDisplay(format!(
                "xauth add failed for display {display} (status {}): {}",
                output.status.code().unwrap_or(-1),
                String::from_utf8_lossy(&output.stderr).trim()
            )))
        }
        Err(error) => {
            let _ = fs::remove_dir_all(&dir);
            Err(MultiseatError::SeatDisplay(format!(
                "running xauth for display {display}: {error}"
            )))
        }
    }
}

#[cfg(unix)]
fn restrict_directory(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o700));
}

#[cfg(not(unix))]
fn restrict_directory(_path: &Path) {}

fn random_cookie() -> Vec<u8> {
    let mut bytes = [0u8; 16];
    if let Ok(mut file) = File::open("/dev/urandom") {
        if file.read_exact(&mut bytes).is_ok() {
            return bytes.to_vec();
        }
    }
    // Deterministic fallback still gives 16 distinct bytes per call; this path
    // only runs when /dev/urandom is unavailable.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_nanos())
        .unwrap_or_default();
    let pid = u128::from(std::process::id());
    let sequence = u128::from(AUTH_SEQUENCE.fetch_add(1, Ordering::Relaxed));
    let mut value = now ^ (pid << 64) ^ (sequence << 96);
    for byte in &mut bytes {
        *byte = (value & 0xff) as u8;
        value = value.rotate_right(8) ^ (value >> 3);
    }
    bytes.to_vec()
}

fn hex_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// Resolve a program on `PATH`, or return `None` when it is not executable.
pub fn resolve_program(program: &Path) -> Option<PathBuf> {
    if program.as_os_str().is_empty() {
        return None;
    }
    if program.components().count() > 1 || program.is_absolute() {
        return is_executable(program).then(|| program.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    for directory in std::env::split_paths(&path) {
        let candidate = directory.join(program);
        if is_executable(&candidate) {
            return Some(candidate);
        }
    }
    None
}

fn is_executable(path: &Path) -> bool {
    let Ok(metadata) = fs::metadata(path) else {
        return false;
    };
    if !metadata.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        metadata.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

/// Probe an X server by completing the X11 connection setup handshake.
///
/// A successful return proves the server answered with `Success` and is ready
/// to accept clients. A missing socket or refused connection is `Ok(false)`;
/// a protocol/auth rejection is a typed error.
#[cfg(unix)]
pub fn probe_x11_display(
    display: &str,
    cookie: Option<&[u8]>,
    timeout: Duration,
) -> Result<bool, MultiseatError> {
    use std::os::unix::net::UnixStream;

    let Some(number) = parse_display_number(display) else {
        return Err(MultiseatError::SeatDisplay(format!(
            "cannot probe {display:?}: expected a local :N display"
        )));
    };
    let socket = format!("/tmp/.X11-unix/X{number}");
    let mut stream = match UnixStream::connect(&socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false)
        }
        Err(error) => {
            return Err(MultiseatError::SeatDisplay(format!(
                "connecting to {socket}: {error}"
            )))
        }
    };
    let _ = stream.set_read_timeout(Some(timeout));
    let _ = stream.set_write_timeout(Some(timeout));

    let request = x11_setup_request(cookie);
    if let Err(error) = stream.write_all(&request) {
        if matches!(
            error.kind(),
            std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
        ) {
            return Ok(false);
        }
        return Err(MultiseatError::SeatDisplay(format!(
            "writing X11 setup to {display}: {error}"
        )));
    }

    let mut head = [0u8; 8];
    match read_exact_with_timeout(&mut stream, &mut head) {
        Ok(()) => {}
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::WouldBlock
                    | std::io::ErrorKind::TimedOut
                    | std::io::ErrorKind::UnexpectedEof
                    | std::io::ErrorKind::ConnectionReset
            ) =>
        {
            return Ok(false)
        }
        Err(error) => {
            return Err(MultiseatError::SeatDisplay(format!(
                "reading X11 setup reply from {display}: {error}"
            )))
        }
    }

    if head[0] == 1 {
        return Ok(true);
    }
    let reason_len = usize::from(head[1]);
    let mut reason = vec![0u8; reason_len];
    if reason_len > 0 {
        let _ = read_exact_with_timeout(&mut stream, &mut reason);
    }
    let reason = String::from_utf8_lossy(&reason);
    Err(MultiseatError::SeatDisplay(format!(
        "X server {display} rejected the setup handshake: {}",
        reason.trim()
    )))
}

/// Probe a private Wayland compositor.
///
/// A missing socket or refused connection is `Ok(false)`. A successful Unix
/// connection is followed, when `wayland-info` is installed, by a real client
/// round-trip using this seat's private `XDG_RUNTIME_DIR` and
/// `WAYLAND_DISPLAY`. That proves the socket speaks Wayland, not merely that a
/// listener exists. Without `wayland-info`, the successful connection is the
/// readiness evidence.
#[cfg(unix)]
pub fn probe_wayland_display(env: &SeatWaylandEnv) -> Result<bool, MultiseatError> {
    use std::os::unix::net::UnixStream;

    if !env.socket.exists() {
        return Ok(false);
    }
    let stream = match UnixStream::connect(&env.socket) {
        Ok(stream) => stream,
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
            ) =>
        {
            return Ok(false)
        }
        Err(error) => {
            return Err(MultiseatError::SeatDisplay(format!(
                "connecting to Wayland socket {}: {error}",
                env.socket.display()
            )))
        }
    };
    // Drop the probe connection before starting an external client.
    let _ = stream.shutdown(std::net::Shutdown::Both);
    drop(stream);

    let Some(program) = resolve_program(Path::new("wayland-info")) else {
        return Ok(true);
    };
    let output = Command::new(program)
        .env("XDG_RUNTIME_DIR", &env.xdg_runtime_dir)
        .env("WAYLAND_DISPLAY", &env.wayland_display)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .output();
    match output {
        Ok(output) => Ok(output.status.success()),
        Err(error) => Err(MultiseatError::SeatDisplay(format!(
            "running wayland-info against {}: {error}",
            env.socket.display()
        ))),
    }
}

/// Non-Unix hosts have no Wayland Unix socket.
#[cfg(not(unix))]
pub fn probe_wayland_display(_env: &SeatWaylandEnv) -> Result<bool, MultiseatError> {
    Err(MultiseatError::Unsupported(
        "per-seat Wayland compositors require a Unix host".to_owned(),
    ))
}

fn read_exact_with_timeout(stream: &mut impl Read, buffer: &mut [u8]) -> std::io::Result<()> {
    let mut filled = 0;
    while filled < buffer.len() {
        match stream.read(&mut buffer[filled..]) {
            Ok(0) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "X server closed the connection",
                ))
            }
            Ok(read) => filled += read,
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {}
            Err(error) => return Err(error),
        }
    }
    Ok(())
}

/// Build the X11 connection setup request for a local client.
pub fn x11_setup_request(cookie: Option<&[u8]>) -> Vec<u8> {
    const AUTH_NAME: &[u8] = b"MIT-MAGIC-COOKIE-1";
    let (name, data): (&[u8], &[u8]) = match cookie {
        Some(cookie) => (AUTH_NAME, cookie),
        None => (&[], &[]),
    };

    let mut request = Vec::with_capacity(12 + name.len() + data.len() + 6);
    request.push(b'l'); // little-endian
    request.push(0);
    request.extend_from_slice(&11u16.to_le_bytes()); // protocol major
    request.extend_from_slice(&0u16.to_le_bytes()); // protocol minor
    request.extend_from_slice(&(name.len() as u16).to_le_bytes());
    request.extend_from_slice(&(data.len() as u16).to_le_bytes());
    request.extend_from_slice(&[0, 0]); // unused
    request.extend_from_slice(name);
    while request.len() % 4 != 0 {
        request.push(0);
    }
    request.extend_from_slice(data);
    while request.len() % 4 != 0 {
        request.push(0);
    }
    request
}

/// Non-Unix hosts have no X11 Unix socket; the process backend is unsupported.
#[cfg(not(unix))]
pub fn probe_x11_display(
    _display: &str,
    _cookie: Option<&[u8]>,
    _timeout: Duration,
) -> Result<bool, MultiseatError> {
    Err(MultiseatError::Unsupported(
        "per-seat X servers require a Unix host".to_owned(),
    ))
}

/// Build the argv for one private X server. Pure and used by tests.
///
/// `Xvfb`/`Xephyr` take geometry on the command line. `Xorg` takes geometry,
/// video, and input configuration from `xorg_config`; it is started with
/// `-novtswitch -sharevts` so a throwaway seat does not steal a VT, and with
/// `-noreset` so the server survives its last client exactly like a supervised
/// seat server. `-nolisten tcp` is unconditional for every backend. The Weston
/// backend does not use this function; see [`weston_server_args`].
pub fn display_server_args(
    backend: SeatDisplayBackend,
    display: &str,
    size: SeatDisplaySize,
    auth_file: Option<&Path>,
    seat_id: &str,
    xorg_config: Option<&Path>,
) -> Vec<String> {
    if backend == SeatDisplayBackend::Xorg {
        let mut args = vec![display.to_owned()];
        if let Some(config) = xorg_config {
            args.push("-config".to_owned());
            args.push(config.display().to_string());
            let number = parse_display_number(display).unwrap_or(0);
            let log = config.with_file_name(format!("Xorg-{number}.log"));
            args.push("-logfile".to_owned());
            args.push(log.display().to_string());
        }
        args.extend(
            [
                "-nolisten",
                "tcp",
                "-noreset",
                "-novtswitch",
                "-sharevts",
                "-keeptty",
            ]
            .into_iter()
            .map(str::to_owned),
        );
        push_auth_args(&mut args, auth_file);
        return args;
    }

    let mut args = vec![
        display.to_owned(),
        "-screen".to_owned(),
        "0".to_owned(),
        size.x_screen(),
    ];
    if backend == SeatDisplayBackend::Xephyr {
        args.push("-title".to_owned());
        args.push(format!("ozma-{seat_id}"));
    }
    args.push("-nolisten".to_owned());
    args.push("tcp".to_owned());
    push_auth_args(&mut args, auth_file);
    args
}

fn push_auth_args(args: &mut Vec<String>, auth_file: Option<&Path>) {
    match auth_file {
        Some(file) => {
            args.push("-auth".to_owned());
            args.push(file.display().to_string());
        }
        None => args.push("-ac".to_owned()),
    }
}

/// Map a typed display error into a stable status kind.
pub fn display_error_kind(error: &MultiseatError) -> &'static str {
    match error {
        MultiseatError::SeatDisplayTool { .. } => "missing_binary",
        MultiseatError::SeatDisplayNumberExhausted { .. } => "number_exhausted",
        MultiseatError::SeatDisplayStartupTimeout { .. } => "startup_timeout",
        MultiseatError::SeatDisplayCrashed { .. } => "crashed",
        MultiseatError::Unsupported(_) => "unsupported",
        _ => "error",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn size() -> SeatDisplaySize {
        SeatDisplaySize::new(1280, 800).expect("size")
    }

    #[test]
    fn backend_names_include_xorg() {
        assert_eq!(SeatDisplayBackend::Xorg.as_str(), "xorg");
        assert_eq!(
            "xorg".parse::<SeatDisplayBackend>().expect("parse xorg"),
            SeatDisplayBackend::Xorg
        );
        assert!(SeatDisplayBackend::Xorg.is_nested());
    }

    #[test]
    fn xorg_args_use_config_no_vt_switch_and_the_auth_cookie() {
        let args = display_server_args(
            SeatDisplayBackend::Xorg,
            ":90",
            size(),
            Some(Path::new("/tmp/seat/Xauthority")),
            "seat-a",
            Some(Path::new("/tmp/seat/xorg.conf")),
        );
        assert_eq!(
            args,
            vec![
                ":90",
                "-config",
                "/tmp/seat/xorg.conf",
                "-logfile",
                "/tmp/seat/Xorg-90.log",
                "-nolisten",
                "tcp",
                "-noreset",
                "-novtswitch",
                "-sharevts",
                "-keeptty",
                "-auth",
                "/tmp/seat/Xauthority",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "-screen"));
    }

    #[test]
    fn xorg_without_xauth_uses_socket_only_ac() {
        let args = display_server_args(
            SeatDisplayBackend::Xorg,
            ":91",
            size(),
            None,
            "seat-b",
            Some(Path::new("/tmp/seat/xorg.conf")),
        );
        assert_eq!(args.last().map(String::as_str), Some("-ac"));
        assert_eq!(
            args.iter()
                .filter(|arg| arg.as_str() == "-nolisten")
                .count(),
            1
        );
        assert!(args.windows(2).any(|pair| pair == ["-nolisten", "tcp"]));
    }

    #[test]
    fn xvfb_args_keep_geometry_and_do_not_gain_xorg_flags() {
        let args = display_server_args(
            SeatDisplayBackend::Xvfb,
            ":90",
            size(),
            Some(Path::new("/tmp/seat/Xauthority")),
            "seat-a",
            Some(Path::new("/tmp/seat/xorg.conf")),
        );
        assert_eq!(
            args,
            vec![
                ":90",
                "-screen",
                "0",
                "1280x800x24",
                "-nolisten",
                "tcp",
                "-auth",
                "/tmp/seat/Xauthority",
            ]
        );
        assert!(!args.iter().any(|arg| arg == "-novtswitch"));
        assert!(!args.iter().any(|arg| arg == "-config"));
    }

    #[test]
    fn xorg_without_config_is_typed_unsupported_instead_of_a_gpu_probe() {
        let options = SeatDisplayOptions {
            backend: SeatDisplayBackend::Xorg,
            xorg_config: None,
            ..SeatDisplayOptions::default()
        };
        let programs = SeatDisplayPrograms {
            xorg: std::env::current_exe().expect("current exe"),
            ..SeatDisplayPrograms::default()
        };
        let launcher = ProcessSeatDisplayLauncher::new(options).with_programs(programs);
        let request = SeatDisplayRequest {
            seat_id: "seat-a".to_owned(),
            backend: SeatDisplayBackend::Xorg,
            size: size(),
            parent_display: None,
        };
        let error = match launcher.spawn(&request) {
            Ok(_) => panic!("spawning Xorg without a config must fail"),
            Err(error) => error,
        };
        assert!(
            matches!(error, MultiseatError::Unsupported(_)),
            "unexpected error: {error}"
        );
        assert!(error.to_string().contains("config file"));
    }

    #[test]
    fn weston_backend_is_wayland_and_has_a_stable_name() {
        assert_eq!(SeatDisplayBackend::Weston.as_str(), "weston");
        assert_eq!(
            "weston"
                .parse::<SeatDisplayBackend>()
                .expect("parse weston"),
            SeatDisplayBackend::Weston
        );
        assert_eq!(
            "wayland"
                .parse::<SeatDisplayBackend>()
                .expect("parse wayland alias"),
            SeatDisplayBackend::Weston
        );
        assert!(SeatDisplayBackend::Weston.is_nested());
        assert!(SeatDisplayBackend::Weston.is_wayland());
        assert!(!SeatDisplayBackend::Xvfb.is_wayland());
    }

    #[test]
    fn weston_args_are_headless_software_and_use_the_private_socket_and_config() {
        let args = weston_server_args(
            "ozma-weston-90",
            size(),
            Path::new("/tmp/ozma-seat-weston-1/weston.ini"),
        );
        assert_eq!(
            args,
            vec![
                "--backend=headless",
                "--renderer=pixman",
                "--socket=ozma-weston-90",
                "--idle-time=0",
                "--config=/tmp/ozma-seat-weston-1/weston.ini",
                "--width=1280",
                "--height=800",
            ]
        );
        assert!(!args.iter().any(|arg| arg.contains("drm")));
        assert!(!args.iter().any(|arg| arg.contains("x11")));
        assert!(!args.iter().any(|arg| arg.contains("wayland")));
    }

    #[test]
    fn weston_config_is_generated_without_a_desktop_panel_or_idle_blank() {
        let config = weston_config(size());
        assert!(config.contains("[core]"));
        assert!(config.contains("idle-time=0"));
        assert!(config.contains("[shell]"));
        assert!(config.contains("panel-position=none"));
    }

    #[test]
    fn wayland_env_joins_the_private_runtime_dir_and_socket_name() {
        let env = SeatWaylandEnv::new("ozma-weston-90", "/tmp/ozma-seat-weston-1/runtime");
        assert_eq!(env.wayland_display, "ozma-weston-90");
        assert_eq!(
            env.socket,
            PathBuf::from("/tmp/ozma-seat-weston-1/runtime/ozma-weston-90")
        );
        assert_eq!(wayland_display_name(90), "ozma-weston-90");
    }

    #[test]
    fn steerable_backends_report_their_wayland_names() {
        assert_eq!(SeatDisplayBackend::Sway.as_str(), "sway");
        assert_eq!(
            "sway".parse::<SeatDisplayBackend>().expect("parse sway"),
            SeatDisplayBackend::Sway
        );
        assert_eq!(
            "wlroots"
                .parse::<SeatDisplayBackend>()
                .expect("parse wlroots alias"),
            SeatDisplayBackend::Sway
        );
        assert!(SeatDisplayBackend::Sway.is_nested());
        assert!(SeatDisplayBackend::Sway.is_wayland());
        assert!(!SeatDisplayBackend::Xvfb.is_wayland());
        assert_eq!(sway_display_name(90), "ozma-sway-90");
    }

    #[test]
    fn sway_args_and_config_are_headless_and_disable_xwayland() {
        let args = sway_server_args(Path::new("/tmp/ozma-seat-sway-1/sway.conf"));
        assert_eq!(args, vec!["--config", "/tmp/ozma-seat-sway-1/sway.conf"]);
        assert!(!args.iter().any(|arg| arg.contains("drm")));
        let config = sway_config(size());
        assert!(config.contains("output * resolution 1280x800"));
        assert!(config.contains("default_border none"));
        assert!(config.contains("xwayland disable"));
    }

    #[test]
    fn missing_sway_binary_is_a_typed_missing_binary_error() {
        let options = SeatDisplayOptions {
            backend: SeatDisplayBackend::Sway,
            ..SeatDisplayOptions::default()
        };
        let programs = SeatDisplayPrograms {
            sway: PathBuf::from("/nonexistent/ozma-sway"),
            ..SeatDisplayPrograms::default()
        };
        let launcher = ProcessSeatDisplayLauncher::new(options).with_programs(programs);
        let request = SeatDisplayRequest {
            seat_id: "seat-a".to_owned(),
            backend: SeatDisplayBackend::Sway,
            size: size(),
            parent_display: None,
        };
        let error = match launcher.spawn(&request) {
            Ok(_) => panic!("spawning without sway must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, MultiseatError::SeatDisplayTool { .. }));
        assert_eq!(display_error_kind(&error), "missing_binary");
        assert!(launcher.in_use().is_empty());
    }

    #[test]
    fn missing_weston_binary_is_a_typed_missing_binary_error() {
        let options = SeatDisplayOptions {
            backend: SeatDisplayBackend::Weston,
            ..SeatDisplayOptions::default()
        };
        let programs = SeatDisplayPrograms {
            weston: PathBuf::from("/nonexistent/ozma-weston"),
            ..SeatDisplayPrograms::default()
        };
        let launcher = ProcessSeatDisplayLauncher::new(options).with_programs(programs);
        let request = SeatDisplayRequest {
            seat_id: "seat-a".to_owned(),
            backend: SeatDisplayBackend::Weston,
            size: size(),
            parent_display: None,
        };
        let error = match launcher.spawn(&request) {
            Ok(_) => panic!("spawning without weston must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, MultiseatError::SeatDisplayTool { .. }));
        assert_eq!(display_error_kind(&error), "missing_binary");
        assert!(launcher.in_use().is_empty());
    }
}
