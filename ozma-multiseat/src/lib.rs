// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-multiseat` — planning and topology foundation for multiple desktop
//! seats on one host.
//!
//! The Python agent's `agent/multiseat` package is the behavioral baseline.
//! This crate ports the platform-neutral planning layer:
//!
//! * [`display`] enumerates outputs through Xrandr and exposes a side-effect
//!   free DRM connector detector in [`drm`].
//! * [`input`] reads evdev capability bits and groups devices by USB hub.
//! * [`planner`] assigns one seat per display, allocates sequential UDP/HTTP
//!   ports, attaches input groups, and emits the per-seat
//!   `ozma-node --profile desktop` argv.
//! * [`seat`] defines profiles, seat configuration, launch commands, and
//!   diagnostics.
//! * [`status`] is the controller-facing capability/status shape where one
//!   seat is one node, plus a bounded Prometheus metrics renderer.
//! * [`host`] joins a display backend and an input backend and refuses to plan
//!   when display enumeration fails or reports no displays.
//! * [`routing`] is the physical input routing contract: `xinput map-to-output`
//!   command construction/execution through an injected runner, sysfs evdev
//!   name resolution, seat assignment/rollback, and an exclusive-`EVIOCGRAB`
//!   model with per-seat uinput environment.
//! * [`ownership`] models per-seat display ownership and builds
//!   `xrandr --output ... --mode ... --pos ...` commands. On one X server this
//!   is output positioning, not screen isolation.
//! * [`supervisor`] starts, health-checks, restarts, stops, and reports real
//!   `ozma-node --profile desktop` children. [`fake`] contains the test-only
//!   launcher, readiness probe, backends, command runner, and grab fake.
//! * [`persistence`] is a versioned, atomically written seat store. It keeps
//!   explicit ids/displays/hubs/ports/audio sinks stable across replans and
//!   retains missing displays without resurrecting them.
//! * [`hotplug`] debounces display/input-hub events and reconciles plan and
//!   supervisor start/stop, with a fake and a read-only sysfs model.
//! * [`audio`] is the synchronous per-seat null-sink seam over
//!   [`ozma_audio::AudioRouter`]; it is only invoked when a router is installed.
//!
//! The crate performs no input injection, opens no evdev node, creates no
//! virtual display, and allocates no PipeWire sink by default. Routing,
//! ownership, and audio commands are only executed through explicitly injected
//! runners/controllers; the default evdev grab backend returns a typed
//! unsupported error. It contains no `unsafe` code (the Unix SIGTERM helper is
//! the safe `nix` API).
//!
//! Test fakes in [`fake`] never touch the host. Production code must not select
//! them from detection.

#![forbid(unsafe_code)]

pub mod audio;
pub mod display;
pub mod drm;
pub mod error;
pub mod fake;
pub mod host;
pub mod hotplug;
pub mod input;
pub mod ownership;
pub mod persistence;
pub mod planner;
pub mod routing;
pub mod seat;
pub mod status;
pub mod supervisor;

pub use audio::{
    OzmaAudioRouter, SeatAudioRouter, SeatAudioSpec, SeatAudioState, SEAT_AUDIO_CHANNELS,
    SEAT_AUDIO_DESCRIPTION_PREFIX, SEAT_AUDIO_RATE,
};
pub use display::{
    parse_listactivemonitors, BackendDetection, DisplayBackend, DisplayOutput, XrandrDisplayBackend,
};
pub use drm::{
    detect as detect_drm, detect_at as detect_drm_at, DrmConnector, DrmDetection, DrmStatus,
    DEFAULT_DRM_ROOT,
};
pub use error::MultiseatError;
pub use fake::{
    FakeCommandRunner, FakeDeviceNameResolver, FakeDisplayBackend, FakeEvdevGrabBackend,
    FakeGrabAction, FakeGrabEvent, FakeHotplugMonitor, FakeInputBackend, FakeNodeLauncher,
    FakeReadinessProbe, FakeSeatAudioRouter, FakeSeatSupervisor,
};
pub use host::HostTopology;
pub use hotplug::{
    HotplugDebounce, HotplugEvent, HotplugMonitor, HotplugOptions, ReconcileReport, SeatReconciler,
    SysfsHotplugMonitor, DEFAULT_DEBOUNCE,
};
pub use input::{
    classify_capabilities, group_inputs, usb_hub_path_from_sysfs, InputBackend, InputDevice,
    InputDeviceKind, InputGroup, SysfsInputBackend, DEFAULT_INPUT_DEV_ROOT,
    DEFAULT_SYSFS_INPUT_ROOT, INTERNAL_HUB,
};
pub use ownership::{
    apply_ownership, DisplayOwnership, DisplayOwnershipController, DisplayOwnershipKind,
    XrandrOwnershipController, OZMA_DISPLAY_OWNERSHIP_ENV, OZMA_X11_MODE_ENV, OZMA_X11_OUTPUT_ENV,
    SINGLE_X_SERVER_LIMITATION, XRANDR_PROGRAM,
};
pub use persistence::{
    default_seat_store_path, PersistedDisplay, PersistedSeat, SeatStore, DEFAULT_SEAT_STORE_NAME,
    SEAT_STORE_VERSION,
};
pub use planner::{
    allocate_ports, CapturePreference, MultiseatPlanner, PersistedPlan, PlannerOptions,
};
pub use routing::{
    assign_seats, parse_xinput_list, route_environment, CommandOutput, CommandRunner,
    DeviceNameResolver, EvdevGrabBackend, EvdevGrabCommand, EvdevGrabRequest, EvdevGrabRouter,
    InputAssignment, InputRelease, InputRoutePlan, InputRouter, InputRoutingStrategy, InputTarget,
    ProcessCommandRunner, RouteCommand, SeatInputAssignment, SysfsDeviceNameResolver,
    UnsupportedEvdevGrabBackend, XInputDevice, XInputMapping, XInputRouter, EVDEV_ROUTER_NAME,
    EVIOCGRAB_REQUEST, OZMA_INPUT_ROUTER_ENV, OZMA_SEAT_ID_ENV, OZMA_UINPUT_SEAT_ENV,
    XINPUT_PROGRAM, XINPUT_ROUTER_NAME,
};
pub use seat::{
    CapturePlan, DiagnosticCode, MultiseatPlan, NodeCommand, PlanDiagnostic, ProfileDefaults,
    ResourceAction, ResourceKind, SeatConfig, SeatPlan, SeatPorts, SeatProfile, SeatResourceError,
    SeatResourceStatus, SeatRuntimeStatus, SeatState, SeatStatus,
};
pub use status::{render_metrics, MultiseatCapability, MultiseatStatus};
pub use supervisor::{
    restart_delay, ChildExit, ChildHandle, HttpHealthProbe, NodeLaunchSpec, NodeLauncher,
    PortProbe, ProcessLauncher, ProcessSeatSupervisor, ReadinessProbe, SeatSupervisor,
    SupervisorOptions, DEFAULT_HEALTHY_UPTIME, DEFAULT_HEALTH_PATH, DEFAULT_NODE_BIN,
    DEFAULT_READINESS_POLL, DEFAULT_READINESS_TIMEOUT, DEFAULT_RESTART_BACKOFF,
    DEFAULT_RESTART_BACKOFF_MAX, TERMINATE_GRACE,
};
