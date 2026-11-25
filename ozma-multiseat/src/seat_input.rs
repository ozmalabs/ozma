// SPDX-License-Identifier: AGPL-3.0-only
//! Per-seat synthetic Wayland input injection.
//!
//! A seat whose display backend is a private headless wlroots compositor
//! (currently `sway`) exposes two protocol globals that allow privileged
//! clients to create synthetic devices:
//!
//! * `zwp_virtual_keyboard_manager_v1` with an XKB keymap and key events;
//! * `zwlr_virtual_pointer_manager_v1` with relative motion and buttons.
//!
//! [`WaylandSeatInputInjector`] connects **only** to the seat's private
//! `WAYLAND_DISPLAY` socket. It never creates a global uinput device and never
//! sends an event to another compositor. An injector that cannot find the
//! required global reports [`SeatInputError::ProtocolUnavailable`] instead of
//! pretending the seat accepted input.
//!
//! Tests do not need a compositor: [`SeatInputInjector`] is the seam and the
//! `FakeSeatInputInjector` in [`crate::fake`] records events per seat. The
//! opt-in live test uses the real implementation against two throwaway sway
//! compositors.

use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::error::MultiseatError;
use crate::seat_display::SeatWaylandEnv;

/// Stable name of the wlroots/standard virtual-keyboard global.
pub const VIRTUAL_KEYBOARD_PROTOCOL: &str = "zwp_virtual_keyboard_manager_v1";
/// Stable name of the wlroots virtual-pointer global.
pub const VIRTUAL_POINTER_PROTOCOL: &str = "zwlr_virtual_pointer_manager_v1";
/// XKB keymap format (`WL_KEYBOARD_KEYMAP_FORMAT_XKB_V1`).
pub const XKB_V1_FORMAT: u32 = 1;
/// XKB keycode for the left shift key (evdev 42 + 8).
pub const XKB_KEY_LEFT_SHIFT: u32 = 50;
/// Linux/Wayland button code for the primary mouse button.
pub const BTN_LEFT: u32 = 0x110;

/// How a seat supplies synthetic input.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatInputMode {
    /// Do not create an injector (default).
    #[default]
    Off,
    /// Create virtual keyboard/pointer objects on the seat's own Wayland socket.
    Wayland,
}

impl SeatInputMode {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Wayland => "wayland",
        }
    }

    /// Whether the mode creates a per-seat Wayland injector.
    pub fn is_wayland(self) -> bool {
        matches!(self, Self::Wayland)
    }
}

impl fmt::Display for SeatInputMode {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SeatInputMode {
    type Err = MultiseatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "wayland" => Ok(Self::Wayland),
            other => Err(MultiseatError::SeatInput(format!(
                "unknown seat input mode {other:?}; expected off or wayland"
            ))),
        }
    }
}

/// One request to connect an injector to a seat's private compositor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatInputRequest {
    /// Stable seat id.
    pub seat_id: String,
    /// Private compositor socket and runtime directory.
    pub wayland: SeatWaylandEnv,
}

/// Protocol availability discovered on one seat socket.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatInputCapabilities {
    /// `zwp_virtual_keyboard_manager_v1` was bound and a keyboard was created.
    pub keyboard: bool,
    /// `zwlr_virtual_pointer_manager_v1` was bound and a pointer was created.
    pub pointer: bool,
    /// Keyboard protocol name, for evidence/status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub keyboard_protocol: Option<String>,
    /// Pointer protocol name, for evidence/status.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer_protocol: Option<String>,
}

impl SeatInputCapabilities {
    /// Whether text/key injection is possible.
    pub fn supports_text(&self) -> bool {
        self.keyboard
    }

    /// Whether every requested device class is available.
    pub fn is_complete(&self) -> bool {
        self.keyboard && self.pointer
    }
}

/// Pressed or released, matching the Wayland key/button state values.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatKeyState {
    /// Key/button released (value 0).
    Released,
    /// Key/button pressed (value 1).
    Pressed,
}

impl SeatKeyState {
    /// Protocol value.
    pub fn value(self) -> u32 {
        match self {
            Self::Released => 0,
            Self::Pressed => 1,
        }
    }
}

/// Typed synthetic-input failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SeatInputError {
    /// The compositor did not advertise a required global.
    #[error("seat display {display:?} does not expose {protocol}")]
    ProtocolUnavailable {
        /// `WAYLAND_DISPLAY` name.
        display: String,
        /// Missing global interface.
        protocol: String,
    },
    /// The compositor or its socket refused/could not complete the connection.
    #[error("seat display {display:?} refused synthetic input: {message}")]
    CompositorRefused {
        /// `WAYLAND_DISPLAY` name.
        display: String,
        /// Protocol/transport detail.
        message: String,
    },
    /// The socket or an fd/pipe disappeared.
    #[error("seat display {display:?} is disconnected: {message}")]
    Disconnected {
        /// `WAYLAND_DISPLAY` name.
        display: String,
        /// Underlying error.
        message: String,
    },
    /// No connection was opened for this seat.
    #[error("seat {0:?} has no Wayland input injector connection")]
    NotConnected(String),
    /// A character has no simple US-keymap keycode mapping.
    #[error("cannot inject character {0:?} with the built-in US keymap")]
    UnsupportedKey(char),
    /// Local keymap/fd I/O failed.
    #[error("synthetic input I/O error: {0}")]
    Io(String),
    /// The host is not Unix or the feature is unavailable.
    #[error("synthetic Wayland input is unsupported: {0}")]
    Unsupported(String),
}

impl SeatInputError {
    /// Stable machine-readable kind.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::ProtocolUnavailable { .. } => "protocol_unavailable",
            Self::CompositorRefused { .. } => "compositor_refused",
            Self::Disconnected { .. } => "disconnected",
            Self::NotConnected(_) => "not_connected",
            Self::UnsupportedKey(_) => "unsupported_key",
            Self::Io(_) => "io",
            Self::Unsupported(_) => "unsupported",
        }
    }

    /// Convert into the crate-wide typed error.
    pub fn into_multiseat(self) -> MultiseatError {
        MultiseatError::SeatInput(format!("{}: {self}", self.kind()))
    }
}

/// Injectable per-seat synthetic-input seam.
///
/// Implementations must keep every event bound to the seat connection created
/// by [`Self::connect`]. There is deliberately no global-device operation.
pub trait SeatInputInjector {
    /// Connect to the seat's private socket and create the virtual devices.
    fn connect(
        &mut self,
        request: &SeatInputRequest,
    ) -> Result<SeatInputCapabilities, SeatInputError>;

    /// Capabilities for a connected seat.
    fn capabilities(&self, seat_id: &str) -> Option<SeatInputCapabilities>;

    /// Send one key event using an XKB keycode.
    fn inject_key(
        &mut self,
        seat_id: &str,
        keycode: u32,
        state: SeatKeyState,
    ) -> Result<(), SeatInputError>;

    /// Type text using press/release pairs. Returns the number of key events.
    fn inject_text(&mut self, seat_id: &str, text: &str) -> Result<usize, SeatInputError> {
        let mut events = 0;
        for character in text.chars() {
            match ascii_key(character) {
                Some((keycode, shifted)) => {
                    if shifted {
                        self.inject_key(seat_id, XKB_KEY_LEFT_SHIFT, SeatKeyState::Pressed)?;
                        events += 1;
                    }
                    self.inject_key(seat_id, keycode, SeatKeyState::Pressed)?;
                    self.inject_key(seat_id, keycode, SeatKeyState::Released)?;
                    events += 2;
                    if shifted {
                        self.inject_key(seat_id, XKB_KEY_LEFT_SHIFT, SeatKeyState::Released)?;
                        events += 1;
                    }
                }
                None => return Err(SeatInputError::UnsupportedKey(character)),
            }
        }
        Ok(events)
    }

    /// Send relative pointer motion.
    fn pointer_motion(&mut self, seat_id: &str, dx: f64, dy: f64) -> Result<(), SeatInputError>;

    /// Send one pointer button event.
    fn pointer_button(
        &mut self,
        seat_id: &str,
        button: u32,
        state: SeatKeyState,
    ) -> Result<(), SeatInputError>;

    /// Drop the seat connection; no further events may be sent.
    fn disconnect(&mut self, seat_id: &str);

    /// Number of events queued for a seat (best-effort status evidence).
    fn events_sent(&self, _seat_id: &str) -> u64 {
        0
    }

    /// Currently connected seat ids.
    fn connected_seats(&self) -> Vec<String>;
}

/// Map an ASCII character to `(XKB keycode, needs_shift)` for the built-in US
/// keymap. The XKB keycode is the Linux evdev code plus 8.
pub fn ascii_key(character: char) -> Option<(u32, bool)> {
    let (evdev, shifted) = match character {
        'a' => (30, false),
        'b' => (48, false),
        'c' => (46, false),
        'd' => (32, false),
        'e' => (18, false),
        'f' => (33, false),
        'g' => (34, false),
        'h' => (35, false),
        'i' => (23, false),
        'j' => (36, false),
        'k' => (37, false),
        'l' => (38, false),
        'm' => (50, false),
        'n' => (49, false),
        'o' => (24, false),
        'p' => (25, false),
        'q' => (16, false),
        'r' => (19, false),
        's' => (31, false),
        't' => (20, false),
        'u' => (22, false),
        'v' => (47, false),
        'w' => (17, false),
        'x' => (45, false),
        'y' => (21, false),
        'z' => (44, false),
        'A' => (30, true),
        'B' => (48, true),
        'C' => (46, true),
        'D' => (32, true),
        'E' => (18, true),
        'F' => (33, true),
        'G' => (34, true),
        'H' => (35, true),
        'I' => (23, true),
        'J' => (36, true),
        'K' => (37, true),
        'L' => (38, true),
        'M' => (50, true),
        'N' => (49, true),
        'O' => (24, true),
        'P' => (25, true),
        'Q' => (16, true),
        'R' => (19, true),
        'S' => (31, true),
        'T' => (20, true),
        'U' => (22, true),
        'V' => (47, true),
        'W' => (17, true),
        'X' => (45, true),
        'Y' => (21, true),
        'Z' => (44, true),
        '1' => (2, false),
        '2' => (3, false),
        '3' => (4, false),
        '4' => (5, false),
        '5' => (6, false),
        '6' => (7, false),
        '7' => (8, false),
        '8' => (9, false),
        '9' => (10, false),
        '0' => (11, false),
        '!' => (2, true),
        '@' => (3, true),
        '#' => (4, true),
        '$' => (5, true),
        '%' => (6, true),
        '^' => (7, true),
        '&' => (8, true),
        '*' => (9, true),
        '(' => (10, true),
        ')' => (11, true),
        '-' => (12, false),
        '_' => (12, true),
        '=' => (13, false),
        '+' => (13, true),
        '\t' => (15, false),
        '[' => (26, false),
        '{' => (26, true),
        ']' => (27, false),
        '}' => (27, true),
        '\n' => (28, false),
        ';' => (39, false),
        ':' => (39, true),
        '\'' => (40, false),
        '"' => (40, true),
        '`' => (41, false),
        '~' => (41, true),
        '\\' => (43, false),
        '|' => (43, true),
        ',' => (51, false),
        '<' => (51, true),
        '.' => (52, false),
        '>' => (52, true),
        '/' => (53, false),
        '?' => (53, true),
        ' ' => (57, false),
        _ => return None,
    };
    Some(((evdev + 8) as u32, shifted))
}

#[cfg(unix)]
mod imp {
    use std::collections::BTreeMap;
    use std::fs::{self, File, OpenOptions};
    use std::io::Write;
    use std::os::fd::AsFd;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::UnixStream;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::Instant;

    use wayland_client::globals::{registry_queue_init, GlobalListContents};
    use wayland_client::protocol::wl_registry::WlRegistry;
    use wayland_client::protocol::wl_seat::WlSeat;
    use wayland_client::{Connection, Dispatch, Proxy, QueueHandle};
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1;
    use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1;
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1;
    use wayland_protocols_wlr::virtual_pointer::v1::client::zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1;

    use super::{
        SeatInputCapabilities, SeatInputError, SeatInputInjector, SeatInputRequest, SeatKeyState,
        VIRTUAL_KEYBOARD_PROTOCOL, VIRTUAL_POINTER_PROTOCOL, XKB_V1_FORMAT,
    };
    use crate::seat_display::SeatWaylandEnv;

    static KEYMAP_SEQUENCE: AtomicU64 = AtomicU64::new(0);

    /// Built-in US keymap. It is compiled into the binary so injection never
    /// depends on the host's `xkb` data or an external `xkbcomp`.
    pub(super) const US_KEYMAP: &str = include_str!("../data/wayland-us.xkb");

    #[derive(Default)]
    struct InjectState;

    impl Dispatch<WlRegistry, GlobalListContents> for InjectState {
        fn event(
            _state: &mut Self,
            _proxy: &WlRegistry,
            _event: <WlRegistry as Proxy>::Event,
            _data: &GlobalListContents,
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<WlSeat, ()> for InjectState {
        fn event(
            _state: &mut Self,
            _proxy: &WlSeat,
            _event: <WlSeat as Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwpVirtualKeyboardManagerV1, ()> for InjectState {
        fn event(
            _state: &mut Self,
            _proxy: &ZwpVirtualKeyboardManagerV1,
            _event: <ZwpVirtualKeyboardManagerV1 as Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwpVirtualKeyboardV1, ()> for InjectState {
        fn event(
            _state: &mut Self,
            _proxy: &ZwpVirtualKeyboardV1,
            _event: <ZwpVirtualKeyboardV1 as Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwlrVirtualPointerManagerV1, ()> for InjectState {
        fn event(
            _state: &mut Self,
            _proxy: &ZwlrVirtualPointerManagerV1,
            _event: <ZwlrVirtualPointerManagerV1 as Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    impl Dispatch<ZwlrVirtualPointerV1, ()> for InjectState {
        fn event(
            _state: &mut Self,
            _proxy: &ZwlrVirtualPointerV1,
            _event: <ZwlrVirtualPointerV1 as Proxy>::Event,
            _data: &(),
            _conn: &Connection,
            _qh: &QueueHandle<Self>,
        ) {
        }
    }

    struct Session {
        connection: Connection,
        keyboard: Option<ZwpVirtualKeyboardV1>,
        pointer: Option<ZwlrVirtualPointerV1>,
        capabilities: SeatInputCapabilities,
        display: String,
        socket: PathBuf,
        started: Instant,
        events: u64,
    }

    impl Session {
        fn timestamp_ms(&self) -> u32 {
            u32::try_from(self.started.elapsed().as_millis()).unwrap_or(u32::MAX)
        }

        fn flush(&self) -> Result<(), SeatInputError> {
            self.connection.flush().map_err(|error| {
                if self.connection.display().is_alive() {
                    SeatInputError::CompositorRefused {
                        display: self.display.clone(),
                        message: error.to_string(),
                    }
                } else {
                    SeatInputError::Disconnected {
                        display: self.display.clone(),
                        message: error.to_string(),
                    }
                }
            })
        }
    }

    /// Real per-seat injector backed by `wayland-client`.
    pub struct WaylandSeatInputInjector {
        sessions: BTreeMap<String, Session>,
    }

    impl Default for WaylandSeatInputInjector {
        fn default() -> Self {
            Self::new()
        }
    }

    impl WaylandSeatInputInjector {
        /// Create an injector with no connections.
        pub fn new() -> Self {
            Self {
                sessions: BTreeMap::new(),
            }
        }

        /// Event count sent to a seat (one per key/button request).
        pub fn events_sent(&self, seat_id: &str) -> u64 {
            self.sessions
                .get(seat_id)
                .map(|session| session.events)
                .unwrap_or(0)
        }

        /// Absolute socket path used for a seat, for evidence/debugging.
        pub fn socket_path(&self, seat_id: &str) -> Option<&std::path::Path> {
            self.sessions
                .get(seat_id)
                .map(|session| session.socket.as_path())
        }
    }

    fn keymap_file() -> Result<(File, u32), SeatInputError> {
        let sequence = KEYMAP_SEQUENCE.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "ozma-wayland-keymap-{}-{sequence}.xkb",
            std::process::id()
        ));
        let mut file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(&path)
            .map_err(|error| SeatInputError::Io(format!("creating {}: {error}", path.display())))?;
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        file.write_all(US_KEYMAP.as_bytes())
            .map_err(|error| SeatInputError::Io(format!("writing {}: {error}", path.display())))?;
        file.flush()
            .map_err(|error| SeatInputError::Io(format!("flushing {}: {error}", path.display())))?;
        // The compositor receives a duplicated fd with the keymap request; the
        // path itself is no longer needed.
        let _ = fs::remove_file(&path);
        Ok((file, u32::try_from(US_KEYMAP.len()).unwrap_or(u32::MAX)))
    }

    impl SeatInputInjector for WaylandSeatInputInjector {
        fn connect(
            &mut self,
            request: &SeatInputRequest,
        ) -> Result<SeatInputCapabilities, SeatInputError> {
            self.sessions.remove(&request.seat_id);
            let env: &SeatWaylandEnv = &request.wayland;
            let stream =
                UnixStream::connect(&env.socket).map_err(|error| SeatInputError::Disconnected {
                    display: env.wayland_display.clone(),
                    message: format!("connecting to {}: {error}", env.socket.display()),
                })?;
            let connection =
                Connection::from_socket(stream).map_err(|error| SeatInputError::Disconnected {
                    display: env.wayland_display.clone(),
                    message: format!("initializing wayland connection: {error}"),
                })?;

            let (globals, mut queue) =
                registry_queue_init::<InjectState>(&connection).map_err(|error| {
                    SeatInputError::CompositorRefused {
                        display: env.wayland_display.clone(),
                        message: format!("registry handshake: {error}"),
                    }
                })?;
            let qh = queue.handle();
            let seat = globals.bind::<WlSeat, _, _>(&qh, 1..=9, ()).map_err(|_| {
                SeatInputError::ProtocolUnavailable {
                    display: env.wayland_display.clone(),
                    protocol: "wl_seat".to_owned(),
                }
            })?;
            let keyboard_manager = globals
                .bind::<ZwpVirtualKeyboardManagerV1, _, _>(&qh, 1..=1, ())
                .map_err(|_| SeatInputError::ProtocolUnavailable {
                    display: env.wayland_display.clone(),
                    protocol: VIRTUAL_KEYBOARD_PROTOCOL.to_owned(),
                })?;
            let pointer_manager = globals
                .bind::<ZwlrVirtualPointerManagerV1, _, _>(&qh, 1..=2, ())
                .map_err(|_| SeatInputError::ProtocolUnavailable {
                    display: env.wayland_display.clone(),
                    protocol: VIRTUAL_POINTER_PROTOCOL.to_owned(),
                })?;

            let keyboard = keyboard_manager.create_virtual_keyboard(&seat, &qh, ());
            let pointer = pointer_manager.create_virtual_pointer(Some(&seat), &qh, ());

            let (keymap, keymap_len) = keymap_file()?;
            keyboard.keymap(XKB_V1_FORMAT, keymap.as_fd(), keymap_len);

            let mut state = InjectState;
            queue
                .roundtrip(&mut state)
                .map_err(|error| SeatInputError::CompositorRefused {
                    display: env.wayland_display.clone(),
                    message: format!("creating virtual input objects: {error}"),
                })?;
            connection
                .flush()
                .map_err(|error| SeatInputError::CompositorRefused {
                    display: env.wayland_display.clone(),
                    message: format!("flushing virtual input objects: {error}"),
                })?;

            let capabilities = SeatInputCapabilities {
                keyboard: true,
                pointer: true,
                keyboard_protocol: Some(VIRTUAL_KEYBOARD_PROTOCOL.to_owned()),
                pointer_protocol: Some(VIRTUAL_POINTER_PROTOCOL.to_owned()),
            };
            self.sessions.insert(
                request.seat_id.clone(),
                Session {
                    connection,
                    keyboard: Some(keyboard),
                    pointer: Some(pointer),
                    capabilities: capabilities.clone(),
                    display: env.wayland_display.clone(),
                    socket: env.socket.clone(),
                    started: Instant::now(),
                    events: 0,
                },
            );
            Ok(capabilities)
        }

        fn capabilities(&self, seat_id: &str) -> Option<SeatInputCapabilities> {
            self.sessions
                .get(seat_id)
                .map(|session| session.capabilities.clone())
        }

        fn inject_key(
            &mut self,
            seat_id: &str,
            keycode: u32,
            state: SeatKeyState,
        ) -> Result<(), SeatInputError> {
            let session = self
                .sessions
                .get_mut(seat_id)
                .ok_or_else(|| SeatInputError::NotConnected(seat_id.to_owned()))?;
            let keyboard = session
                .keyboard
                .as_ref()
                .ok_or_else(|| SeatInputError::NotConnected(seat_id.to_owned()))?;
            let time = session.timestamp_ms();
            keyboard.key(time, keycode, state.value());
            session.events = session.events.saturating_add(1);
            session.flush()
        }

        fn pointer_motion(
            &mut self,
            seat_id: &str,
            dx: f64,
            dy: f64,
        ) -> Result<(), SeatInputError> {
            let session = self
                .sessions
                .get_mut(seat_id)
                .ok_or_else(|| SeatInputError::NotConnected(seat_id.to_owned()))?;
            let pointer =
                session
                    .pointer
                    .as_ref()
                    .ok_or_else(|| SeatInputError::ProtocolUnavailable {
                        display: session.display.clone(),
                        protocol: VIRTUAL_POINTER_PROTOCOL.to_owned(),
                    })?;
            let time = session.timestamp_ms();
            pointer.motion(time, dx, dy);
            if pointer.version() >= 2 {
                pointer.frame();
            }
            session.events = session.events.saturating_add(1);
            session.flush()
        }

        fn pointer_button(
            &mut self,
            seat_id: &str,
            button: u32,
            state: SeatKeyState,
        ) -> Result<(), SeatInputError> {
            let session = self
                .sessions
                .get_mut(seat_id)
                .ok_or_else(|| SeatInputError::NotConnected(seat_id.to_owned()))?;
            let pointer =
                session
                    .pointer
                    .as_ref()
                    .ok_or_else(|| SeatInputError::ProtocolUnavailable {
                        display: session.display.clone(),
                        protocol: VIRTUAL_POINTER_PROTOCOL.to_owned(),
                    })?;
            let time = session.timestamp_ms();
            let button_state = match state {
                SeatKeyState::Pressed => wayland_client::protocol::wl_pointer::ButtonState::Pressed,
                SeatKeyState::Released => {
                    wayland_client::protocol::wl_pointer::ButtonState::Released
                }
            };
            pointer.button(time, button, button_state);
            if pointer.version() >= 2 {
                pointer.frame();
            }
            session.events = session.events.saturating_add(1);
            session.flush()
        }

        fn disconnect(&mut self, seat_id: &str) {
            if let Some(mut session) = self.sessions.remove(seat_id) {
                if let Some(keyboard) = session.keyboard.take() {
                    keyboard.destroy();
                }
                if let Some(pointer) = session.pointer.take() {
                    pointer.destroy();
                }
                let _ = session.connection.flush();
            }
        }

        fn events_sent(&self, seat_id: &str) -> u64 {
            self.sessions
                .get(seat_id)
                .map(|session| session.events)
                .unwrap_or(0)
        }

        fn connected_seats(&self) -> Vec<String> {
            self.sessions.keys().cloned().collect()
        }
    }
}

#[cfg(unix)]
pub use imp::WaylandSeatInputInjector;

/// Non-Unix stub: the real protocols require a Unix socket.
#[cfg(not(unix))]
#[derive(Default)]
pub struct WaylandSeatInputInjector;

#[cfg(not(unix))]
impl SeatInputInjector for WaylandSeatInputInjector {
    fn connect(
        &mut self,
        _request: &SeatInputRequest,
    ) -> Result<SeatInputCapabilities, SeatInputError> {
        Err(SeatInputError::Unsupported(
            "Wayland sockets require a Unix host".to_owned(),
        ))
    }

    fn capabilities(&self, _seat_id: &str) -> Option<SeatInputCapabilities> {
        None
    }

    fn inject_key(
        &mut self,
        _seat_id: &str,
        _keycode: u32,
        _state: SeatKeyState,
    ) -> Result<(), SeatInputError> {
        Err(SeatInputError::Unsupported(
            "Wayland sockets require a Unix host".to_owned(),
        ))
    }

    fn pointer_motion(&mut self, _seat_id: &str, _dx: f64, _dy: f64) -> Result<(), SeatInputError> {
        Err(SeatInputError::Unsupported(
            "Wayland sockets require a Unix host".to_owned(),
        ))
    }

    fn pointer_button(
        &mut self,
        _seat_id: &str,
        _button: u32,
        _state: SeatKeyState,
    ) -> Result<(), SeatInputError> {
        Err(SeatInputError::Unsupported(
            "Wayland sockets require a Unix host".to_owned(),
        ))
    }

    fn disconnect(&mut self, _seat_id: &str) {}

    fn events_sent(&self, _seat_id: &str) -> u64 {
        0
    }

    fn connected_seats(&self) -> Vec<String> {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mode_parses_off_and_wayland() {
        assert_eq!("off".parse::<SeatInputMode>().unwrap(), SeatInputMode::Off);
        assert_eq!(
            " Wayland ".parse::<SeatInputMode>().unwrap(),
            SeatInputMode::Wayland
        );
        assert!(SeatInputMode::Wayland.is_wayland());
        assert!(!SeatInputMode::Off.is_wayland());
        assert!("uinput".parse::<SeatInputMode>().is_err());
    }

    #[test]
    fn ascii_keys_use_xkb_evdev_plus_eight_codes() {
        assert_eq!(ascii_key('a'), Some((38, false)));
        assert_eq!(ascii_key('b'), Some((56, false)));
        assert_eq!(ascii_key('A'), Some((38, true)));
        assert_eq!(ascii_key('q'), Some((24, false)));
        assert_eq!(ascii_key('1'), Some((10, false)));
        assert_eq!(ascii_key('!'), Some((10, true)));
        assert_eq!(ascii_key(' '), Some((65, false)));
        assert_eq!(ascii_key('\n'), Some((36, false)));
        assert_eq!(ascii_key('é'), None);
    }

    #[test]
    fn errors_have_stable_kinds() {
        let missing = SeatInputError::ProtocolUnavailable {
            display: "wayland-1".to_owned(),
            protocol: VIRTUAL_KEYBOARD_PROTOCOL.to_owned(),
        };
        assert_eq!(missing.kind(), "protocol_unavailable");
        assert!(matches!(
            missing.into_multiseat(),
            MultiseatError::SeatInput(_)
        ));
        assert_eq!(
            SeatInputError::CompositorRefused {
                display: "wayland-1".to_owned(),
                message: "denied".to_owned(),
            }
            .kind(),
            "compositor_refused"
        );
        assert_eq!(
            SeatInputError::Disconnected {
                display: "wayland-1".to_owned(),
                message: "gone".to_owned(),
            }
            .kind(),
            "disconnected"
        );
    }

    #[cfg(unix)]
    #[test]
    fn built_in_keymap_is_a_complete_xkb_keymap() {
        assert!(super::imp::US_KEYMAP.contains("xkb_keymap {"));
        assert!(super::imp::US_KEYMAP.contains("xkb_keycodes"));
        assert!(super::imp::US_KEYMAP.contains("xkb_symbols"));
        assert!(super::imp::US_KEYMAP.contains("<AC01>"));
    }
}
