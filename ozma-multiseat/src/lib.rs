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
//!   seat is one node.
//! * [`supervisor`] is the start/stop/status contract. A subprocess launcher is
//!   deliberately not implemented yet; [`fake::FakeSeatSupervisor`] is the test
//!   double.
//!
//! The crate performs no input injection, opens no evdev node, creates no
//! virtual display, allocates no PipeWire sink, and spawns no process. It
//! contains no `unsafe` code.
//!
//! Test fakes in [`fake`] never touch the host. Production code must not select
//! them from detection.

#![forbid(unsafe_code)]

pub mod display;
pub mod drm;
pub mod error;
pub mod fake;
pub mod input;
pub mod planner;
pub mod seat;
pub mod status;
pub mod supervisor;

pub use display::{
    parse_listactivemonitors, BackendDetection, DisplayBackend, DisplayOutput, XrandrDisplayBackend,
};
pub use drm::{
    detect as detect_drm, detect_at as detect_drm_at, DrmConnector, DrmDetection, DrmStatus,
    DEFAULT_DRM_ROOT,
};
pub use error::MultiseatError;
pub use fake::{FakeDisplayBackend, FakeInputBackend, FakeSeatSupervisor};
pub use input::{
    classify_capabilities, group_inputs, usb_hub_path_from_sysfs, InputBackend, InputDevice,
    InputDeviceKind, InputGroup, SysfsInputBackend, DEFAULT_INPUT_DEV_ROOT,
    DEFAULT_SYSFS_INPUT_ROOT, INTERNAL_HUB,
};
pub use planner::{allocate_ports, CapturePreference, MultiseatPlanner, PlannerOptions};
pub use seat::{
    CapturePlan, DiagnosticCode, MultiseatPlan, NodeCommand, PlanDiagnostic, ProfileDefaults,
    SeatConfig, SeatPlan, SeatPorts, SeatProfile, SeatRuntimeStatus, SeatState, SeatStatus,
};
pub use status::{MultiseatCapability, MultiseatStatus};
pub use supervisor::SeatSupervisor;
