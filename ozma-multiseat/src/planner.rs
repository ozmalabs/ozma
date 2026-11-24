// SPDX-License-Identifier: AGPL-3.0-only
//! Deterministic multi-seat planning.
//!
//! The planner takes the outputs and input devices reported by the backends,
//! assigns one seat per display, groups input by USB hub, allocates sequential
//! ports, and emits the exact `ozma-node --profile desktop` command for each
//! seat. It is pure with respect to the host: it never starts a node, grabs a
//! device, or creates a PipeWire sink.

use std::collections::{BTreeMap, BTreeSet};

use crate::display::{DisplayBackend, DisplayOutput};
use crate::error::MultiseatError;
use crate::input::{group_inputs, InputBackend, InputDevice, InputGroup};
use crate::ownership::DisplayOwnership;
use crate::persistence::{PersistedSeat, SeatStore};
use crate::routing::InputRoutePlan;
use crate::seat::{
    CapturePlan, DiagnosticCode, MultiseatPlan, NodeCommand, PlanDiagnostic, SeatConfig, SeatPlan,
    SeatPorts, SeatProfile,
};

/// Which capture transport the planner should emit for each seat.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum CapturePreference {
    /// X11 `x11grab` for the output's screen and region. This is the default
    /// because each display already has an explicit X screen/offset.
    #[default]
    X11,
    /// PipeWire capture. Each display/output name must map to an explicit
    /// target; the planner never invents a `name:` or `node:` target.
    PipeWire {
        /// Display name (or index as a string) to PipeWire target.
        targets: BTreeMap<String, String>,
    },
}

/// A plan plus the persisted store updated from it.
///
/// The store keeps entries for displays that are currently absent, so a
/// returning display regains its stable seat id, ports, and audio sink.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PersistedPlan {
    /// The plan for the currently discovered topology.
    pub plan: MultiseatPlan,
    /// Store updated with the planned seats while retaining missing displays.
    pub store: SeatStore,
}

/// Planner configuration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PlannerOptions {
    /// UDP port for seat 0; later seats increment by one.
    pub base_udp_port: u16,
    /// HTTP port for seat 0; later seats increment by one.
    pub base_http_port: u16,
    /// Host name used to build seat ids and node names.
    pub machine_name: String,
    /// Node binary name or path.
    pub node_program: String,
    /// Requested seat count. `None` means one seat per available display.
    pub seat_count: Option<usize>,
    /// Profile assigned to every planned seat.
    pub profile: SeatProfile,
    /// Prefix for generated audio sink names, normally `ozma-seat`.
    pub audio_sink_prefix: String,
    /// Capture transport selection.
    pub capture: CapturePreference,
    /// Fallback X display when a [`DisplayOutput`] carries no `x_screen`.
    pub x11_display: Option<String>,
    /// Extra node arguments appended after the generated ones.
    pub extra_node_args: Vec<String>,
}

impl Default for PlannerOptions {
    fn default() -> Self {
        Self {
            base_udp_port: 7331,
            base_http_port: 7382,
            machine_name: "ozma".to_owned(),
            node_program: "ozma-node".to_owned(),
            seat_count: None,
            profile: SeatProfile::Desktop,
            audio_sink_prefix: "ozma-seat".to_owned(),
            capture: CapturePreference::X11,
            x11_display: None,
            extra_node_args: Vec::new(),
        }
    }
}

/// Plans seats from backend output.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct MultiseatPlanner {
    options: PlannerOptions,
}

impl MultiseatPlanner {
    /// Build a planner with explicit options.
    pub fn new(options: PlannerOptions) -> Self {
        Self { options }
    }

    /// The options this planner uses.
    pub fn options(&self) -> &PlannerOptions {
        &self.options
    }

    /// Plan from already-enumerated displays and input devices.
    ///
    /// This path does not create virtual displays. If more seats are requested
    /// than displays exist, the plan is capped and carries a diagnostic.
    pub fn plan(
        &self,
        displays: &[DisplayOutput],
        inputs: &[InputDevice],
    ) -> Result<MultiseatPlan, MultiseatError> {
        let requested = self.requested_seats(displays.len())?;
        let mut diagnostics = Vec::new();
        let displays =
            self.resolve_displays(displays.to_vec(), requested, None, &mut diagnostics)?;
        self.build_plan(
            displays,
            requested,
            inputs,
            "provided",
            "provided",
            diagnostics,
        )
    }

    /// Plan while reusing explicit identities from a persisted store.
    ///
    /// A persisted seat is reused only when its display output is still
    /// discovered. Missing displays/devices produce non-fatal diagnostics and
    /// are never resurrected. New displays receive deterministic ids and ports
    /// that avoid every persisted id/port, including ids/ports belonging to
    /// currently missing displays.
    pub fn plan_with_store(
        &self,
        displays: &[DisplayOutput],
        inputs: &[InputDevice],
        store: &SeatStore,
    ) -> Result<PersistedPlan, MultiseatError> {
        store.validate()?;
        let requested = self.requested_seats(displays.len())?;
        let mut diagnostics = Vec::new();
        let displays =
            self.resolve_displays(displays.to_vec(), requested, None, &mut diagnostics)?;
        let plan = self.build_plan_with_store(
            displays,
            requested,
            inputs,
            "provided",
            "provided",
            diagnostics,
            Some(store),
        )?;
        let store = store.merge_plan(&plan);
        store.validate()?;
        Ok(PersistedPlan { plan, store })
    }

    /// Enumerate through the backends and plan, creating virtual displays when
    /// the requested seat count exceeds the physical output count.
    pub fn plan_with_backends(
        &self,
        display_backend: &dyn DisplayBackend,
        input_backend: &dyn InputBackend,
    ) -> Result<MultiseatPlan, MultiseatError> {
        let displays = display_backend.enumerate().map_err(|error| match error {
            MultiseatError::DisplayBackend(_) => error,
            other => MultiseatError::DisplayBackend(other.to_string()),
        })?;
        let inputs = input_backend.enumerate().map_err(|error| match error {
            MultiseatError::InputBackend(_) => error,
            other => MultiseatError::InputBackend(other.to_string()),
        })?;

        let requested = self.requested_seats(displays.len())?;
        let mut diagnostics = Vec::new();
        let displays =
            self.resolve_displays(displays, requested, Some(display_backend), &mut diagnostics)?;

        self.build_plan(
            displays,
            requested,
            &inputs,
            display_backend.backend_name(),
            input_backend.backend_name(),
            diagnostics,
        )
    }

    fn requested_seats(&self, available: usize) -> Result<usize, MultiseatError> {
        match self.options.seat_count {
            Some(0) => Err(MultiseatError::ZeroSeats),
            Some(requested) => Ok(requested),
            None => Ok(available),
        }
    }

    fn resolve_displays(
        &self,
        mut displays: Vec<DisplayOutput>,
        requested: usize,
        backend: Option<&dyn DisplayBackend>,
        diagnostics: &mut Vec<PlanDiagnostic>,
    ) -> Result<Vec<DisplayOutput>, MultiseatError> {
        if requested == 0 {
            return Err(MultiseatError::NoDisplays);
        }

        if displays.len() < requested {
            if let Some(backend) = backend {
                let defaults = self.options.profile.defaults();
                while displays.len() < requested {
                    match backend.create_virtual(defaults.capture_width, defaults.capture_height)? {
                        Some(display) => {
                            diagnostics.push(PlanDiagnostic::new(
                                DiagnosticCode::VirtualDisplayCreated,
                                format!(
                                    "created virtual display {} ({}x{}) to reach {requested} seat(s)",
                                    display.name, display.width, display.height
                                ),
                            ));
                            displays.push(display);
                        }
                        None => {
                            diagnostics.push(PlanDiagnostic::new(
                                DiagnosticCode::VirtualDisplayUnavailable,
                                format!(
                                    "virtual display creation is unavailable; {} of {requested} requested seat(s) can be planned",
                                    displays.len()
                                ),
                            ));
                            break;
                        }
                    }
                }
            }
        }

        if displays.is_empty() {
            return Err(MultiseatError::NoDisplays);
        }

        validate_display_identity(&displays)?;
        displays.sort_by(|left, right| {
            left.index
                .cmp(&right.index)
                .then_with(|| left.name.cmp(&right.name))
        });

        let planned = requested.min(displays.len());
        if planned == 0 {
            return Err(MultiseatError::NoDisplays);
        }
        displays.truncate(planned);
        for display in &displays {
            validate_display_geometry(display)?;
        }

        if planned < requested {
            diagnostics.push(PlanDiagnostic::new(
                DiagnosticCode::RequestedSeatsCapped,
                format!(
                    "requested {requested} seat(s) but only {planned} display(s) are available"
                ),
            ));
        }

        Ok(displays)
    }

    fn build_plan(
        &self,
        displays: Vec<DisplayOutput>,
        requested_seats: usize,
        inputs: &[InputDevice],
        display_backend: &str,
        input_backend: &str,
        diagnostics: Vec<PlanDiagnostic>,
    ) -> Result<MultiseatPlan, MultiseatError> {
        self.build_plan_with_store(
            displays,
            requested_seats,
            inputs,
            display_backend,
            input_backend,
            diagnostics,
            None,
        )
    }

    fn build_plan_with_store(
        &self,
        displays: Vec<DisplayOutput>,
        requested_seats: usize,
        inputs: &[InputDevice],
        display_backend: &str,
        input_backend: &str,
        mut diagnostics: Vec<PlanDiagnostic>,
        store: Option<&SeatStore>,
    ) -> Result<MultiseatPlan, MultiseatError> {
        let groups = group_inputs(inputs);
        let persisted_by_display = match store {
            Some(store) => persisted_for_displays(store, &displays, inputs, &mut diagnostics),
            None => BTreeMap::new(),
        };
        let persisted_for_ordinal: Vec<Option<&PersistedSeat>> = displays
            .iter()
            .map(|display| persisted_by_display.get(&display.name))
            .collect();
        let assignments =
            self.assign_inputs_with_persisted(&groups, &persisted_for_ordinal, &mut diagnostics);

        if inputs.is_empty() {
            diagnostics.push(PlanDiagnostic::new(
                DiagnosticCode::NoInputDevices,
                "the input backend returned no usable evdev devices; seats will have no keyboard/mouse assignment",
            ));
        }

        let machine = machine_name(&self.options.machine_name);
        let mut seats = Vec::with_capacity(displays.len());
        let mut used_ids: BTreeSet<String> = store
            .map(|store| store.seats.iter().map(|seat| seat.id.clone()).collect())
            .unwrap_or_default();
        let mut used_udp: BTreeSet<u16> = store
            .map(|store| store.seats.iter().map(|seat| seat.udp_port).collect())
            .unwrap_or_default();
        let mut used_http: BTreeSet<u16> = store
            .map(|store| store.seats.iter().map(|seat| seat.http_port).collect())
            .unwrap_or_default();
        let mut next_seat_index = 0usize;

        for (ordinal, display) in displays.iter().enumerate() {
            let persisted = persisted_for_ordinal[ordinal];
            let (seat_id, audio_sink, ports, seat_profile) = match persisted {
                Some(persisted) => (
                    persisted.id.clone(),
                    persisted.audio_sink.clone(),
                    SeatPorts {
                        udp: persisted.udp_port,
                        http: persisted.http_port,
                    },
                    persisted.profile,
                ),
                None => {
                    let (index, seat_id) = loop {
                        let index = next_seat_index;
                        next_seat_index = next_seat_index.saturating_add(1);
                        let seat_id = format!("{machine}-seat-{index}");
                        if !used_ids.contains(&seat_id) {
                            break (index, seat_id);
                        }
                    };
                    let ports = allocate_next_ports(
                        self.options.base_udp_port,
                        self.options.base_http_port,
                        &mut used_udp,
                        &mut used_http,
                    )?;
                    let audio_sink = audio_sink_name(&self.options.audio_sink_prefix, index);
                    (seat_id, audio_sink, ports, self.options.profile)
                }
            };

            // Persisted ids are already reserved; only a newly allocated id
            // can collide here.
            if persisted.is_none() && !used_ids.insert(seat_id.clone()) {
                return Err(MultiseatError::DuplicateSeat(seat_id));
            }
            used_udp.insert(ports.udp);
            used_http.insert(ports.http);

            let capture = self.capture_for(display, seat_profile.defaults())?;
            let ownership = match &capture {
                CapturePlan::X11 {
                    display: x_screen, ..
                } => Some(DisplayOwnership::from_output(&seat_id, display, x_screen)),
                CapturePlan::PipeWire { .. } => None,
            };
            let input = assignments[ordinal].clone();
            let input_route = match (&capture, input.as_ref()) {
                (
                    CapturePlan::X11 {
                        display: x_screen, ..
                    },
                    Some(group),
                ) => Some(InputRoutePlan::for_group(
                    &seat_id,
                    &display.name,
                    x_screen,
                    group,
                )),
                _ => None,
            };
            let input_devices = input
                .as_ref()
                .map(InputGroup::all_devices)
                .unwrap_or_default();
            let input_hub = input.as_ref().map(|group| group.hub_path.clone());
            if input.is_none() {
                diagnostics.push(PlanDiagnostic::for_seat(
                    DiagnosticCode::SeatWithoutInput,
                    &seat_id,
                    format!(
                        "display {} has no input group; the seat is planned without keyboard/mouse ownership",
                        display.name
                    ),
                ));
            }

            let config = SeatConfig {
                id: seat_id.clone(),
                display_index: display.index,
                display_output: display.name.clone(),
                input_devices,
                input_hub,
                audio_sink: audio_sink.clone(),
                udp_port: ports.udp,
                http_port: ports.http,
                profile: seat_profile,
                capture: capture.clone(),
            };
            let command = self.node_command(
                &seat_id,
                ports,
                &audio_sink,
                &capture,
                ownership.as_ref(),
                input_route.as_ref(),
            );

            seats.push(SeatPlan {
                config,
                node_name: seat_id,
                display: display.clone(),
                input,
                ownership,
                input_route,
                command,
            });
        }

        Ok(MultiseatPlan {
            seats,
            requested_seats,
            display_backend: display_backend.to_owned(),
            input_backend: input_backend.to_owned(),
            diagnostics,
        })
    }

    fn assign_inputs_with_persisted(
        &self,
        groups: &[InputGroup],
        persisted: &[Option<&PersistedSeat>],
        diagnostics: &mut Vec<PlanDiagnostic>,
    ) -> Vec<Option<InputGroup>> {
        let seat_count = persisted.len();
        let mut assignments = vec![None; seat_count];
        let mut used_hubs = BTreeSet::new();
        // A seat whose persisted hub is missing/conflicting must not silently
        // receive another user's discovered group.
        let mut blocked = BTreeSet::new();

        // Persisted hub mappings win over positional assignment.
        for (ordinal, seat) in persisted.iter().enumerate() {
            let Some(seat) = seat else {
                continue;
            };
            let Some(hub) = seat.input_hub.as_deref() else {
                continue;
            };
            match groups.iter().find(|group| group.hub_path == hub) {
                Some(group) => {
                    if used_hubs.insert(group.hub_path.clone()) {
                        assignments[ordinal] = Some(group.clone());
                    } else {
                        blocked.insert(ordinal);
                        diagnostics.push(PlanDiagnostic::for_seat(
                            DiagnosticCode::PersistedSeatConflict,
                            &seat.id,
                            format!(
                                "input hub {hub:?} is claimed by more than one persisted seat; this seat is planned without input rather than substituting another group"
                            ),
                        ));
                    }
                }
                None => {
                    blocked.insert(ordinal);
                    diagnostics.push(PlanDiagnostic::for_seat(
                        DiagnosticCode::PersistedSeatMissingInput,
                        &seat.id,
                        format!(
                            "persisted input hub {hub:?} was not discovered; the seat is planned without that input group"
                        ),
                    ));
                }
            }
        }

        // Full groups that are not already claimed fill the remaining seats in
        // discovery order, skipping seats blocked by a missing persisted hub.
        let remaining_full: Vec<&InputGroup> = groups
            .iter()
            .filter(|group| group.has_input() && !used_hubs.contains(&group.hub_path))
            .collect();
        let mut full_index = 0usize;
        for (ordinal, slot) in assignments.iter_mut().enumerate() {
            if slot.is_some() || blocked.contains(&ordinal) {
                continue;
            }
            let Some(group) = remaining_full.get(full_index).copied() else {
                break;
            };
            full_index += 1;
            used_hubs.insert(group.hub_path.clone());
            *slot = Some(group.clone());
        }
        for group in remaining_full.into_iter().skip(full_index) {
            diagnostics.push(PlanDiagnostic::new(
                DiagnosticCode::UnassignedInputGroup,
                format!(
                    "input group {} has no free seat; its {} device(s) stay unassigned",
                    group.hub_path,
                    group.device_count()
                ),
            ));
        }

        let mut gamepad_seat = 0usize;
        for group in groups
            .iter()
            .filter(|group| !group.has_input() && !group.gamepads.is_empty())
        {
            if used_hubs.contains(&group.hub_path) {
                continue;
            }
            while gamepad_seat < seat_count && blocked.contains(&gamepad_seat) {
                gamepad_seat += 1;
            }
            if gamepad_seat < seat_count {
                match &mut assignments[gamepad_seat] {
                    Some(existing) => existing.gamepads.extend(group.gamepads.iter().cloned()),
                    slot @ None => *slot = Some(group.clone()),
                }
                used_hubs.insert(group.hub_path.clone());
                gamepad_seat += 1;
            } else {
                diagnostics.push(PlanDiagnostic::new(
                    DiagnosticCode::UnassignedInputGroup,
                    format!(
                        "gamepad group {} has no free seat; {} device(s) stay unassigned",
                        group.hub_path,
                        group.gamepads.len()
                    ),
                ));
            }
        }

        for group in groups.iter().filter(|group| {
            !group.has_input() && group.gamepads.is_empty() && !used_hubs.contains(&group.hub_path)
        }) {
            diagnostics.push(PlanDiagnostic::new(
                DiagnosticCode::UnassignedInputGroup,
                format!(
                    "input group {} has no keyboard, mouse, or gamepad; its {} device(s) stay unassigned",
                    group.hub_path,
                    group.device_count()
                ),
            ));
        }

        assignments
    }

    fn capture_for(
        &self,
        display: &DisplayOutput,
        defaults: crate::seat::ProfileDefaults,
    ) -> Result<CapturePlan, MultiseatError> {
        match &self.options.capture {
            CapturePreference::X11 => {
                let x_screen = if !display.x_screen.trim().is_empty() {
                    display.x_screen.trim().to_owned()
                } else {
                    self.options
                        .x11_display
                        .as_deref()
                        .map(str::trim)
                        .filter(|value| !value.is_empty())
                        .map(str::to_owned)
                        .ok_or_else(|| MultiseatError::MissingX11Display(display.name.clone()))?
                };

                Ok(CapturePlan::X11 {
                    display: x_screen,
                    offset_x: display.x,
                    offset_y: display.y,
                    width: display.width,
                    height: display.height,
                    fps: defaults.capture_fps,
                })
            }
            CapturePreference::PipeWire { targets } => {
                let target = targets
                    .get(&display.name)
                    .or_else(|| targets.get(&display.index.to_string()))
                    .filter(|target| !target.trim().is_empty())
                    .cloned()
                    .ok_or_else(|| MultiseatError::MissingPipeWireTarget(display.name.clone()))?;
                Ok(CapturePlan::PipeWire {
                    target,
                    width: defaults.capture_width,
                    height: defaults.capture_height,
                    fps: defaults.capture_fps,
                })
            }
        }
    }

    fn node_command(
        &self,
        seat_id: &str,
        ports: SeatPorts,
        audio_sink: &str,
        capture: &CapturePlan,
        ownership: Option<&DisplayOwnership>,
        input_route: Option<&InputRoutePlan>,
    ) -> NodeCommand {
        let mut args = Vec::new();
        push_pair(&mut args, "--profile", "desktop");
        push_pair(&mut args, "--name", seat_id);
        push_pair(&mut args, "--hid-udp-port", ports.udp.to_string());
        push_pair(&mut args, "--http-port", ports.http.to_string());
        push_pair(&mut args, "--input-provider", "uinput");
        args.extend(capture.node_args());
        push_pair(&mut args, "--audio-sink", audio_sink);
        push_pair(
            &mut args,
            "--audio-capture-target",
            format!("monitor:{audio_sink}"),
        );
        args.extend(self.options.extra_node_args.iter().cloned());

        let mut env = capture.environment();
        if let Some(ownership) = ownership {
            env.extend(ownership.environment());
        }
        if let Some(input_route) = input_route {
            env.extend(input_route.environment());
        }
        env.insert("PULSE_SINK".to_owned(), audio_sink.to_owned());
        env.insert("OZMA_SEAT_ID".to_owned(), seat_id.to_owned());

        NodeCommand {
            program: self.options.node_program.clone(),
            args,
            env,
        }
    }
}

/// Allocate `count` sequential UDP/HTTP port pairs.
///
/// Seat N gets `base_udp + N` and `base_http + N`. An overflow or more than
/// 65,536 seats is a typed error rather than a wrapped port.
pub fn allocate_ports(
    count: usize,
    base_udp: u16,
    base_http: u16,
) -> Result<Vec<SeatPorts>, MultiseatError> {
    let mut ports = Vec::with_capacity(count);
    for index in 0..count {
        let offset = u16::try_from(index).map_err(|_| MultiseatError::PortOverflow {
            count,
            base_udp,
            base_http,
        })?;
        let udp = base_udp
            .checked_add(offset)
            .ok_or(MultiseatError::PortOverflow {
                count,
                base_udp,
                base_http,
            })?;
        let http = base_http
            .checked_add(offset)
            .ok_or(MultiseatError::PortOverflow {
                count,
                base_udp,
                base_http,
            })?;
        ports.push(SeatPorts { udp, http });
    }
    Ok(ports)
}

fn persisted_for_displays(
    store: &SeatStore,
    displays: &[DisplayOutput],
    inputs: &[InputDevice],
    diagnostics: &mut Vec<PlanDiagnostic>,
) -> BTreeMap<String, PersistedSeat> {
    let discovered_devices: BTreeSet<&str> =
        inputs.iter().map(|input| input.path.as_str()).collect();
    let mut matched = BTreeMap::new();

    for persisted in &store.seats {
        let Some(display) = displays
            .iter()
            .find(|display| persisted.display.matches(display))
        else {
            diagnostics.push(PlanDiagnostic::for_seat(
                DiagnosticCode::PersistedSeatMissingDisplay,
                &persisted.id,
                format!(
                    "persisted display {:?} is not currently discovered; seat {:?} is retained in the store but not planned",
                    persisted.display.output, persisted.id
                ),
            ));
            continue;
        };

        // The display may have been replugged at a different position/mode;
        // keep the persisted identity and refresh geometry through the plan.
        let _ = display;

        // If the hub itself is gone, `assign_inputs_with_persisted` emits the
        // hub-level diagnostic. Repeating every device path would be noise; the
        // device-level check is for a hub that is still present with a device
        // missing from its group.
        let hub_present = persisted.input_hub.as_deref().map_or(true, |hub| {
            inputs.iter().any(|input| {
                input
                    .usb_hub
                    .as_deref()
                    .unwrap_or(crate::input::INTERNAL_HUB)
                    == hub
            })
        });
        let missing: Vec<&str> = persisted
            .input_devices
            .iter()
            .map(String::as_str)
            .filter(|path| !discovered_devices.contains(path))
            .collect();
        if hub_present && !missing.is_empty() {
            diagnostics.push(PlanDiagnostic::for_seat(
                DiagnosticCode::PersistedSeatMissingInput,
                &persisted.id,
                format!(
                    "persisted input device path(s) {} are not currently discovered; the seat keeps its display but may lose input",
                    missing.join(", ")
                ),
            ));
        }

        matched.insert(persisted.display.output.clone(), persisted.clone());
    }

    matched
}

fn allocate_next_ports(
    base_udp: u16,
    base_http: u16,
    used_udp: &mut BTreeSet<u16>,
    used_http: &mut BTreeSet<u16>,
) -> Result<SeatPorts, MultiseatError> {
    let count = used_udp.len().max(used_http.len()).saturating_add(1);
    let mut offset = 0u16;
    loop {
        let udp = base_udp
            .checked_add(offset)
            .ok_or(MultiseatError::PortOverflow {
                count,
                base_udp,
                base_http,
            })?;
        let http = base_http
            .checked_add(offset)
            .ok_or(MultiseatError::PortOverflow {
                count,
                base_udp,
                base_http,
            })?;
        if !used_udp.contains(&udp) && !used_http.contains(&http) {
            used_udp.insert(udp);
            used_http.insert(http);
            return Ok(SeatPorts { udp, http });
        }
        offset = offset.checked_add(1).ok_or(MultiseatError::PortOverflow {
            count,
            base_udp,
            base_http,
        })?;
    }
}

fn validate_display_identity(displays: &[DisplayOutput]) -> Result<(), MultiseatError> {
    let mut names = BTreeSet::new();
    let mut indexes = BTreeMap::new();
    for display in displays {
        if !names.insert(display.name.clone()) {
            return Err(MultiseatError::DuplicateDisplay {
                display: display.name.clone(),
                reason: "two outputs share the same name".to_owned(),
            });
        }
        if let Some(previous) = indexes.insert(display.index, display.name.clone()) {
            return Err(MultiseatError::DuplicateDisplay {
                display: display.name.clone(),
                reason: format!("index {} is also used by {previous}", display.index),
            });
        }
    }
    Ok(())
}

fn validate_display_geometry(display: &DisplayOutput) -> Result<(), MultiseatError> {
    if display.width == 0 || display.height == 0 {
        return Err(MultiseatError::InvalidDisplay {
            display: display.name.clone(),
            width: display.width,
            height: display.height,
        });
    }
    Ok(())
}

fn machine_name(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        "ozma".to_owned()
    } else {
        trimmed.to_owned()
    }
}

fn audio_sink_name(prefix: &str, ordinal: usize) -> String {
    let prefix = prefix.trim().trim_end_matches('-');
    if prefix.is_empty() {
        format!("ozma-seat-{ordinal}")
    } else {
        format!("{prefix}-{ordinal}")
    }
}

fn push_pair(args: &mut Vec<String>, flag: &str, value: impl Into<String>) {
    args.push(flag.to_owned());
    args.push(value.into());
}
