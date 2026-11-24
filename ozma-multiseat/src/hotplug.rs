// SPDX-License-Identifier: AGPL-3.0-only
//! Hotplug detection, debounce, and supervisor reconciliation.
//!
//! [`HotplugMonitor`] is the event source: a fake source drives tests, and
//! [`SysfsHotplugMonitor`] compares read-only sysfs snapshots of DRM connectors
//! and input devices. [`HotplugDebounce`] absorbs the burst of add/remove
//! events a USB hub or monitor emits, and cancels a transient add+remove pair.
//! [`SeatReconciler`] replans against the current topology, reuses persisted
//! seats, stops seats whose display disappeared, and starts/restarts seats that
//! appeared or changed.
//!
//! Nothing here grabs input, changes a mode, mounts a device, starts an audio
//! server, or writes the seat store. The caller owns the store file and the
//! supervisor. All topology and action failures are diagnostics or typed
//! errors, never a fabricated running seat.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use crate::display::{DisplayBackend, DisplayOutput};
use crate::drm::{detect_at as detect_drm_at, DrmDetection, DEFAULT_DRM_ROOT};
use crate::error::MultiseatError;
use crate::input::{
    InputDevice, SysfsInputBackend, DEFAULT_INPUT_DEV_ROOT, DEFAULT_SYSFS_INPUT_ROOT, INTERNAL_HUB,
};
use crate::persistence::SeatStore;
use crate::planner::MultiseatPlanner;
use crate::seat::{
    DiagnosticCode, MultiseatPlan, PlanDiagnostic, SeatPlan, SeatRuntimeStatus, SeatState,
};
use crate::supervisor::SeatSupervisor;

/// Default quiet window before a hotplug batch is reconciled.
pub const DEFAULT_DEBOUNCE: Duration = Duration::from_millis(500);

/// One topology change reported by a [`HotplugMonitor`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum HotplugEvent {
    /// A display output appeared or its geometry changed.
    DisplayAdded(DisplayOutput),
    /// A display output disappeared.
    DisplayRemoved {
        /// Output/connector name.
        output: String,
    },
    /// A USB hub (or the internal device group) appeared with its devices.
    InputHubAdded {
        /// Hub key, for example `1-1` or `internal`.
        hub_path: String,
        /// Classified devices on the hub.
        devices: Vec<InputDevice>,
    },
    /// A USB hub (or the internal device group) disappeared.
    InputHubRemoved {
        /// Hub key, for example `1-1` or `internal`.
        hub_path: String,
    },
}

impl HotplugEvent {
    /// Stable, bounded name for logs/tests.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::DisplayAdded(_) => "display-added",
            Self::DisplayRemoved { .. } => "display-removed",
            Self::InputHubAdded { .. } => "input-hub-added",
            Self::InputHubRemoved { .. } => "input-hub-removed",
        }
    }
}

/// Event source for hotplug reconciliation.
pub trait HotplugMonitor: Send {
    /// Return all events since the previous poll. An empty vector is not an
    /// error; backend failure is.
    fn poll(&mut self) -> Result<Vec<HotplugEvent>, MultiseatError>;
}

/// A quiet-window debouncer that coalesces per-display/per-hub changes.
#[derive(Clone, Debug)]
pub struct HotplugDebounce {
    settle: Duration,
    pending: Vec<HotplugEvent>,
    last_event: Option<Instant>,
}

impl HotplugDebounce {
    /// Build a debouncer with an explicit settle/quiet window.
    pub fn new(settle: Duration) -> Self {
        Self {
            settle,
            pending: Vec::new(),
            last_event: None,
        }
    }

    /// The configured quiet window.
    pub fn settle(&self) -> Duration {
        self.settle
    }

    /// Whether events are waiting for the quiet window.
    pub fn has_pending(&self) -> bool {
        self.last_event.is_some()
    }

    /// Add newly polled events and restart the quiet window.
    pub fn ingest(&mut self, events: impl IntoIterator<Item = HotplugEvent>, now: Instant) {
        let mut any = false;
        for event in events {
            self.pending.push(event);
            any = true;
        }
        if any {
            self.last_event = Some(now);
        }
    }

    /// Return the coalesced batch once the quiet window elapsed since the last
    /// event. A transient add+remove pair cancels out.
    pub fn ready(&mut self, now: Instant) -> Vec<HotplugEvent> {
        let Some(last) = self.last_event else {
            return Vec::new();
        };
        if now.saturating_duration_since(last) < self.settle {
            return Vec::new();
        }
        self.last_event = None;
        let pending = std::mem::take(&mut self.pending);
        coalesce_events(pending)
    }
}

fn coalesce_events(events: Vec<HotplugEvent>) -> Vec<HotplugEvent> {
    let mut result: Vec<HotplugEvent> = Vec::new();
    for event in events {
        match &event {
            HotplugEvent::DisplayAdded(display) => {
                // Remove+add in one window leaves the display present; a
                // previous add for the same output is replaced by the newer
                // geometry.
                if let Some(position) = result.iter().position(|existing| {
                    matches!(existing, HotplugEvent::DisplayRemoved { output } if output == &display.name)
                }) {
                    result.remove(position);
                }
                if let Some(position) = result.iter().position(|existing| {
                    matches!(existing, HotplugEvent::DisplayAdded(existing) if existing.name == display.name)
                }) {
                    result.remove(position);
                }
                result.push(event);
            }
            HotplugEvent::DisplayRemoved { output } => {
                if let Some(position) = result.iter().position(|existing| {
                    matches!(existing, HotplugEvent::DisplayAdded(display) if &display.name == output)
                }) {
                    // Add then remove leaves the display absent. Emitting the
                    // removal is a no-op for a display the reconciler never
                    // had, and correctly removes a changed existing display.
                    result[position] = event;
                } else if !result.iter().any(|existing| {
                    matches!(existing, HotplugEvent::DisplayRemoved { output: name } if name == output)
                }) {
                    result.push(event);
                }
            }
            HotplugEvent::InputHubAdded { hub_path, .. } => {
                if let Some(position) = result.iter().position(|existing| {
                    matches!(existing, HotplugEvent::InputHubRemoved { hub_path: name } if name == hub_path)
                }) {
                    result.remove(position);
                }
                if let Some(position) = result.iter().position(|existing| {
                    matches!(existing, HotplugEvent::InputHubAdded { hub_path: name, .. } if name == hub_path)
                }) {
                    result.remove(position);
                }
                result.push(event);
            }
            HotplugEvent::InputHubRemoved { hub_path } => {
                if let Some(position) = result.iter().position(|existing| {
                    matches!(existing, HotplugEvent::InputHubAdded { hub_path: name, .. } if name == hub_path)
                }) {
                    result[position] = event;
                } else if !result.iter().any(|existing| {
                    matches!(existing, HotplugEvent::InputHubRemoved { hub_path: name } if name == hub_path)
                }) {
                    result.push(event);
                }
            }
        }
    }
    result
}

/// Reconciliation policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HotplugOptions {
    /// Quiet window before a batch is applied.
    pub debounce: Duration,
    /// Seat ids that must never be started or stopped by reconciliation.
    pub excluded_seats: BTreeSet<String>,
    /// Display outputs ignored by both events and planning.
    pub excluded_displays: BTreeSet<String>,
    /// Input hub keys ignored by both events and planning.
    pub excluded_hubs: BTreeSet<String>,
}

impl Default for HotplugOptions {
    fn default() -> Self {
        Self {
            debounce: DEFAULT_DEBOUNCE,
            excluded_seats: BTreeSet::new(),
            excluded_displays: BTreeSet::new(),
            excluded_hubs: BTreeSet::new(),
        }
    }
}

impl HotplugOptions {
    /// Exclude a seat id from reconciliation.
    pub fn with_excluded_seat(mut self, seat_id: impl Into<String>) -> Self {
        self.excluded_seats.insert(seat_id.into());
        self
    }

    /// Exclude a display output from hotplug and planning.
    pub fn with_excluded_display(mut self, output: impl Into<String>) -> Self {
        self.excluded_displays.insert(output.into());
        self
    }

    /// Exclude an input hub from hotplug and planning.
    pub fn with_excluded_hub(mut self, hub_path: impl Into<String>) -> Self {
        self.excluded_hubs.insert(hub_path.into());
        self
    }
}

/// Result of one reconciliation pass.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ReconcileReport {
    /// At least one seat was started, stopped, or restarted.
    pub changed: bool,
    /// Seat ids started for the first time.
    pub started: Vec<String>,
    /// Seat ids restarted because their plan changed.
    pub restarted: Vec<String>,
    /// Seat ids stopped because their display disappeared.
    pub stopped: Vec<String>,
    /// Seat ids whose plan was already current.
    pub unchanged: Vec<String>,
    /// Seat ids skipped by policy.
    pub excluded: Vec<String>,
    /// Planning and action diagnostics.
    pub diagnostics: Vec<PlanDiagnostic>,
    /// Current supervisor status after the pass.
    pub statuses: Vec<SeatRuntimeStatus>,
    /// Store after the pass. Missing-display entries are retained.
    pub store: SeatStore,
    /// The plan for the current topology, when planning succeeded.
    pub plan: Option<MultiseatPlan>,
}

impl ReconcileReport {
    fn no_op(store: SeatStore, statuses: Vec<SeatRuntimeStatus>) -> Self {
        Self {
            store,
            statuses,
            ..Self::default()
        }
    }
}

/// Replans and reconciles a supervisor against hotplug topology changes.
pub struct SeatReconciler {
    planner: MultiseatPlanner,
    store: SeatStore,
    displays: Vec<DisplayOutput>,
    inputs: Vec<InputDevice>,
    options: HotplugOptions,
    display_backend: String,
    input_backend: String,
    known: BTreeMap<String, SeatPlan>,
    diagnostics: Vec<PlanDiagnostic>,
}

impl SeatReconciler {
    /// Build a reconciler from the current topology and persisted store.
    pub fn new(
        planner: MultiseatPlanner,
        store: SeatStore,
        displays: Vec<DisplayOutput>,
        inputs: Vec<InputDevice>,
    ) -> Self {
        Self {
            planner,
            store,
            displays,
            inputs,
            options: HotplugOptions::default(),
            display_backend: "provided".to_owned(),
            input_backend: "provided".to_owned(),
            known: BTreeMap::new(),
            diagnostics: Vec::new(),
        }
    }

    /// Replace the reconciliation policy.
    pub fn with_options(mut self, options: HotplugOptions) -> Self {
        self.options = options;
        self
    }

    /// Replace the reconciliation policy on a live reconciler.
    pub fn set_options(&mut self, options: HotplugOptions) {
        self.options = options;
    }

    /// Preserve the discovered backend labels in reconciled plans/status.
    pub fn with_backend_names(
        mut self,
        display_backend: impl Into<String>,
        input_backend: impl Into<String>,
    ) -> Self {
        self.display_backend = display_backend.into();
        self.input_backend = input_backend.into();
        self
    }

    /// The persisted store in use.
    pub fn store(&self) -> &SeatStore {
        &self.store
    }

    /// The last plan diagnostics (also copied into each report).
    pub fn diagnostics(&self) -> &[PlanDiagnostic] {
        &self.diagnostics
    }

    /// Plans currently known to the reconciler (started or pending).
    pub fn known_seats(&self) -> impl Iterator<Item = &SeatPlan> {
        self.known.values()
    }

    /// Apply one event to the reconciler's topology snapshot.
    ///
    /// Excluded displays/hubs are ignored. Duplicate adds replace the previous
    /// snapshot rather than creating a second display/hub.
    pub fn apply_event(&mut self, event: HotplugEvent) {
        match event {
            HotplugEvent::DisplayAdded(display) => {
                if self.options.excluded_displays.contains(&display.name) {
                    return;
                }
                if let Some(existing) = self
                    .displays
                    .iter_mut()
                    .find(|existing| existing.name == display.name)
                {
                    *existing = display;
                } else {
                    self.displays.push(display);
                    self.displays.sort_by(|left, right| {
                        left.index
                            .cmp(&right.index)
                            .then_with(|| left.name.cmp(&right.name))
                    });
                }
            }
            HotplugEvent::DisplayRemoved { output } => {
                self.displays.retain(|display| display.name != output);
            }
            HotplugEvent::InputHubAdded { hub_path, devices } => {
                if self.options.excluded_hubs.contains(&hub_path) {
                    return;
                }
                self.inputs.retain(|input| hub_of(input) != hub_path);
                self.inputs.extend(devices);
                self.inputs
                    .sort_by(|left, right| left.path.cmp(&right.path));
            }
            HotplugEvent::InputHubRemoved { hub_path } => {
                self.inputs.retain(|input| hub_of(input) != hub_path);
            }
        }
    }

    /// Apply a batch of events in order.
    pub fn apply_events(&mut self, events: impl IntoIterator<Item = HotplugEvent>) {
        for event in events {
            self.apply_event(event);
        }
    }

    /// Replan and apply the difference to `supervisor`.
    ///
    /// Planning failure is a diagnostic, not a panic or a fake success. When no
    /// display is discovered, every non-excluded known seat is stopped.
    pub fn reconcile(&mut self, supervisor: &mut dyn SeatSupervisor) -> ReconcileReport {
        let mut report = ReconcileReport::no_op(self.store.clone(), Vec::new());

        let displays: Vec<DisplayOutput> = self
            .displays
            .iter()
            .filter(|display| !self.options.excluded_displays.contains(&display.name))
            .cloned()
            .collect();
        let inputs: Vec<InputDevice> = self
            .inputs
            .iter()
            .filter(|input| !self.options.excluded_hubs.contains(&hub_of(input)))
            .cloned()
            .collect();

        if displays.is_empty() {
            report.diagnostics.push(PlanDiagnostic::new(
                DiagnosticCode::HotplugReconcileFailed,
                "no display is currently discovered; stopping every non-excluded known seat",
            ));
            let desired: BTreeMap<String, SeatPlan> = BTreeMap::new();
            self.stop_missing(&desired, supervisor, &mut report);
            report.changed = !report.stopped.is_empty();
            report.statuses = supervisor.status();
            return report;
        }

        let persisted = match self
            .planner
            .plan_with_store(&displays, &inputs, &self.store)
            .map(|mut persisted| {
                persisted.plan.display_backend = self.display_backend.clone();
                persisted.plan.input_backend = self.input_backend.clone();
                persisted
            }) {
            Ok(persisted) => persisted,
            Err(error) => {
                report.diagnostics.push(PlanDiagnostic::new(
                    DiagnosticCode::HotplugReconcileFailed,
                    error.to_string(),
                ));
                report.statuses = supervisor.status();
                return report;
            }
        };

        self.diagnostics = persisted.plan.diagnostics.clone();
        report.diagnostics = persisted.plan.diagnostics.clone();
        report.plan = Some(persisted.plan.clone());

        let desired: BTreeMap<String, SeatPlan> = persisted
            .plan
            .seats
            .iter()
            .cloned()
            .map(|seat| (seat.config.id.clone(), seat))
            .collect();

        self.stop_missing(&desired, supervisor, &mut report);

        for (seat_id, plan) in &desired {
            if self.options.excluded_seats.contains(seat_id) {
                report.excluded.push(seat_id.clone());
                continue;
            }
            match self.known.get(seat_id) {
                Some(existing) if existing == plan => {
                    report.unchanged.push(seat_id.clone());
                }
                Some(_) => match supervisor.stop(seat_id) {
                    Ok(_) => {
                        self.known.remove(seat_id);
                        match supervisor.start(plan) {
                            Ok(_) => {
                                self.known.insert(seat_id.clone(), plan.clone());
                                report.restarted.push(seat_id.clone());
                            }
                            Err(error) => report.diagnostics.push(PlanDiagnostic::for_seat(
                                DiagnosticCode::HotplugSeatActionFailed,
                                seat_id,
                                format!("restarting changed seat failed: {error}"),
                            )),
                        }
                    }
                    Err(error) => report.diagnostics.push(PlanDiagnostic::for_seat(
                        DiagnosticCode::HotplugSeatActionFailed,
                        seat_id,
                        format!("stopping changed seat failed: {error}"),
                    )),
                },
                None => match supervisor.start(plan) {
                    Ok(status) => {
                        self.known.insert(seat_id.clone(), plan.clone());
                        if status.state == SeatState::Failed {
                            report.diagnostics.push(PlanDiagnostic::for_seat(
                                DiagnosticCode::HotplugSeatActionFailed,
                                seat_id,
                                status
                                    .last_error
                                    .clone()
                                    .unwrap_or_else(|| "seat start reported failed".to_owned()),
                            ));
                        } else {
                            report.started.push(seat_id.clone());
                        }
                    }
                    Err(error) => report.diagnostics.push(PlanDiagnostic::for_seat(
                        DiagnosticCode::HotplugSeatActionFailed,
                        seat_id,
                        format!("starting seat failed: {error}"),
                    )),
                },
            }
        }

        self.store = persisted.store;
        report.store = self.store.clone();
        report.changed = !(report.started.is_empty()
            && report.stopped.is_empty()
            && report.restarted.is_empty());
        report.statuses = supervisor.status();
        report
    }

    fn stop_missing(
        &mut self,
        desired: &BTreeMap<String, SeatPlan>,
        supervisor: &mut dyn SeatSupervisor,
        report: &mut ReconcileReport,
    ) {
        let removed: Vec<String> = self
            .known
            .keys()
            .filter(|seat_id| !desired.contains_key(*seat_id))
            .cloned()
            .collect();
        for seat_id in removed {
            if self.options.excluded_seats.contains(&seat_id) {
                report.excluded.push(seat_id);
                continue;
            }
            match supervisor.stop(&seat_id) {
                Ok(_) => {
                    self.known.remove(&seat_id);
                    report.stopped.push(seat_id);
                }
                Err(error) => report.diagnostics.push(PlanDiagnostic::for_seat(
                    DiagnosticCode::HotplugSeatActionFailed,
                    &seat_id,
                    format!("stopping removed seat failed: {error}"),
                )),
            }
        }
    }

    /// Poll a monitor, feed the debouncer, and reconcile once the quiet window
    /// elapses. Never returns a fabricated report when the monitor fails.
    pub fn poll_and_reconcile(
        &mut self,
        monitor: &mut dyn HotplugMonitor,
        debounce: &mut HotplugDebounce,
        now: Instant,
        supervisor: &mut dyn SeatSupervisor,
    ) -> Result<ReconcileReport, MultiseatError> {
        let events = monitor.poll()?;
        debounce.ingest(events, now);
        let ready = debounce.ready(now);
        if ready.is_empty() {
            return Ok(ReconcileReport::no_op(
                self.store.clone(),
                supervisor.status(),
            ));
        }
        self.apply_events(ready);
        Ok(self.reconcile(supervisor))
    }
}

fn hub_of(device: &InputDevice) -> String {
    device
        .usb_hub
        .clone()
        .unwrap_or_else(|| INTERNAL_HUB.to_owned())
}

/// Read-only sysfs hotplug model.
///
/// The monitor snapshots a display surface and classified input devices. The
/// first poll establishes a baseline and emits nothing; later polls emit display
/// and input-hub deltas. It does not open evdev nodes, change modes, or create a
/// display. By default the display surface is connected DRM connectors, which
/// have no X screen; the CLI injects an `XrandrDisplayBackend` through
/// [`Self::with_display_backend`] so display names/screens match the plan. A
/// DRM-only display fed to the X11 planner gets the normal `MissingX11Display`
/// error rather than an invented `:0`.
pub struct SysfsHotplugMonitor {
    input_backend: SysfsInputBackend,
    drm_root: PathBuf,
    display_backend: Option<Box<dyn DisplayBackend + Send>>,
    displays: BTreeMap<String, DisplayOutput>,
    hubs: BTreeMap<String, Vec<InputDevice>>,
    initialized: bool,
}

impl Default for SysfsHotplugMonitor {
    fn default() -> Self {
        Self::new()
    }
}

impl SysfsHotplugMonitor {
    /// Monitor the default sysfs DRM and input roots.
    pub fn new() -> Self {
        Self::with_roots(
            DEFAULT_SYSFS_INPUT_ROOT,
            DEFAULT_INPUT_DEV_ROOT,
            DEFAULT_DRM_ROOT,
        )
    }

    /// Monitor explicit fixture roots.
    pub fn with_roots(
        sysfs_input_root: impl Into<PathBuf>,
        dev_input_root: impl Into<PathBuf>,
        drm_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            input_backend: SysfsInputBackend::with_roots(sysfs_input_root, dev_input_root),
            drm_root: drm_root.into(),
            display_backend: None,
            displays: BTreeMap::new(),
            hubs: BTreeMap::new(),
            initialized: false,
        }
    }

    /// Use an injected display backend (for example `XrandrDisplayBackend`)
    /// instead of the DRM connector model.
    ///
    /// This is what the CLI uses: it keeps display names, geometry, and the X
    /// screen consistent with the initial plan. The default DRM model is still
    /// available for side-effect-free detection and cannot feed the X11 planner
    /// without an explicit X display.
    pub fn with_display_backend(mut self, backend: Box<dyn DisplayBackend + Send>) -> Self {
        self.display_backend = Some(backend);
        self
    }
}

fn display_map(displays: Vec<DisplayOutput>) -> BTreeMap<String, DisplayOutput> {
    let mut map = BTreeMap::new();
    for display in displays {
        map.insert(display.name.clone(), display);
    }
    map
}

impl HotplugMonitor for SysfsHotplugMonitor {
    fn poll(&mut self) -> Result<Vec<HotplugEvent>, MultiseatError> {
        let devices = self.input_backend.enumerate()?;
        let displays = match self.display_backend.as_ref() {
            Some(backend) => display_map(backend.enumerate()?),
            None => displays_from_drm(&detect_drm_at(&self.drm_root)),
        };
        let hubs = group_devices_by_hub(&devices);

        if !self.initialized {
            self.displays = displays;
            self.hubs = hubs;
            self.initialized = true;
            return Ok(Vec::new());
        }

        let mut events = Vec::new();
        for (name, display) in &displays {
            match self.displays.get(name) {
                Some(previous) if previous == display => {}
                _ => events.push(HotplugEvent::DisplayAdded(display.clone())),
            }
        }
        for name in self.displays.keys() {
            if !displays.contains_key(name) {
                events.push(HotplugEvent::DisplayRemoved {
                    output: name.clone(),
                });
            }
        }
        for (hub_path, devices) in &hubs {
            match self.hubs.get(hub_path) {
                Some(previous) if previous == devices => {}
                _ => events.push(HotplugEvent::InputHubAdded {
                    hub_path: hub_path.clone(),
                    devices: devices.clone(),
                }),
            }
        }
        for hub_path in self.hubs.keys() {
            if !hubs.contains_key(hub_path) {
                events.push(HotplugEvent::InputHubRemoved {
                    hub_path: hub_path.clone(),
                });
            }
        }

        self.displays = displays;
        self.hubs = hubs;
        Ok(events)
    }
}

fn group_devices_by_hub(devices: &[InputDevice]) -> BTreeMap<String, Vec<InputDevice>> {
    let mut hubs: BTreeMap<String, Vec<InputDevice>> = BTreeMap::new();
    for device in devices {
        hubs.entry(hub_of(device)).or_default().push(device.clone());
    }
    for devices in hubs.values_mut() {
        devices.sort_by(|left, right| left.path.cmp(&right.path));
    }
    hubs
}

fn displays_from_drm(detection: &DrmDetection) -> BTreeMap<String, DisplayOutput> {
    let mut displays = BTreeMap::new();
    let mut x_offset = 0i32;
    let mut index = 0u32;
    for connector in detection.connected() {
        let (width, height) = connector
            .modes
            .iter()
            .find_map(|mode| parse_mode_size(mode))
            .unwrap_or((0, 0));
        let display = DisplayOutput {
            index,
            name: connector.connector.clone(),
            width,
            height,
            x: x_offset,
            y: 0,
            x_screen: String::new(),
            primary: false,
            virtual_display: false,
        };
        x_offset = x_offset.saturating_add(i32::try_from(width).unwrap_or(0));
        index = index.saturating_add(1);
        displays.insert(connector.connector.clone(), display);
    }
    displays
}

fn parse_mode_size(mode: &str) -> Option<(u32, u32)> {
    let size = mode.split_whitespace().next()?;
    let (width, height) = size.split_once('x')?;
    Some((width.parse().ok()?, height.parse().ok()?))
}
