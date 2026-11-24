// SPDX-License-Identifier: AGPL-3.0-only
//! Physical input routing models, command builders, and injectable executors.
//!
//! This module owns the seat-level input assignment contract that the planner
//! and supervisor layers deliberately left out:
//!
//! * [`XInputRouter`] maps pointer devices to an X output through
//!   `xinput map-to-output <device> <output>`. It parses `xinput list --short`
//!   for the runtime device ids and resolves evdev paths to names through
//!   sysfs (or an injected resolver).
//! * [`EvdevGrabRouter`] models exclusive `EVIOCGRAB` requests and per-seat
//!   uinput environment. It never performs an ioctl itself: execution requires
//!   an injected [`EvdevGrabBackend`]. The default backend is
//!   [`UnsupportedEvdevGrabBackend`], which returns a typed
//!   [`MultiseatError::Unsupported`] instead of pretending a grab happened.
//!
//! [`InputRouter`] is the shared `assign`/`release`/`status` contract. Build
//! concrete routers with an injected [`CommandRunner`] (production:
//! [`ProcessCommandRunner`], tests: `fake::FakeCommandRunner`) so no unit test
//! ever runs `xinput`, opens `/dev/input`, or grabs a device.
//!
//! This crate contains `#![forbid(unsafe_code)]`, so the real `EVIOCGRAB`
//! ioctl (request value [`EVIOCGRAB_REQUEST`]) is intentionally represented as
//! a model. A privileged helper or a future isolated backend can implement
//! [`EvdevGrabBackend`] without changing the routing API.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::error::MultiseatError;
use crate::input::InputGroup;

/// Default `xinput` program name.
pub const XINPUT_PROGRAM: &str = "xinput";

/// Environment variable carrying the router name to seat-local applications.
pub const OZMA_INPUT_ROUTER_ENV: &str = "OZMA_INPUT_ROUTER";

/// Environment variable that explicitly names the uinput target seat.
pub const OZMA_UINPUT_SEAT_ENV: &str = "OZMA_UINPUT_SEAT";

/// Environment variable carrying the seat id (also set by the planner).
pub const OZMA_SEAT_ID_ENV: &str = "OZMA_SEAT_ID";

/// Router name reported by [`XInputRouter`].
pub const XINPUT_ROUTER_NAME: &str = "xinput-map-to-output";

/// Router name reported by [`EvdevGrabRouter`].
pub const EVDEV_ROUTER_NAME: &str = "evdev-grab";

/// `EVIOCGRAB` request value (`_IOW('E', 0x90, int)`).
///
/// This is a model constant only. The crate forbids `unsafe`, so no ioctl is
/// issued by this module.
pub const EVIOCGRAB_REQUEST: u64 = 0x4004_4590;

/// A process invocation that a [`CommandRunner`] may execute.
///
/// Building a command is pure: no process runs until a runner is asked to run
/// it. Neither `xinput` nor `xrandr` is executed at construction time.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RouteCommand {
    /// Program name or path.
    pub program: String,
    /// Arguments after the program name.
    pub args: Vec<String>,
    /// Explicit environment for this command. The runner inherits the caller's
    /// environment in addition to these entries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl RouteCommand {
    /// Build a command for `program` with no arguments.
    pub fn new(program: impl Into<String>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            env: BTreeMap::new(),
        }
    }

    /// Append one argument.
    pub fn arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Append several arguments.
    pub fn args<I, S>(mut self, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        self.args.extend(args.into_iter().map(Into::into));
        self
    }

    /// Add one environment entry.
    pub fn with_env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.env.insert(key.into(), value.into());
        self
    }

    /// Add `DISPLAY` when the value is non-empty.
    pub fn with_display(mut self, display: &str) -> Self {
        let display = display.trim();
        if !display.is_empty() {
            self.env.insert("DISPLAY".to_owned(), display.to_owned());
        }
        self
    }

    /// Full argv, including the program as element zero.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

/// Captured result of one command invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status, or `-1` when terminated by a signal.
    pub status: i32,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
    pub stderr: String,
}

impl CommandOutput {
    /// A successful command with empty output.
    pub fn success() -> Self {
        Self {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }

    /// A successful command with stdout.
    pub fn stdout(value: impl Into<String>) -> Self {
        Self {
            status: 0,
            stdout: value.into(),
            stderr: String::new(),
        }
    }

    /// A failed command with an exit status and stderr.
    pub fn failure(status: i32, stderr: impl Into<String>) -> Self {
        Self {
            status,
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    /// Whether the command exited with status zero.
    pub fn is_success(&self) -> bool {
        self.status == 0
    }
}

/// Injectable command executor.
///
/// Production callers use [`ProcessCommandRunner`]. Tests must inject a
/// recording fake so no real X11 or evdev operation occurs.
pub trait CommandRunner {
    /// Run one command and capture its output.
    fn run(&self, command: &RouteCommand) -> Result<CommandOutput, MultiseatError>;
}

/// Real process runner used by operator-facing code.
///
/// It is never selected by tests. A missing binary is reported as
/// [`MultiseatError::CommandUnavailable`] rather than a generic I/O failure.
#[derive(Clone, Copy, Debug, Default)]
pub struct ProcessCommandRunner;

impl CommandRunner for ProcessCommandRunner {
    fn run(&self, command: &RouteCommand) -> Result<CommandOutput, MultiseatError> {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            .envs(&command.env)
            .output()
            .map_err(|error| match error.kind() {
                std::io::ErrorKind::NotFound => MultiseatError::CommandUnavailable {
                    program: command.program.clone(),
                    reason: "not found on PATH".to_owned(),
                },
                _ => MultiseatError::Io {
                    path: command.program.clone(),
                    source: error,
                },
            })?;

        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Resolve an evdev path to its kernel device name.
///
/// The name is what `/sys/class/input/<event>/device/name` reports and what
/// `xinput list` matches. A test fake can supply names without reading sysfs.
pub trait DeviceNameResolver {
    /// Return the device name, or `None` when the path is not an evdev event
    /// node or the sysfs attribute is absent.
    fn device_name(&self, evdev_path: &str) -> Result<Option<String>, MultiseatError>;
}

/// Read-only sysfs resolver for `/sys/class/input`.
#[derive(Clone, Debug)]
pub struct SysfsDeviceNameResolver {
    root: PathBuf,
}

impl Default for SysfsDeviceNameResolver {
    fn default() -> Self {
        Self::new("/sys/class/input")
    }
}

impl SysfsDeviceNameResolver {
    /// Use an explicit sysfs input root (for fixtures and tests).
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Configured sysfs root.
    pub fn root(&self) -> &std::path::Path {
        &self.root
    }
}

impl DeviceNameResolver for SysfsDeviceNameResolver {
    fn device_name(&self, evdev_path: &str) -> Result<Option<String>, MultiseatError> {
        let Some(file_name) = std::path::Path::new(evdev_path)
            .file_name()
            .and_then(|name| name.to_str())
        else {
            return Ok(None);
        };
        if !file_name.starts_with("event") {
            return Ok(None);
        }

        let path = self.root.join(file_name).join("device").join("name");
        match std::fs::read_to_string(&path) {
            Ok(name) => {
                let name = name.trim();
                Ok((!name.is_empty()).then(|| name.to_owned()))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(MultiseatError::io(&path, error)),
        }
    }
}

/// One device from `xinput list`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct XInputDevice {
    /// XI device id.
    pub id: u32,
    /// Device name as reported by XInput.
    pub name: String,
}

/// Parse `xinput list --short` (or the plain `xinput list`) output.
///
/// Lines look like:
///
/// ```text
/// ⎜   ↳ Logitech USB Optical Mouse             id=12   [slave  pointer  (2)]
/// ```
///
/// Lines without `id=` are ignored. Names keep internal spaces; tree-drawing
/// prefixes (`⎜`, `↳`, `∼`) are stripped. Malformed `id=` values are skipped
/// rather than guessed.
pub fn parse_xinput_list(output: &str) -> Vec<XInputDevice> {
    let mut devices = Vec::new();
    for line in output.lines() {
        let Some(id_start) = line.find("id=") else {
            continue;
        };
        let id_text: String = line[id_start + 3..]
            .trim_start()
            .chars()
            .take_while(|character| character.is_ascii_digit())
            .collect();
        let Ok(id) = id_text.parse::<u32>() else {
            continue;
        };

        let name = line[..id_start]
            .split_whitespace()
            .skip_while(|token| !token.chars().any(|character| character.is_alphanumeric()))
            .collect::<Vec<_>>()
            .join(" ");
        if name.is_empty() {
            continue;
        }

        devices.push(XInputDevice { id, name });
    }
    devices
}

/// The display a seat's input should be confined to.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputTarget {
    /// Seat id, also used as the per-seat uinput target.
    pub seat_id: String,
    /// Xrandr output name, for example `HDMI-1`.
    pub display_output: String,
    /// X display string, for example `:0`.
    pub x_screen: String,
}

impl InputTarget {
    /// Build a target and validate its required fields.
    pub fn new(
        seat_id: impl Into<String>,
        display_output: impl Into<String>,
        x_screen: impl Into<String>,
    ) -> Result<Self, MultiseatError> {
        let target = Self {
            seat_id: seat_id.into().trim().to_owned(),
            display_output: display_output.into().trim().to_owned(),
            x_screen: x_screen.into().trim().to_owned(),
        };
        target.validate()?;
        Ok(target)
    }

    /// Validate that all routing keys are present.
    pub fn validate(&self) -> Result<(), MultiseatError> {
        if self.seat_id.is_empty() {
            return Err(MultiseatError::InputRouting(
                "input target is missing a seat id".to_owned(),
            ));
        }
        if self.display_output.is_empty() {
            return Err(MultiseatError::InputRouting(format!(
                "seat {} has no display output for input routing",
                self.seat_id
            )));
        }
        if self.x_screen.is_empty() {
            return Err(MultiseatError::InputRouting(format!(
                "seat {} has no X screen for input routing",
                self.seat_id
            )));
        }
        Ok(())
    }
}

/// One pointer device mapped to an X output.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct XInputMapping {
    /// Evdev path assigned by the planner.
    pub device_path: String,
    /// Kernel/XInput device name.
    pub device_name: String,
    /// Runtime XInput id resolved from `xinput list`.
    pub xinput_id: u32,
    /// Xrandr output the pointer was mapped to.
    pub output: String,
}

/// Result of one successful seat input assignment.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputAssignment {
    /// Seat id.
    pub seat_id: String,
    /// Router name (`xinput-map-to-output` or `evdev-grab`).
    pub router: String,
    /// USB hub key assigned to the seat.
    pub hub_path: String,
    /// Display output the input is confined to.
    pub display_output: String,
    /// X display string.
    pub x_screen: String,
    /// Every evdev path in the assigned group.
    pub devices: Vec<String>,
    /// Pointer mappings actually applied by the XInput router.
    #[serde(default)]
    pub xinput: Vec<XInputMapping>,
    /// Devices actually grabbed by the evdev router (fake/backed only).
    #[serde(default)]
    pub grabbed: Vec<String>,
    /// Per-seat uinput and routing environment.
    #[serde(default)]
    pub env: BTreeMap<String, String>,
    /// Non-fatal issues, for example a keyboard-only group with no pointer to
    /// map or a pointer device that XInput does not know about.
    #[serde(default)]
    pub warnings: Vec<String>,
}

impl InputAssignment {
    fn base(router: &str, group: &InputGroup, target: &InputTarget) -> Self {
        Self {
            seat_id: target.seat_id.clone(),
            router: router.to_owned(),
            hub_path: group.hub_path.clone(),
            display_output: target.display_output.clone(),
            x_screen: target.x_screen.clone(),
            devices: group.all_devices(),
            xinput: Vec::new(),
            grabbed: Vec::new(),
            env: route_environment(&target.seat_id, &target.x_screen, router),
            warnings: Vec::new(),
        }
    }
}

/// Result of releasing one seat's input ownership.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRelease {
    /// Seat id.
    pub seat_id: String,
    /// Device paths released or un-mapped.
    pub released_devices: Vec<String>,
    /// Non-fatal release problems.
    #[serde(default)]
    pub warnings: Vec<String>,
}

/// Shared physical input routing contract.
pub trait InputRouter {
    /// Stable router name for status and diagnostics.
    fn router_name(&self) -> &'static str;

    /// Assign every device in `group` to `target`.
    ///
    /// A partial failure must roll back any change already made for this
    /// assignment and return a typed error. Returning `Ok` means the devices
    /// are routed (or, for a keyboard-only group, tracked for the seat) and the
    /// assignment is visible through [`Self::status`].
    fn assign(
        &mut self,
        group: &InputGroup,
        target: &InputTarget,
    ) -> Result<InputAssignment, MultiseatError>;

    /// Release the assignment for `seat_id`.
    fn release(&mut self, seat_id: &str) -> Result<InputRelease, MultiseatError>;

    /// Current assignments, ordered by seat id.
    fn status(&self) -> Vec<InputAssignment>;
}

/// Build the per-seat environment used by both routers.
///
/// `DISPLAY` is set when a screen is known; the seat id is exposed both as
/// `OZMA_SEAT_ID` (the existing planner variable) and `OZMA_UINPUT_SEAT`
/// (explicit uinput target).
pub fn route_environment(seat_id: &str, x_screen: &str, router: &str) -> BTreeMap<String, String> {
    let mut env = BTreeMap::new();
    let x_screen = x_screen.trim();
    if !x_screen.is_empty() {
        env.insert("DISPLAY".to_owned(), x_screen.to_owned());
    }
    env.insert(OZMA_SEAT_ID_ENV.to_owned(), seat_id.to_owned());
    env.insert(OZMA_UINPUT_SEAT_ENV.to_owned(), seat_id.to_owned());
    env.insert(OZMA_INPUT_ROUTER_ENV.to_owned(), router.to_owned());
    env
}

/// X11 pointer routing through `xinput map-to-output`.
pub struct XInputRouter {
    runner: Box<dyn CommandRunner>,
    resolver: Box<dyn DeviceNameResolver>,
    program: String,
    restore_output: String,
    assignments: BTreeMap<String, InputAssignment>,
}

impl XInputRouter {
    /// Build a router with a default sysfs resolver and `xinput` on `PATH`.
    pub fn new(runner: Box<dyn CommandRunner>) -> Self {
        Self {
            runner,
            resolver: Box::new(SysfsDeviceNameResolver::default()),
            program: XINPUT_PROGRAM.to_owned(),
            restore_output: "all".to_owned(),
            assignments: BTreeMap::new(),
        }
    }

    /// Build a router with an explicit device-name resolver.
    pub fn with_resolver(
        runner: Box<dyn CommandRunner>,
        resolver: Box<dyn DeviceNameResolver>,
    ) -> Self {
        Self {
            runner,
            resolver,
            program: XINPUT_PROGRAM.to_owned(),
            restore_output: "all".to_owned(),
            assignments: BTreeMap::new(),
        }
    }

    /// Override the `xinput` program name or path.
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    /// Output name used to reset a pointer on release. `all` means the whole
    /// screen, which is the `xinput` reset value.
    pub fn with_restore_output(mut self, output: impl Into<String>) -> Self {
        self.restore_output = output.into();
        self
    }

    /// Read the XInput device list for `x_screen`.
    ///
    /// This is the only read-only entry point; it runs `xinput list --short`
    /// through the injected runner and never maps or grabs anything.
    pub fn list_devices(&self, x_screen: &str) -> Result<Vec<XInputDevice>, MultiseatError> {
        let x_screen = x_screen.trim();
        if x_screen.is_empty() {
            return Err(MultiseatError::InputRouting(
                "cannot list XInput devices without an explicit X display".to_owned(),
            ));
        }
        let command = RouteCommand::new(&self.program)
            .args(["list", "--short"])
            .with_display(x_screen);
        let output = self.run_checked(&command)?;
        Ok(parse_xinput_list(&output.stdout))
    }

    /// The `xinput map-to-output` command for one runtime device id.
    pub fn map_command(&self, xinput_id: u32, output: &str, x_screen: &str) -> RouteCommand {
        RouteCommand::new(&self.program)
            .args(["map-to-output", &xinput_id.to_string(), output])
            .with_display(x_screen)
    }

    /// The reset/release command for one runtime device id.
    pub fn release_command(&self, xinput_id: u32, x_screen: &str) -> RouteCommand {
        self.map_command(xinput_id, &self.restore_output, x_screen)
    }

    /// Run a command and translate a non-zero exit into a typed error.
    fn run_checked(&self, command: &RouteCommand) -> Result<CommandOutput, MultiseatError> {
        let output = self.runner.run(command)?;
        if !output.is_success() {
            return Err(MultiseatError::CommandFailed {
                program: command.program.clone(),
                args: command.args.clone(),
                status: output.status,
                stderr: output.stderr.trim().to_owned(),
            });
        }
        Ok(output)
    }

    /// Best-effort rollback of mappings already applied for one assignment.
    fn rollback_mappings(&self, mappings: &[XInputMapping], x_screen: &str) -> Vec<String> {
        let mut failures = Vec::new();
        for mapping in mappings.iter().rev() {
            let command = self.release_command(mapping.xinput_id, x_screen);
            match self.runner.run(&command) {
                Ok(output) if output.is_success() => {}
                Ok(output) => failures.push(format!(
                    "reset of xinput id {} to {} exited {}: {}",
                    mapping.xinput_id,
                    self.restore_output,
                    output.status,
                    output.stderr.trim()
                )),
                Err(error) => failures.push(format!(
                    "reset of xinput id {} to {} failed: {error}",
                    mapping.xinput_id, self.restore_output
                )),
            }
        }
        failures
    }

    fn validate_assign(
        &self,
        group: &InputGroup,
        target: &InputTarget,
    ) -> Result<(), MultiseatError> {
        target.validate()?;
        if group.device_count() == 0 {
            return Err(MultiseatError::InputRouting(format!(
                "input group {:?} has no devices to assign",
                group.hub_path
            )));
        }
        if self.assignments.contains_key(&target.seat_id) {
            return Err(MultiseatError::InputRouting(format!(
                "seat {} already has an input assignment; release it first",
                target.seat_id
            )));
        }
        Ok(())
    }
}

impl InputRouter for XInputRouter {
    fn router_name(&self) -> &'static str {
        XINPUT_ROUTER_NAME
    }

    fn assign(
        &mut self,
        group: &InputGroup,
        target: &InputTarget,
    ) -> Result<InputAssignment, MultiseatError> {
        self.validate_assign(group, target)?;

        let devices = self.list_devices(&target.x_screen)?;
        let mut assignment = InputAssignment::base(XINPUT_ROUTER_NAME, group, target);

        if !group.has_input() {
            assignment.warnings.push(format!(
                "input group {:?} has no keyboard or mouse; only gamepad/other devices are tracked",
                group.hub_path
            ));
        }

        for device_path in &group.mice {
            let Some(name) = self.resolver.device_name(device_path)? else {
                assignment.warnings.push(format!(
                    "{device_path}: no sysfs device name found; pointer not mapped"
                ));
                continue;
            };
            let Some(device) = devices.iter().find(|device| device.name == name) else {
                assignment.warnings.push(format!(
                    "{device_path} ({name:?}): device is not present in xinput list; pointer not mapped"
                ));
                continue;
            };

            let command = self.map_command(device.id, &target.display_output, &target.x_screen);
            if let Err(error) = self.run_checked(&command) {
                let _ = self.rollback_mappings(&assignment.xinput, &target.x_screen);
                return Err(error);
            }
            assignment.xinput.push(XInputMapping {
                device_path: device_path.clone(),
                device_name: name,
                xinput_id: device.id,
                output: target.display_output.clone(),
            });
        }

        if !group.mice.is_empty() && assignment.xinput.is_empty() {
            return Err(MultiseatError::InputRouting(format!(
                "none of the {} pointer device(s) in group {:?} could be mapped to {}: {}",
                group.mice.len(),
                group.hub_path,
                target.display_output,
                assignment.warnings.join("; ")
            )));
        }

        self.assignments
            .insert(target.seat_id.clone(), assignment.clone());
        Ok(assignment)
    }

    fn release(&mut self, seat_id: &str) -> Result<InputRelease, MultiseatError> {
        let Some(assignment) = self.assignments.get(seat_id).cloned() else {
            return Err(MultiseatError::InputRouting(format!(
                "seat {seat_id} has no input assignment to release"
            )));
        };

        let mut released_devices = Vec::new();
        let mut warnings = Vec::new();
        let mut failures = Vec::new();
        for mapping in assignment.xinput.iter().rev() {
            let command = self.release_command(mapping.xinput_id, &assignment.x_screen);
            match self.runner.run(&command) {
                Ok(output) if output.is_success() => {
                    released_devices.push(mapping.device_path.clone());
                }
                Ok(output) => failures.push(format!(
                    "{} (xinput id {}): reset exited {}: {}",
                    mapping.device_path,
                    mapping.xinput_id,
                    output.status,
                    output.stderr.trim()
                )),
                Err(error) => failures.push(format!(
                    "{} (xinput id {}): reset failed: {error}",
                    mapping.device_path, mapping.xinput_id
                )),
            }
        }

        if !failures.is_empty() {
            return Err(MultiseatError::InputRouting(format!(
                "could not reset every pointer for seat {seat_id}; assignment kept: {}",
                failures.join("; ")
            )));
        }

        self.assignments.remove(seat_id);
        if assignment.xinput.is_empty() {
            warnings.push(format!(
                "seat {seat_id} had no pointer mappings; only the tracked assignment was released"
            ));
        }
        Ok(InputRelease {
            seat_id: seat_id.to_owned(),
            released_devices,
            warnings,
        })
    }

    fn status(&self) -> Vec<InputAssignment> {
        self.assignments.values().cloned().collect()
    }
}

/// One requested exclusive grab.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvdevGrabRequest {
    /// Evdev path, for example `/dev/input/event5`.
    pub device: String,
    /// Seat that will own the device.
    pub seat_id: String,
}

/// A modelled `EVIOCGRAB` action.
///
/// This is not an executed command. It is the serializable request a privileged
/// backend (or a test fake) would perform.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvdevGrabCommand {
    /// Evdev path.
    pub device: String,
    /// Seat that owns the device.
    pub seat_id: String,
    /// `ioctl` name, currently always `EVIOCGRAB`.
    pub ioctl: String,
    /// Request value ([`EVIOCGRAB_REQUEST`]).
    pub request: u64,
    /// `true` for grab, `false` for release.
    pub grab: bool,
}

impl EvdevGrabCommand {
    /// Model a grab request for one device.
    pub fn grab(device: impl Into<String>, seat_id: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            seat_id: seat_id.into(),
            ioctl: "EVIOCGRAB".to_owned(),
            request: EVIOCGRAB_REQUEST,
            grab: true,
        }
    }

    /// Model a release request for one device.
    pub fn release(device: impl Into<String>, seat_id: impl Into<String>) -> Self {
        Self {
            device: device.into(),
            seat_id: seat_id.into(),
            ioctl: "EVIOCGRAB".to_owned(),
            request: EVIOCGRAB_REQUEST,
            grab: false,
        }
    }
}

/// Execution seam for exclusive evdev grabs.
///
/// The production default is [`UnsupportedEvdevGrabBackend`]. Tests inject a
/// fake. There is no `unsafe` ioctl in this crate.
pub trait EvdevGrabBackend {
    /// Grab one device exclusively.
    fn grab(&mut self, request: &EvdevGrabRequest) -> Result<(), MultiseatError>;
    /// Release a previously grabbed device.
    fn ungrab(&mut self, request: &EvdevGrabRequest) -> Result<(), MultiseatError>;
}

/// Backend that refuses every grab with a typed unsupported error.
///
/// This is the honest default: the crate models `EVIOCGRAB` but cannot issue
/// the ioctl while `#![forbid(unsafe_code)]` is in force.
#[derive(Clone, Copy, Debug, Default)]
pub struct UnsupportedEvdevGrabBackend;

impl EvdevGrabBackend for UnsupportedEvdevGrabBackend {
    fn grab(&mut self, request: &EvdevGrabRequest) -> Result<(), MultiseatError> {
        Err(MultiseatError::Unsupported(format!(
            "exclusive EVIOCGRAB for {} on seat {} needs a privileged helper; \
             this crate only models the request",
            request.device, request.seat_id
        )))
    }

    fn ungrab(&mut self, request: &EvdevGrabRequest) -> Result<(), MultiseatError> {
        Err(MultiseatError::Unsupported(format!(
            "releasing EVIOCGRAB for {} on seat {} needs a privileged helper",
            request.device, request.seat_id
        )))
    }
}

/// Exclusive evdev grab router.
pub struct EvdevGrabRouter {
    backend: Box<dyn EvdevGrabBackend>,
    assignments: BTreeMap<String, InputAssignment>,
}

impl Default for EvdevGrabRouter {
    fn default() -> Self {
        Self::new()
    }
}

impl EvdevGrabRouter {
    /// Build a router with the unsupported default backend.
    pub fn new() -> Self {
        Self::with_backend(Box::new(UnsupportedEvdevGrabBackend))
    }

    /// Build a router with an injected backend (a fake in tests, or a future
    /// privileged helper).
    pub fn with_backend(backend: Box<dyn EvdevGrabBackend>) -> Self {
        Self {
            backend,
            assignments: BTreeMap::new(),
        }
    }

    /// Model the grab commands for a group.
    pub fn grab_commands(group: &InputGroup, seat_id: &str) -> Vec<EvdevGrabCommand> {
        group
            .all_devices()
            .into_iter()
            .map(|device| EvdevGrabCommand::grab(device, seat_id))
            .collect()
    }

    /// Model the release commands for a group.
    pub fn release_commands(group: &InputGroup, seat_id: &str) -> Vec<EvdevGrabCommand> {
        group
            .all_devices()
            .into_iter()
            .map(|device| EvdevGrabCommand::release(device, seat_id))
            .collect()
    }

    /// Per-seat uinput environment for this router.
    pub fn environment(seat_id: &str, x_screen: &str) -> BTreeMap<String, String> {
        route_environment(seat_id, x_screen, EVDEV_ROUTER_NAME)
    }
}

impl InputRouter for EvdevGrabRouter {
    fn router_name(&self) -> &'static str {
        EVDEV_ROUTER_NAME
    }

    fn assign(
        &mut self,
        group: &InputGroup,
        target: &InputTarget,
    ) -> Result<InputAssignment, MultiseatError> {
        target.validate()?;
        if group.device_count() == 0 {
            return Err(MultiseatError::InputRouting(format!(
                "input group {:?} has no devices to grab",
                group.hub_path
            )));
        }
        if self.assignments.contains_key(&target.seat_id) {
            return Err(MultiseatError::InputRouting(format!(
                "seat {} already has an input assignment; release it first",
                target.seat_id
            )));
        }

        let devices = group.all_devices();
        let mut grabbed: Vec<String> = Vec::new();
        for device in &devices {
            let request = EvdevGrabRequest {
                device: device.clone(),
                seat_id: target.seat_id.clone(),
            };
            if let Err(error) = self.backend.grab(&request) {
                for already in grabbed.iter().rev() {
                    let release = EvdevGrabRequest {
                        device: already.clone(),
                        seat_id: target.seat_id.clone(),
                    };
                    let _ = self.backend.ungrab(&release);
                }
                return Err(error);
            }
            grabbed.push(device.clone());
        }

        let mut assignment = InputAssignment::base(EVDEV_ROUTER_NAME, group, target);
        if !group.has_input() {
            assignment.warnings.push(format!(
                "input group {:?} has no keyboard or mouse; only gamepad/other devices are grabbed",
                group.hub_path
            ));
        }
        assignment.grabbed = grabbed;
        self.assignments
            .insert(target.seat_id.clone(), assignment.clone());
        Ok(assignment)
    }

    fn release(&mut self, seat_id: &str) -> Result<InputRelease, MultiseatError> {
        let Some(assignment) = self.assignments.get(seat_id).cloned() else {
            return Err(MultiseatError::InputRouting(format!(
                "seat {seat_id} has no input assignment to release"
            )));
        };

        let mut released_devices = Vec::new();
        let mut failures = Vec::new();
        for device in assignment.grabbed.iter().rev() {
            let request = EvdevGrabRequest {
                device: device.clone(),
                seat_id: seat_id.to_owned(),
            };
            match self.backend.ungrab(&request) {
                Ok(()) => released_devices.push(device.clone()),
                Err(error) => failures.push(format!("{device}: {error}")),
            }
        }

        if !failures.is_empty() {
            return Err(MultiseatError::InputRouting(format!(
                "could not release every grabbed device for seat {seat_id}; assignment kept: {}",
                failures.join("; ")
            )));
        }

        self.assignments.remove(seat_id);
        Ok(InputRelease {
            seat_id: seat_id.to_owned(),
            released_devices,
            warnings: Vec::new(),
        })
    }

    fn status(&self) -> Vec<InputAssignment> {
        self.assignments.values().cloned().collect()
    }
}

/// Planned input routing for one seat.
///
/// This is intent, not a live assignment: device names and XInput ids are
/// resolved at assignment time. `grab_exclusive` stays `false` because this
/// crate never issues a real grab.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputRoutingStrategy {
    /// `xinput map-to-output` for pointer devices.
    XInputMapToOutput,
    /// Modelled exclusive evdev grab.
    EvdevGrab,
}

impl InputRoutingStrategy {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::XInputMapToOutput => XINPUT_ROUTER_NAME,
            Self::EvdevGrab => EVDEV_ROUTER_NAME,
        }
    }
}

/// Per-seat input routing intent carried by [`crate::seat::SeatPlan`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputRoutePlan {
    /// Seat id.
    pub seat_id: String,
    /// USB hub key of the assigned group.
    pub hub_path: String,
    /// X output the pointer devices belong to.
    pub display_output: String,
    /// X display string.
    pub x_screen: String,
    /// Routing strategy this plan describes.
    pub strategy: InputRoutingStrategy,
    /// Mouse/pointer evdev paths that should be mapped to the output.
    #[serde(default)]
    pub pointer_devices: Vec<String>,
    /// Every evdev path that could be grabbed exclusively.
    #[serde(default)]
    pub exclusive_devices: Vec<String>,
    /// Always `false` in this crate; a real grab needs an injected backend.
    #[serde(default)]
    pub grab_exclusive: bool,
    /// Environment additions for the node process.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub env: BTreeMap<String, String>,
}

impl InputRoutePlan {
    /// Plan XInput output mapping for one group.
    pub fn for_group(
        seat_id: impl Into<String>,
        display_output: impl Into<String>,
        x_screen: impl Into<String>,
        group: &InputGroup,
    ) -> Self {
        let seat_id = seat_id.into();
        let x_screen = x_screen.into();
        Self {
            hub_path: group.hub_path.clone(),
            pointer_devices: group.mice.clone(),
            exclusive_devices: group.all_devices(),
            display_output: display_output.into(),
            x_screen: x_screen.clone(),
            strategy: InputRoutingStrategy::XInputMapToOutput,
            grab_exclusive: false,
            env: route_environment(&seat_id, &x_screen, XINPUT_ROUTER_NAME),
            seat_id,
        }
    }

    /// Mark that a grab would be required. The plan is still only intent; a
    /// real grab needs an injected [`EvdevGrabBackend`].
    pub fn with_exclusive_grab(mut self) -> Self {
        self.strategy = InputRoutingStrategy::EvdevGrab;
        self.grab_exclusive = true;
        self.env.insert(
            OZMA_INPUT_ROUTER_ENV.to_owned(),
            EVDEV_ROUTER_NAME.to_owned(),
        );
        self
    }

    /// Environment additions for the node process.
    pub fn environment(&self) -> BTreeMap<String, String> {
        self.env.clone()
    }
}

/// A group paired with the seat display that should own it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatInputAssignment {
    /// Input group to route.
    pub group: InputGroup,
    /// Seat/display destination.
    pub target: InputTarget,
}

impl SeatInputAssignment {
    /// Build a seat assignment.
    pub fn new(group: InputGroup, target: InputTarget) -> Self {
        Self { group, target }
    }
}

/// Assign every group through `router`, rolling back earlier seats on failure.
///
/// Duplicate seat ids, duplicate hubs, or duplicate display outputs are
/// rejected before the first command runs. If a later assignment fails, every
/// earlier assignment is released in reverse order and the original typed error
/// is returned.
pub fn assign_seats(
    router: &mut dyn InputRouter,
    seats: &[SeatInputAssignment],
) -> Result<Vec<InputAssignment>, MultiseatError> {
    let mut seat_ids = BTreeSet::new();
    let mut hubs = BTreeSet::new();
    let mut outputs = BTreeSet::new();
    for seat in seats {
        seat.target.validate()?;
        if !seat_ids.insert(seat.target.seat_id.clone()) {
            return Err(MultiseatError::InputRouting(format!(
                "duplicate seat id {:?} in input assignment",
                seat.target.seat_id
            )));
        }
        if !hubs.insert(seat.group.hub_path.clone()) {
            return Err(MultiseatError::InputRouting(format!(
                "input group {:?} is assigned to more than one seat",
                seat.group.hub_path
            )));
        }
        if !outputs.insert(seat.target.display_output.clone()) {
            return Err(MultiseatError::InputRouting(format!(
                "display output {:?} is assigned more than once",
                seat.target.display_output
            )));
        }
    }

    let mut assignments = Vec::with_capacity(seats.len());
    for seat in seats {
        match router.assign(&seat.group, &seat.target) {
            Ok(assignment) => assignments.push(assignment),
            Err(error) => {
                let mut rollback_failures = Vec::new();
                for assigned in assignments.iter().rev() {
                    if let Err(rollback) = router.release(&assigned.seat_id) {
                        rollback_failures.push(rollback.to_string());
                    }
                }
                if rollback_failures.is_empty() {
                    return Err(error);
                }
                return Err(MultiseatError::InputRouting(format!(
                    "{error}; rollback also failed: {}",
                    rollback_failures.join("; ")
                )));
            }
        }
    }
    Ok(assignments)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_short_and_full_xinput_lists() {
        let short = "\
⎡ Virtual core pointer                     id=2    [master pointer  (3)]
⎜   ↳ Logitech USB Optical Mouse           id=12   [slave  pointer  (2)]
⎜   ↳ AT Translated Set 2 keyboard          id=14   [slave  keyboard (3)]
";
        let devices = parse_xinput_list(short);
        assert_eq!(devices.len(), 3);
        assert_eq!(devices[1].id, 12);
        assert_eq!(devices[1].name, "Logitech USB Optical Mouse");
        assert_eq!(devices[2].id, 14);
        assert_eq!(devices[2].name, "AT Translated Set 2 keyboard");
    }

    #[test]
    fn malformed_xinput_ids_are_skipped() {
        let output = "good id=7\nbad id=not-a-number\nno id here\n";
        let devices = parse_xinput_list(output);
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id, 7);
    }

    #[test]
    fn xinput_commands_are_exact_and_use_display_env() {
        let command = RouteCommand::new("xinput")
            .args(["map-to-output", "12", "HDMI-1"])
            .with_display(":1");
        assert_eq!(
            command.argv(),
            vec!["xinput", "map-to-output", "12", "HDMI-1"]
        );
        assert_eq!(command.env.get("DISPLAY").map(String::as_str), Some(":1"));
    }

    #[test]
    fn grab_commands_are_models_not_executions() {
        let group = InputGroup {
            hub_path: "1-1".to_owned(),
            keyboards: vec!["/dev/input/event0".to_owned()],
            mice: vec!["/dev/input/event1".to_owned()],
            gamepads: Vec::new(),
            other: Vec::new(),
        };
        let grabs = EvdevGrabRouter::grab_commands(&group, "seat-0");
        assert_eq!(grabs.len(), 2);
        assert!(grabs.iter().all(|command| command.grab));
        assert_eq!(grabs[0].request, EVIOCGRAB_REQUEST);
        assert_eq!(grabs[0].ioctl, "EVIOCGRAB");

        let releases = EvdevGrabRouter::release_commands(&group, "seat-0");
        assert!(releases.iter().all(|command| !command.grab));
    }

    #[test]
    fn uinput_environment_names_the_seat_and_display() {
        let env = EvdevGrabRouter::environment("host-seat-1", ":1");
        assert_eq!(env.get("DISPLAY").map(String::as_str), Some(":1"));
        assert_eq!(
            env.get(OZMA_SEAT_ID_ENV).map(String::as_str),
            Some("host-seat-1")
        );
        assert_eq!(
            env.get(OZMA_UINPUT_SEAT_ENV).map(String::as_str),
            Some("host-seat-1")
        );
        assert_eq!(
            env.get(OZMA_INPUT_ROUTER_ENV).map(String::as_str),
            Some(EVDEV_ROUTER_NAME)
        );
    }

    #[test]
    fn unsupported_grab_backend_never_reports_success() {
        let mut router = EvdevGrabRouter::new();
        let group = InputGroup {
            hub_path: "1-1".to_owned(),
            keyboards: vec!["/dev/input/event0".to_owned()],
            ..InputGroup::default()
        };
        let target = InputTarget::new("host-seat-0", "HDMI-1", ":0").expect("target");
        let error = router.assign(&group, &target).expect_err("must fail");
        assert!(matches!(error, MultiseatError::Unsupported(_)));
        assert!(router.status().is_empty());
    }

    #[test]
    fn input_target_rejects_missing_output_or_screen() {
        let error = InputTarget::new("host-seat-0", "", ":0").expect_err("output");
        assert!(matches!(error, MultiseatError::InputRouting(_)));
        let error = InputTarget::new("host-seat-0", "HDMI-1", "").expect_err("screen");
        assert!(matches!(error, MultiseatError::InputRouting(_)));
    }
}
