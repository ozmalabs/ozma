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
        mut diagnostics: Vec<PlanDiagnostic>,
    ) -> Result<MultiseatPlan, MultiseatError> {
        let ports = allocate_ports(
            displays.len(),
            self.options.base_udp_port,
            self.options.base_http_port,
        )?;
        let groups = group_inputs(inputs);
        let assignments = self.assign_inputs(&groups, displays.len(), &mut diagnostics);

        if inputs.is_empty() {
            diagnostics.push(PlanDiagnostic::new(
                DiagnosticCode::NoInputDevices,
                "the input backend returned no usable evdev devices; seats will have no keyboard/mouse assignment",
            ));
        }

        let machine = machine_name(&self.options.machine_name);
        let profile_defaults = self.options.profile.defaults();
        let mut seats = Vec::with_capacity(displays.len());
        let mut seat_ids = BTreeSet::new();

        for (ordinal, display) in displays.iter().enumerate() {
            let seat_id = format!("{machine}-seat-{ordinal}");
            if !seat_ids.insert(seat_id.clone()) {
                return Err(MultiseatError::DuplicateSeat(seat_id));
            }

            let capture = self.capture_for(display, profile_defaults)?;
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

            let audio_sink = audio_sink_name(&self.options.audio_sink_prefix, ordinal);
            let config = SeatConfig {
                id: seat_id.clone(),
                display_index: display.index,
                display_output: display.name.clone(),
                input_devices,
                input_hub,
                audio_sink: audio_sink.clone(),
                udp_port: ports[ordinal].udp,
                http_port: ports[ordinal].http,
                profile: self.options.profile,
                capture: capture.clone(),
            };
            let command = self.node_command(
                &seat_id,
                ports[ordinal],
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

    fn assign_inputs(
        &self,
        groups: &[InputGroup],
        seat_count: usize,
        diagnostics: &mut Vec<PlanDiagnostic>,
    ) -> Vec<Option<InputGroup>> {
        let mut assignments = vec![None; seat_count];

        let full_groups: Vec<&InputGroup> =
            groups.iter().filter(|group| group.has_input()).collect();
        for (index, group) in full_groups.iter().enumerate() {
            if index < seat_count {
                assignments[index] = Some((*group).clone());
            } else {
                diagnostics.push(PlanDiagnostic::new(
                    DiagnosticCode::UnassignedInputGroup,
                    format!(
                        "input group {} has no free seat; its {} device(s) stay unassigned",
                        group.hub_path,
                        group.device_count()
                    ),
                ));
            }
        }

        let mut gamepad_seat = 0usize;
        for group in groups
            .iter()
            .filter(|group| !group.has_input() && !group.gamepads.is_empty())
        {
            if gamepad_seat < seat_count {
                match &mut assignments[gamepad_seat] {
                    Some(existing) => existing.gamepads.extend(group.gamepads.iter().cloned()),
                    slot @ None => *slot = Some(group.clone()),
                }
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

        for group in groups
            .iter()
            .filter(|group| !group.has_input() && group.gamepads.is_empty())
        {
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
