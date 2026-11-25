// SPDX-License-Identifier: AGPL-3.0-only
//! Seat configuration, profiles, launch commands, and runtime state.

use std::collections::BTreeMap;
use std::fmt;
use std::str::FromStr;

use serde::{Deserialize, Serialize};

use crate::display::DisplayOutput;
use crate::drm_lease::DrmLeaseStatus;
use crate::error::MultiseatError;
use crate::input::InputGroup;
use crate::ownership::DisplayOwnership;
use crate::routing::InputRoutePlan;
use crate::seat_display::SeatDisplayStatus;

/// The built-in per-seat profiles from the Python baseline.
///
/// The node profile is always `desktop`; the seat profile controls capture
/// defaults and the advertised profile label.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatProfile {
    /// Full desktop session, 30 fps.
    #[default]
    Desktop,
    /// Low-latency game library, 60 fps.
    Gaming,
    /// Balanced workstation, 15 fps.
    Workstation,
    /// Media player, 30 fps with surround audio.
    Media,
    /// Locked-down single app, 10 fps at 1280x720.
    Kiosk,
    /// Digital signage browser, 10 fps.
    Signage,
    /// Operator-supplied command, 30 fps.
    Custom,
}

impl SeatProfile {
    /// Every built-in profile, in declaration order.
    pub const ALL: [Self; 7] = [
        Self::Desktop,
        Self::Gaming,
        Self::Workstation,
        Self::Media,
        Self::Kiosk,
        Self::Signage,
        Self::Custom,
    ];

    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Desktop => "desktop",
            Self::Gaming => "gaming",
            Self::Workstation => "workstation",
            Self::Media => "media",
            Self::Kiosk => "kiosk",
            Self::Signage => "signage",
            Self::Custom => "custom",
        }
    }

    /// Capture and audio defaults for this profile.
    pub fn defaults(self) -> ProfileDefaults {
        match self {
            Self::Desktop => ProfileDefaults::new(1920, 1080, 30, 2, true),
            Self::Gaming => ProfileDefaults::new(1920, 1080, 60, 2, true),
            Self::Workstation => ProfileDefaults::new(1920, 1080, 15, 2, false),
            Self::Media => ProfileDefaults::new(1920, 1080, 30, 6, false),
            Self::Kiosk => ProfileDefaults::new(1280, 720, 10, 2, false),
            Self::Signage => ProfileDefaults::new(1920, 1080, 10, 2, false),
            Self::Custom => ProfileDefaults::new(1920, 1080, 30, 2, true),
        }
    }
}

impl fmt::Display for SeatProfile {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl FromStr for SeatProfile {
    type Err = MultiseatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let normalized = value.trim().to_ascii_lowercase().replace('-', "_");
        match normalized.as_str() {
            "desktop" => Ok(Self::Desktop),
            "gaming" => Ok(Self::Gaming),
            "workstation" => Ok(Self::Workstation),
            "media" => Ok(Self::Media),
            "kiosk" => Ok(Self::Kiosk),
            "signage" => Ok(Self::Signage),
            "custom" => Ok(Self::Custom),
            _ => Err(MultiseatError::UnknownProfile(value.trim().to_owned())),
        }
    }
}

/// Capture/audio defaults attached to a [`SeatProfile`].
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProfileDefaults {
    /// Requested capture width.
    pub capture_width: u32,
    /// Requested capture height.
    pub capture_height: u32,
    /// Requested capture frame rate.
    pub capture_fps: u32,
    /// Audio channels (2 stereo, 6 surround).
    pub audio_channels: u16,
    /// Whether the profile prefers low latency.
    pub low_latency: bool,
}

impl ProfileDefaults {
    const fn new(
        capture_width: u32,
        capture_height: u32,
        capture_fps: u32,
        audio_channels: u16,
        low_latency: bool,
    ) -> Self {
        Self {
            capture_width,
            capture_height,
            capture_fps,
            audio_channels,
            low_latency,
        }
    }
}

/// How a seat's screen is captured.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum CapturePlan {
    /// X11 `x11grab` for an explicit display and output region.
    #[serde(rename = "x11")]
    X11 {
        /// X display, for example `:0`.
        display: String,
        /// X offset of the output within the root screen.
        offset_x: i32,
        /// Y offset of the output within the root screen.
        offset_y: i32,
        /// Capture width.
        width: u32,
        /// Capture height.
        height: u32,
        /// Capture frame rate.
        fps: u32,
    },
    /// PipeWire `Video/Source` target. The target is configured, never
    /// synthesised from a guessed node name.
    #[serde(rename = "pipewire")]
    PipeWire {
        /// `default`, `name:<node>`, `object:<node>`, or `node:<id>`.
        target: String,
        /// Requested capture width.
        width: u32,
        /// Requested capture height.
        height: u32,
        /// Requested capture frame rate.
        fps: u32,
    },
}

impl CapturePlan {
    /// `ozma-node` capture provider name.
    pub fn provider(&self) -> &'static str {
        match self {
            Self::X11 { .. } => "x11",
            Self::PipeWire { .. } => "pipewire",
        }
    }

    /// Capture frame rate.
    pub fn fps(&self) -> u32 {
        match self {
            Self::X11 { fps, .. } | Self::PipeWire { fps, .. } => *fps,
        }
    }

    /// Capture width.
    pub fn width(&self) -> u32 {
        match self {
            Self::X11 { width, .. } | Self::PipeWire { width, .. } => *width,
        }
    }

    /// Capture height.
    pub fn height(&self) -> u32 {
        match self {
            Self::X11 { height, .. } | Self::PipeWire { height, .. } => *height,
        }
    }

    /// Arguments understood by `ozma-node --profile desktop`.
    pub fn node_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        match self {
            Self::X11 {
                display,
                offset_x,
                offset_y,
                width,
                height,
                fps,
            } => {
                push_pair(&mut args, "--capture-provider", "x11");
                push_pair(&mut args, "--x11-display", display);
                push_pair(&mut args, "--x11-offset", format!("{offset_x},{offset_y}"));
                push_pair(&mut args, "--x11-width", width.to_string());
                push_pair(&mut args, "--x11-height", height.to_string());
                push_pair(&mut args, "--x11-fps", fps.to_string());
            }
            Self::PipeWire {
                target,
                width,
                height,
                fps,
            } => {
                push_pair(&mut args, "--capture-provider", "pipewire");
                push_pair(&mut args, "--desktop-capture-target", target);
                push_pair(&mut args, "--desktop-capture-width", width.to_string());
                push_pair(&mut args, "--desktop-capture-height", height.to_string());
                push_pair(&mut args, "--desktop-capture-fps", fps.to_string());
            }
        }
        args
    }

    /// Environment that helps a seat-local application use the same display.
    pub fn environment(&self) -> BTreeMap<String, String> {
        let mut env = BTreeMap::new();
        if let Self::X11 { display, .. } = self {
            env.insert("DISPLAY".to_owned(), display.clone());
        }
        env
    }
}

/// A concrete seat configuration resolved by the planner.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatConfig {
    /// Stable seat id, also used as the controller-visible node name.
    pub id: String,
    /// Index of the display assigned to this seat.
    pub display_index: u32,
    /// Display output/connector name, for example `HDMI-1`.
    pub display_output: String,
    /// Physical input device paths assigned to the seat. Empty means no
    /// keyboard/mouse topology was available.
    #[serde(default)]
    pub input_devices: Vec<String>,
    /// Primary USB hub key for the assigned input group.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input_hub: Option<String>,
    /// Audio sink name advertised to the controller.
    pub audio_sink: String,
    /// UDP port for HID packets.
    pub udp_port: u16,
    /// HTTP port for the node API.
    pub http_port: u16,
    /// Seat profile.
    pub profile: SeatProfile,
    /// Capture transport and geometry.
    pub capture: CapturePlan,
}

/// Ports assigned to one seat.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatPorts {
    /// UDP HID port.
    pub udp: u16,
    /// HTTP API port.
    pub http: u16,
}

/// Program, arguments, and environment for one planned node process.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NodeCommand {
    /// Binary to launch, normally `ozma-node`.
    pub program: String,
    /// Arguments after the program name.
    pub args: Vec<String>,
    /// Extra environment for the process and its seat-local applications.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
}

impl NodeCommand {
    /// Full argv, including the program as element zero.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

/// A resolved, launchable seat plan.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatPlan {
    /// Concrete seat configuration.
    pub config: SeatConfig,
    /// Controller-visible node name (currently equal to the seat id).
    pub node_name: String,
    /// Display assigned to this seat.
    pub display: DisplayOutput,
    /// Assigned input group, or `None` when no input topology was available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<InputGroup>,
    /// Planned display ownership. Present for X11 seats; `None` for PipeWire
    /// capture, where the compositor owns outputs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<DisplayOwnership>,
    /// Planned physical input routing intent. Present when the seat has an
    /// input group and an X11 display.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_route: Option<InputRoutePlan>,
    /// Deterministic node launch command.
    pub command: NodeCommand,
}

impl SeatPlan {
    /// Full `ozma-node` argv, including the program.
    pub fn node_argv(&self) -> Vec<String> {
        self.command.argv()
    }

    /// Assigned ports.
    pub fn ports(&self) -> SeatPorts {
        SeatPorts {
            udp: self.config.udp_port,
            http: self.config.http_port,
        }
    }

    /// Current status with a caller-supplied runtime state.
    pub fn status(&self, state: SeatState, message: Option<String>) -> SeatStatus {
        SeatStatus {
            seat_id: self.config.id.clone(),
            node_name: self.node_name.clone(),
            display_index: self.config.display_index,
            display_output: self.config.display_output.clone(),
            udp_port: self.config.udp_port,
            http_port: self.config.http_port,
            audio_sink: self.config.audio_sink.clone(),
            profile: self.config.profile,
            capture: self.config.capture.clone(),
            input_devices: self.config.input_devices.clone(),
            ownership: self.ownership.clone(),
            input_route: self.input_route.clone(),
            state,
            pid: None,
            restarts: 0,
            last_error: None,
            next_restart_in_ms: None,
            resources: SeatResourceStatus::default(),
            display: SeatDisplayStatus::shared(),
            drm_lease: DrmLeaseStatus::default(),
            message,
        }
    }
}

/// Lifecycle state reported by a supervisor.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SeatState {
    /// Planned, not started.
    #[default]
    Planned,
    /// Start requested.
    Starting,
    /// Node process is running.
    Running,
    /// Stopped cleanly.
    Stopped,
    /// Start or runtime failure.
    Failed,
}

impl SeatState {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Planned => "planned",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }
}

/// Which optional seat resource an action or error refers to.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceKind {
    /// Per-seat `xrandr` output ownership.
    Ownership,
    /// Per-seat `xinput`/evdev input routing.
    Routing,
    /// Per-seat privileged input isolation (`EVIOCGRAB` + uinput helper).
    InputIsolation,
    /// Restricting a private X display to the seat's helper uinput devices.
    InputTargeting,
    /// Synthetic per-seat Wayland keyboard/pointer injection.
    SeatInput,
    /// Per-seat PipeWire/PulseAudio null sink.
    Audio,
    /// Per-seat DRM lease (connector/CRTC object handoff only).
    DrmLease,
}

impl ResourceKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Ownership => "ownership",
            Self::Routing => "routing",
            Self::InputIsolation => "input_isolation",
            Self::InputTargeting => "input_targeting",
            Self::SeatInput => "seat_input",
            Self::Audio => "audio",
            Self::DrmLease => "drm_lease",
        }
    }
}

/// Whether a resource operation was an apply or a release.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceAction {
    /// Reserve/apply the resource before spawning a child.
    Apply,
    /// Release the resource on stop, rollback, or shutdown.
    Release,
}

impl ResourceAction {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Apply => "apply",
            Self::Release => "release",
        }
    }
}

/// One typed resource apply/release failure.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatResourceError {
    /// Resource kind.
    pub kind: ResourceKind,
    /// Action that failed.
    pub action: ResourceAction,
    /// Error text from the controller, preserving the typed
    /// `MultiseatError` message (missing tool, non-zero command, unsupported).
    pub message: String,
}

/// Applied/released state for the optional ownership and routing resources of
/// one seat.
///
/// The field is omitted from status JSON when empty, so supervisor status with
/// both opt-in flags off is byte-for-byte compatible with the earlier shape.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatResourceStatus {
    /// Ownership was applied successfully and has not been released.
    #[serde(default)]
    pub ownership_applied: bool,
    /// Routing was applied successfully and has not been released.
    #[serde(default)]
    pub routing_applied: bool,
    /// A per-seat audio sink was ensured and has not been removed.
    #[serde(default)]
    pub audio_applied: bool,
    /// The privileged input helper confirmed the seat's exclusive grabs and
    /// uinput devices, and they have not been released.
    #[serde(default)]
    pub input_isolation_applied: bool,
    /// The seat's private X display was restricted to its own uinput devices
    /// and that restriction has not been released.
    #[serde(default)]
    pub input_targeting_applied: bool,
    /// A per-seat Wayland virtual keyboard/pointer connection is open.
    #[serde(default)]
    pub input_injection_applied: bool,
    /// The compositor advertised the virtual-input protocol globals.
    #[serde(default)]
    pub input_injection_available: bool,
    /// Synthetic input events successfully queued for this seat.
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub input_injection_events: u64,
    /// A DRM lease was acquired for this seat and has not been released.
    #[serde(default)]
    pub drm_lease_applied: bool,
    /// Ownership was released after being applied (or a previous release was
    /// retried successfully).
    #[serde(default)]
    pub ownership_released: bool,
    /// Routing was released after being applied.
    #[serde(default)]
    pub routing_released: bool,
    /// Audio sink was removed after being ensured.
    #[serde(default)]
    pub audio_released: bool,
    /// Input isolation was released after being applied.
    #[serde(default)]
    pub input_isolation_released: bool,
    /// Private-display input targeting was released after being applied.
    #[serde(default)]
    pub input_targeting_released: bool,
    /// Synthetic Wayland input was disconnected after being applied.
    #[serde(default)]
    pub input_injection_released: bool,
    /// A DRM lease was released after being applied.
    #[serde(default)]
    pub drm_lease_released: bool,
    /// Devices the helper confirmed as exclusively grabbed for this seat.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub grabbed_device_count: usize,
    /// uinput device names the helper confirmed for this seat.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub uinput_devices: Vec<String>,
    /// The subset of `uinput_devices` enabled on the seat's private display.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub targeted_devices: Vec<String>,
    /// Other seats' uinput device names disabled on the private display.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub disabled_devices: Vec<String>,
    /// Apply/release errors, in occurrence order. Best-effort release failures
    /// are reported here without hiding the seat's lifecycle state.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub errors: Vec<SeatResourceError>,
}

impl SeatResourceStatus {
    /// Whether no resource was applied/released and no error was recorded.
    pub fn is_empty(&self) -> bool {
        !self.ownership_applied
            && !self.routing_applied
            && !self.audio_applied
            && !self.input_isolation_applied
            && !self.input_targeting_applied
            && !self.input_injection_applied
            && !self.input_injection_available
            && !self.drm_lease_applied
            && !self.ownership_released
            && !self.routing_released
            && !self.audio_released
            && !self.input_isolation_released
            && !self.input_targeting_released
            && !self.input_injection_released
            && !self.drm_lease_released
            && self.grabbed_device_count == 0
            && self.input_injection_events == 0
            && self.uinput_devices.is_empty()
            && self.targeted_devices.is_empty()
            && self.disabled_devices.is_empty()
            && self.errors.is_empty()
    }

    /// Record one typed apply/release failure.
    pub fn push_error(
        &mut self,
        kind: ResourceKind,
        action: ResourceAction,
        message: impl Into<String>,
    ) {
        self.errors.push(SeatResourceError {
            kind,
            action,
            message: message.into(),
        });
    }
}

fn is_zero(value: &usize) -> bool {
    *value == 0
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

/// State transition reported by a [`crate::supervisor::SeatSupervisor`].
///
/// The state fields are a superset of the original `state`/`message` pair so a
/// subprocess supervisor can report the child pid, planned ports, restart
/// count, and the most recent failure without a second status document. They
/// are optional/defaulted, so status documents written by the planner or the
/// test fake remain valid.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatRuntimeStatus {
    /// Seat id the status belongs to.
    pub seat_id: String,
    /// Current state.
    pub state: SeatState,
    /// Child process id, when a live child exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Planned UDP HID port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udp_port: Option<u16>,
    /// Planned HTTP API port.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub http_port: Option<u16>,
    /// Successful (re)starts after the initial spawn.
    #[serde(default)]
    pub restarts: u32,
    /// Most recent spawn, readiness, or exit error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Milliseconds until the next scheduled restart, when one is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_in_ms: Option<u64>,
    /// Applied/released ownership and routing state, with typed errors.
    #[serde(default, skip_serializing_if = "SeatResourceStatus::is_empty")]
    pub resources: SeatResourceStatus,
    /// Private per-seat X server state. Flattened into the status document and
    /// omitted entirely for the default `shared` backend.
    #[serde(default, flatten)]
    pub display: SeatDisplayStatus,
    /// Opt-in DRM lease state. Omitted when no lease was requested.
    #[serde(default, skip_serializing_if = "DrmLeaseStatus::is_empty")]
    pub drm_lease: DrmLeaseStatus,
    /// Optional operator-facing message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

impl SeatRuntimeStatus {
    /// A status with no message.
    pub fn new(seat_id: impl Into<String>, state: SeatState) -> Self {
        Self {
            seat_id: seat_id.into(),
            state,
            pid: None,
            udp_port: None,
            http_port: None,
            restarts: 0,
            last_error: None,
            next_restart_in_ms: None,
            resources: SeatResourceStatus::default(),
            display: SeatDisplayStatus::shared(),
            drm_lease: DrmLeaseStatus::default(),
            message: None,
        }
    }

    /// Attach a message.
    pub fn with_message(mut self, message: impl Into<String>) -> Self {
        self.message = Some(message.into());
        self
    }

    /// Attach process and port details.
    pub fn with_process(mut self, pid: Option<u32>, udp_port: u16, http_port: u16) -> Self {
        self.pid = pid;
        self.udp_port = Some(udp_port);
        self.http_port = Some(http_port);
        self
    }

    /// Attach the restart count.
    pub fn with_restarts(mut self, restarts: u32) -> Self {
        self.restarts = restarts;
        self
    }

    /// Attach the most recent failure.
    pub fn with_last_error(mut self, error: impl Into<String>) -> Self {
        self.last_error = Some(error.into());
        self
    }

    /// Attach the pending restart delay.
    pub fn with_next_restart_in_ms(mut self, delay_ms: u64) -> Self {
        self.next_restart_in_ms = Some(delay_ms);
        self
    }

    /// Attach applied/released resource state.
    pub fn with_resources(mut self, resources: SeatResourceStatus) -> Self {
        self.resources = resources;
        self
    }

    /// Attach private display server state.
    pub fn with_display(mut self, display: SeatDisplayStatus) -> Self {
        self.display = display;
        self
    }

    /// Attach DRM lease state.
    pub fn with_drm_lease(mut self, drm_lease: DrmLeaseStatus) -> Self {
        self.drm_lease = drm_lease;
        self
    }
}

/// Controller-facing status for one seat.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatStatus {
    /// Seat id.
    pub seat_id: String,
    /// Controller-visible node name.
    pub node_name: String,
    /// Display index.
    pub display_index: u32,
    /// Display output name.
    pub display_output: String,
    /// UDP HID port.
    pub udp_port: u16,
    /// HTTP API port.
    pub http_port: u16,
    /// Audio sink name.
    pub audio_sink: String,
    /// Seat profile.
    pub profile: SeatProfile,
    /// Capture plan.
    pub capture: CapturePlan,
    /// Assigned input device paths.
    #[serde(default)]
    pub input_devices: Vec<String>,
    /// Planned display ownership, when the seat owns an X11 output.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ownership: Option<DisplayOwnership>,
    /// Planned input routing intent, when the seat has an input group.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_route: Option<InputRoutePlan>,
    /// Lifecycle state.
    pub state: SeatState,
    /// Child process id, when a live child exists.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Successful (re)starts after the initial spawn.
    #[serde(default)]
    pub restarts: u32,
    /// Most recent spawn, readiness, or exit error.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_error: Option<String>,
    /// Milliseconds until the next scheduled restart, when one is pending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub next_restart_in_ms: Option<u64>,
    /// Applied/released ownership and routing state, with typed errors.
    #[serde(default, skip_serializing_if = "SeatResourceStatus::is_empty")]
    pub resources: SeatResourceStatus,
    /// Private per-seat X server state. Flattened into status and omitted for
    /// the default `shared` backend.
    #[serde(default, flatten)]
    pub display: SeatDisplayStatus,
    /// Opt-in DRM lease state. Omitted when no lease was requested.
    #[serde(default, skip_serializing_if = "DrmLeaseStatus::is_empty")]
    pub drm_lease: DrmLeaseStatus,
    /// Optional state message.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
}

/// Machine-readable reason attached to a plan.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DiagnosticCode {
    /// Fewer displays were available than seats requested.
    RequestedSeatsCapped,
    /// A virtual display was created to reach the requested seat count.
    VirtualDisplayCreated,
    /// A virtual display was requested but the backend could not create one.
    VirtualDisplayUnavailable,
    /// The input backend returned no devices.
    NoInputDevices,
    /// A seat was planned without a keyboard/mouse group.
    SeatWithoutInput,
    /// An input group had no seat left to assign it to.
    UnassignedInputGroup,
    /// A persisted seat's display is no longer present. The seat is retained in
    /// the store but is not planned/started.
    PersistedSeatMissingDisplay,
    /// A persisted seat's input hub or device path is no longer present.
    PersistedSeatMissingInput,
    /// A persisted seat conflicts with another persisted seat or with the
    /// discovered topology.
    PersistedSeatConflict,
    /// Hotplug reconciliation could not plan or apply the current topology.
    HotplugReconcileFailed,
    /// A hotplug start/stop action failed; the supervisor state is unchanged or
    /// keeps its existing retry path.
    HotplugSeatActionFailed,
}

/// One non-fatal planning diagnostic.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanDiagnostic {
    /// Stable diagnostic code.
    pub code: DiagnosticCode,
    /// Human-readable explanation.
    pub message: String,
    /// Seat affected, when the diagnostic is seat-specific.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub seat_id: Option<String>,
}

impl PlanDiagnostic {
    /// A plan-wide diagnostic.
    pub fn new(code: DiagnosticCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
            seat_id: None,
        }
    }

    /// A seat-specific diagnostic.
    pub fn for_seat(
        code: DiagnosticCode,
        seat_id: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            code,
            message: message.into(),
            seat_id: Some(seat_id.into()),
        }
    }
}

/// The complete result of planning a multi-seat host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiseatPlan {
    /// One plan per seat. Empty is never produced: the planner returns
    /// [`MultiseatError::NoDisplays`] instead.
    pub seats: Vec<SeatPlan>,
    /// Requested seat count before display caps.
    pub requested_seats: usize,
    /// Display backend that supplied the outputs.
    pub display_backend: String,
    /// Input backend that supplied the devices.
    pub input_backend: String,
    /// Non-fatal diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<PlanDiagnostic>,
}

impl MultiseatPlan {
    /// Number of planned seats.
    pub fn seat_count(&self) -> usize {
        self.seats.len()
    }

    /// Number of controller-visible nodes. One seat is one node.
    pub fn node_count(&self) -> usize {
        self.seats.len()
    }

    /// Provider names used by the planned seats, sorted and deduplicated.
    pub fn capture_providers(&self) -> Vec<String> {
        let mut providers: Vec<String> = self
            .seats
            .iter()
            .map(|seat| seat.config.capture.provider().to_owned())
            .collect();
        providers.sort();
        providers.dedup();
        providers
    }
}

fn push_pair(args: &mut Vec<String>, flag: &str, value: impl Into<String>) {
    args.push(flag.to_owned());
    args.push(value.into());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profiles_parse_case_insensitively() {
        assert_eq!(
            "GAMING".parse::<SeatProfile>().expect("gaming"),
            SeatProfile::Gaming
        );
        assert!(matches!(
            "work-station".parse::<SeatProfile>(),
            Err(MultiseatError::UnknownProfile(value)) if value == "work-station"
        ));
        assert_eq!(
            "workstation".parse::<SeatProfile>().expect("workstation"),
            SeatProfile::Workstation
        );
    }

    #[test]
    fn x11_capture_argv_is_stable() {
        let capture = CapturePlan::X11 {
            display: ":0".to_owned(),
            offset_x: 1920,
            offset_y: 0,
            width: 2560,
            height: 1440,
            fps: 30,
        };
        assert_eq!(
            capture.node_args(),
            vec![
                "--capture-provider",
                "x11",
                "--x11-display",
                ":0",
                "--x11-offset",
                "1920,0",
                "--x11-width",
                "2560",
                "--x11-height",
                "1440",
                "--x11-fps",
                "30",
            ]
        );
        assert_eq!(
            capture.environment().get("DISPLAY").map(String::as_str),
            Some(":0")
        );
    }
}
