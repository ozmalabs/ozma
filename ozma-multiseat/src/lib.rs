// SPDX-License-Identifier: AGPL-3.0-only
//! `ozma-multiseat` — planning and topology foundation for multiple desktop
//! seats on one host.
//!
//! The Python agent's `agent/multiseat` package is the behavioral baseline.
//! This crate ports the platform-neutral planning layer:
//!
//! * [`display`] enumerates outputs through Xrandr and exposes a side-effect
//!   free DRM connector detector in [`drm`]. [`drm_lease`] is the opt-in
//!   lease-only handoff: it validates connector/CRTC object ids and returns an
//!   owned lessee fd, but creates no scanout or compositor isolation.
//!   [`drm_scanout`] is the opt-in KMS proof layered on that lease: it fills one
//!   or more rotating dumb buffers, programs one connector/CRTC/plane with an
//!   atomic commit (or legacy `set_crtc` only when atomic KMS is unsupported),
//!   verifies the modeset, and can run a bounded page-flip loop with a moving
//!   damage rectangle before tearing it down. It is not a compositor.
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
//! * [`input_isolation`] is the real per-seat grab path: a typed line/JSON
//!   protocol, a testable helper state machine with path/duplicate/cross-seat
//!   validation, and a non-privileged client for the `ozma-seat-input` helper.
//!   The helper binary, not this library, opens `/dev/input` and `/dev/uinput`.
//! * [`input_targeting`] restricts a private X display to one seat's
//!   helper-created uinput devices through `xinput`, with an injectable
//!   targeter and a bounded real command runner. Shared displays are a no-op.
//! * [`ownership`] models per-seat display ownership and builds
//!   `xrandr --output ... --mode ... --pos ...` commands. On one X server this
//!   is output positioning, not screen isolation.
//! * [`seat_display`] is the opt-in per-seat X server backend: `shared`
//!   (default), private `Xvfb`, or nested `Xephyr`, with free display-number
//!   allocation, an `xauth` cookie, a real X11 readiness handshake, and typed
//!   missing-binary/exhaustion/timeout/crash errors. A private server starts
//!   before the seat and stops after it; meaningless xrandr/xinput operations
//!   on a private root are reported as typed notes.
//! * [`supervisor`] starts, health-checks, restarts, stops, and reports real
//!   `ozma-node --profile desktop` children. [`fake`] contains the test-only
//!   launcher, readiness probe, backends, command runner, display spawner, and
//!   grab fake.
//! * [`persistence`] is a versioned, atomically written seat store. It keeps
//!   explicit ids/displays/hubs/ports/audio sinks stable across replans and
//!   retains missing displays without resurrecting them.
//! * [`hotplug`] debounces display/input-hub events and reconciles plan and
//!   supervisor start/stop, with a fake and a read-only sysfs model.
//! * [`audio`] is the synchronous per-seat null-sink seam over
//!   [`ozma_audio::AudioRouter`]; it is only invoked when a router is installed.
//!
//! The library performs no input injection, opens no evdev node, creates no
//! virtual display, creates no DRM lease, and allocates no PipeWire sink by
//! default. Routing, ownership, isolation, audio, display-server, and DRM
//! lease commands are only executed through explicitly injected
//! runners/controllers/helpers/backends; the default evdev grab backend returns
//! a typed unsupported error. The library contains no `unsafe` code (the Unix
//! SIGTERM helper is the safe `nix` API); only the `ozma-seat-input` binary's
//! `ioctl` module uses `unsafe`, to set a grabbed fd non-blocking.
//!
//! Test fakes in [`fake`] never touch the host. Production code must not select
//! them from detection.

#![forbid(unsafe_code)]

pub mod audio;
pub mod display;
pub mod drm;
pub mod drm_lease;
pub mod drm_scanout;
pub mod error;
pub mod fake;
pub mod host;
pub mod hotplug;
pub mod input;
pub mod input_isolation;
pub mod input_targeting;
pub mod ownership;
pub mod persistence;
pub mod planner;
pub mod routing;
pub mod seat;
pub mod seat_display;
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
#[cfg(target_os = "linux")]
pub use drm_lease::RealDrmLeaseBackend;
pub use drm_lease::{
    apply_lease_environment, clear_lease_environment, enumerate as enumerate_drm_cards,
    enumerate_at as enumerate_drm_cards_at, DrmCard, DrmCardSelector, DrmCrtcSelector,
    DrmLeaseBackend, DrmLeaseConnector, DrmLeaseError, DrmLeaseErrorStatus, DrmLeaseHandle,
    DrmLeaseNote, DrmLeaseNoteKind, DrmLeaseObjects, DrmLeaseRequest, DrmLeaseStatus,
    DrmObjectKind, DrmPlaneSelector, DEFAULT_DRM_DEV_ROOT, DRM_LEASE_CARD_ENV,
    DRM_LEASE_CONNECTOR_ENV, DRM_LEASE_CONNECTOR_ID_ENV, DRM_LEASE_CRTC_ID_ENV, DRM_LEASE_ENV,
    DRM_LEASE_FD_ENV, DRM_LEASE_ID_ENV, DRM_LEASE_PLANE_ID_ENV, VIRTUAL_DRM_DRIVER,
};
#[cfg(target_os = "linux")]
pub use drm_scanout::RealDrmScanoutBackend;
pub use drm_scanout::{
    apply_frame_damage, ensure_scanout_card, fill_pattern, frame_buffer_count, frame_damage_rect,
    pattern_checksum, DrmScanoutBackend, DrmScanoutCommit, DrmScanoutError, DrmScanoutErrorStatus,
    DrmScanoutFramePlan, DrmScanoutFrameStats, DrmScanoutHandle, DrmScanoutNote,
    DrmScanoutNoteKind, DrmScanoutRequest, DrmScanoutStatus, DrmScanoutVerification, SCANOUT_BPP,
    SCANOUT_DAMAGE_PATTERN, SCANOUT_DEFAULT_FRAMES, SCANOUT_DEPTH, SCANOUT_FLIP_TIMEOUT_MS,
    SCANOUT_FOURCC, SCANOUT_MAX_FRAMES, SCANOUT_PATTERN,
};
pub use error::MultiseatError;
pub use fake::{
    FakeCommandRunner, FakeDeviceNameResolver, FakeDisplayBackend, FakeDrmLeaseBackend,
    FakeDrmLeaseCard, FakeDrmLeaseFailure, FakeDrmScanoutBackend, FakeDrmScanoutCard,
    FakeDrmScanoutFailure, FakeDrmScanoutFlipFailure, FakeEvdevGrabBackend, FakeGrabAction,
    FakeGrabEvent, FakeHelperBackend, FakeHotplugMonitor, FakeInputBackend, FakeLeaseRecord,
    FakeNodeLauncher, FakeReadinessProbe, FakeSeatAudioRouter, FakeSeatDisplayLauncher,
    FakeSeatInputHelper, FakeSeatInputTargeter, FakeSeatSupervisor,
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
pub use input_isolation::{
    decode_request, decode_response, default_uinput_name, encode_request, encode_response,
    helper_devices, is_uinput_name, uinput_kind_token, validate_device_path, HelperBackend,
    HelperBackendKind, HelperClient, HelperClientConfig, HelperDevice, HelperError,
    HelperErrorCode, HelperRequest, HelperResponse, HelperSeatStatus, InputIsolationAssignment,
    InputIsolationHelper, InputIsolationRelease, SeatInputHelper, DEVICE_PATH_PREFIX,
    HELPER_PROGRAM, HELPER_PROTOCOL_VERSION, INPUT_ISOLATION_ROUTER_NAME, UINPUT_NAME_PREFIX,
};
pub use input_targeting::{
    BoundedProcessCommandRunner, InputTargetingMode, ProcessSeatInputTargeter,
    SeatInputTargetRequest, SeatInputTargeter, SeatInputTargeting, SeatInputTargetingRelease,
    DEFAULT_INPUT_TARGET_TIMEOUT, TARGETING_PROGRAM,
};
pub use ownership::{
    apply_ownership, DisplayOwnership, DisplayOwnershipController, DisplayOwnershipKind,
    XrandrOwnershipController, OZMA_DISPLAY_OWNERSHIP_ENV, OZMA_X11_MODE_ENV, OZMA_X11_OUTPUT_ENV,
    SINGLE_X_SERVER_LIMITATION, XRANDR_PROGRAM,
};
pub use persistence::{
    default_seat_store_path, PersistedDisplay, PersistedSeat, PersistedSeatStatus, SeatStore,
    SeatStoreStatus, DEFAULT_SEAT_STORE_NAME, SEAT_STORE_VERSION,
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
pub use seat_display::{
    display_error_kind, display_name, display_server_args, parse_display_number,
    probe_wayland_display, probe_x11_display, resolve_program, wayland_display_name, weston_config,
    weston_server_args, x11_setup_request, DisplayExit, DisplayNumberAllocator,
    ProcessSeatDisplayLauncher, SeatDisplayBackend, SeatDisplayError, SeatDisplayHandle,
    SeatDisplayLauncher, SeatDisplayNote, SeatDisplayNoteKind, SeatDisplayOptions,
    SeatDisplayPrograms, SeatDisplayRequest, SeatDisplaySize, SeatDisplayStatus, SeatWaylandEnv,
    DEFAULT_DISPLAY_NUMBER_END, DEFAULT_DISPLAY_NUMBER_START, DEFAULT_DISPLAY_READY_POLL,
    DEFAULT_DISPLAY_READY_TIMEOUT, DEFAULT_SEAT_DISPLAY_HEIGHT, DEFAULT_SEAT_DISPLAY_WIDTH,
    DISPLAY_TERMINATE_GRACE,
};
pub use status::{render_metrics, MultiseatCapability, MultiseatStatus};
pub use supervisor::{
    restart_delay, ChildExit, ChildHandle, HttpHealthProbe, NodeLaunchSpec, NodeLauncher,
    PortProbe, ProcessLauncher, ProcessSeatSupervisor, ReadinessProbe, SeatSupervisor,
    SupervisorOptions, DEFAULT_HEALTHY_UPTIME, DEFAULT_HEALTH_PATH, DEFAULT_NODE_BIN,
    DEFAULT_READINESS_POLL, DEFAULT_READINESS_TIMEOUT, DEFAULT_RESTART_BACKOFF,
    DEFAULT_RESTART_BACKOFF_MAX, TERMINATE_GRACE,
};
