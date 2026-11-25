// SPDX-License-Identifier: AGPL-3.0-only
//! Per-display XInput targeting for helper-created uinput devices.
//!
//! The privileged helper grabs a seat's physical devices and relays them to
//! per-seat uinput devices named `ozma-seat-<seat>-<kind>-<event>`. A private X
//! server can still expose uinput devices belonging to other seats, so display
//! isolation is not complete until the server is restricted to one seat's
//! devices.
//!
//! [`SeatInputTargeter`] is the injectable seam. It lists devices with
//! `xinput list --short` on the seat's own `DISPLAY`, enables the seat's
//! helper-confirmed uinput names, and disables every other `ozma-seat-...`
//! device. Core X devices (`Virtual core pointer`, `Virtual core keyboard`,
//! XTEST devices) and any non-helper device are never touched, so a seat keeps
//! working if its pointer or keyboard is a core/XTEST fallback.
//!
//! The real implementation shells out to `xinput` with a bounded per-command
//! timeout. Tests inject a fake targeter or a scripted command runner; nothing
//! in the default test suite starts an X server or runs `xinput`.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::MultiseatError;
use crate::input_isolation::is_uinput_name;
use crate::routing::{parse_xinput_list, CommandOutput, CommandRunner, RouteCommand};

/// Default program used to inspect and target devices.
pub const TARGETING_PROGRAM: &str = "xinput";
/// Default bound for one `xinput` invocation.
pub const DEFAULT_INPUT_TARGET_TIMEOUT: Duration = Duration::from_secs(5);

/// Policy for a failed private-display input target application.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputTargetingMode {
    /// Do not attempt to restrict a private display.
    Off,
    /// Record a typed error and warning, but continue the seat (default).
    #[default]
    Warn,
    /// Record a typed error and prevent the seat child from spawning.
    Strict,
}

impl InputTargetingMode {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Warn => "warn",
            Self::Strict => "strict",
        }
    }
}

impl std::fmt::Display for InputTargetingMode {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::str::FromStr for InputTargetingMode {
    type Err = MultiseatError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "off" => Ok(Self::Off),
            "warn" => Ok(Self::Warn),
            "strict" => Ok(Self::Strict),
            other => Err(MultiseatError::InputTargeting(format!(
                "unknown input targeting mode {other:?}; expected off, warn, or strict"
            ))),
        }
    }
}

/// One request to restrict a private X display to a seat's uinput devices.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SeatInputTargetRequest {
    /// Seat whose devices should remain enabled.
    pub seat_id: String,
    /// Private X display, for example `:90`.
    pub display: String,
    /// Cookie file for that display, when auth is enabled.
    pub xauthority: Option<PathBuf>,
    /// Helper-confirmed uinput names that belong to this seat.
    pub devices: Vec<String>,
}

/// Result of a successful display restriction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatInputTargeting {
    /// Seat id.
    pub seat_id: String,
    /// Display that was targeted.
    pub display: String,
    /// Own uinput device names that were enabled.
    pub enabled: Vec<String>,
    /// Other seats' uinput device names that were disabled.
    pub disabled: Vec<String>,
}

/// Result of releasing a display restriction.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SeatInputTargetingRelease {
    /// Seat id.
    pub seat_id: String,
    /// Other-seat device names that were re-enabled, or would have been if the
    /// display still had them.
    pub reenabled: Vec<String>,
    /// Non-fatal release problems.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
}

impl SeatInputTargeting {
    /// Whether this targeting was applied to the same display and device set.
    pub fn matches(&self, display: &str, devices: &[String]) -> bool {
        self.display == display
            && self.enabled.len() == devices.len()
            && devices
                .iter()
                .all(|device| self.enabled.iter().any(|own| own == device))
    }
}

/// Injectable private-display input targeter.
///
/// Production uses [`ProcessSeatInputTargeter`]. Tests use
/// `crate::fake::FakeSeatInputTargeter` so no X server or `xinput` process is
/// touched.
pub trait SeatInputTargeter: Send {
    /// Restrict `request.display` to `request.devices`.
    fn apply(
        &mut self,
        request: &SeatInputTargetRequest,
    ) -> Result<SeatInputTargeting, MultiseatError>;

    /// Re-enable other-seat devices that this seat disabled.
    fn release(&mut self, seat_id: &str) -> Result<SeatInputTargetingRelease, MultiseatError>;

    /// Last successful targeting for `seat_id`.
    fn status_for(&self, seat_id: &str) -> Option<SeatInputTargeting>;

    /// Forget local state without running commands. Used when the private X
    /// server that held the devices is already gone.
    fn clear(&mut self, seat_id: &str);
}

#[derive(Clone)]
struct AppliedTargeting {
    targeting: SeatInputTargeting,
    xauthority: Option<PathBuf>,
}

/// Real targeter backed by `xinput`.
pub struct ProcessSeatInputTargeter {
    runner: Box<dyn CommandRunner + Send>,
    program: String,
    applied: BTreeMap<String, AppliedTargeting>,
}

impl Default for ProcessSeatInputTargeter {
    fn default() -> Self {
        Self::new()
    }
}

impl ProcessSeatInputTargeter {
    /// Build a targeter that runs `xinput` with bounded timeouts.
    pub fn new() -> Self {
        Self {
            runner: Box::new(BoundedProcessCommandRunner::new(
                DEFAULT_INPUT_TARGET_TIMEOUT,
            )),
            program: TARGETING_PROGRAM.to_owned(),
            applied: BTreeMap::new(),
        }
    }

    /// Build a targeter that runs commands through an injected runner. Tests
    /// use this with `FakeCommandRunner` to inspect argv without running
    /// `xinput`.
    pub fn with_runner(runner: Box<dyn CommandRunner + Send>) -> Self {
        Self {
            runner,
            program: TARGETING_PROGRAM.to_owned(),
            applied: BTreeMap::new(),
        }
    }

    /// Override the `xinput` program name or path.
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
    }

    fn command(&self, display: &str, xauthority: Option<&PathBuf>, args: &[&str]) -> RouteCommand {
        let mut command = RouteCommand::new(&self.program)
            .args(args.iter().copied())
            .with_display(display);
        if let Some(xauthority) = xauthority {
            command = command.with_env("XAUTHORITY", xauthority.display().to_string());
        }
        command
    }

    fn list_device_command(&self, display: &str, xauthority: Option<&PathBuf>) -> RouteCommand {
        self.command(display, xauthority, &["list", "--short"])
    }

    fn set_enabled_command(
        &self,
        display: &str,
        xauthority: Option<&PathBuf>,
        enable: bool,
        id: u32,
    ) -> RouteCommand {
        let id = id.to_string();
        let action = if enable { "enable" } else { "disable" };
        self.command(display, xauthority, &[action, &id])
    }

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

    fn list_devices(
        &self,
        display: &str,
        xauthority: Option<&PathBuf>,
    ) -> Result<Vec<crate::routing::XInputDevice>, MultiseatError> {
        let command = self.list_device_command(display, xauthority);
        let output = self.run_checked(&command)?;
        Ok(parse_xinput_list(&output.stdout))
    }
}

impl SeatInputTargeter for ProcessSeatInputTargeter {
    fn apply(
        &mut self,
        request: &SeatInputTargetRequest,
    ) -> Result<SeatInputTargeting, MultiseatError> {
        let display = request.display.trim();
        if display.is_empty() {
            return Err(MultiseatError::InputTargeting(format!(
                "seat {} has no X display to target",
                request.seat_id
            )));
        }
        if request.devices.is_empty() {
            return Err(MultiseatError::InputTargeting(format!(
                "seat {} has no confirmed uinput devices to target",
                request.seat_id
            )));
        }

        let own: BTreeSet<String> = request.devices.iter().cloned().collect();
        let devices = self.list_devices(display, request.xauthority.as_ref())?;
        if devices.is_empty() {
            return Err(MultiseatError::InputTargeting(format!(
                "xinput on {display} reported no devices for seat {}",
                request.seat_id
            )));
        }

        // Validate the complete own set before changing any device. A private
        // server that does not expose the helper's uinput devices cannot be
        // claimed as targeted.
        let mut own_ids = Vec::new();
        let mut missing = Vec::new();
        for name in &own {
            match devices.iter().find(|device| &device.name == name) {
                Some(device) => own_ids.push(device.id),
                None => missing.push(name.clone()),
            }
        }
        if !missing.is_empty() {
            return Err(MultiseatError::InputTargeting(format!(
                "seat {} uinput device(s) not visible on {display}: {}; the private X server is \
                 not consuming the helper's devices",
                request.seat_id,
                missing.join(", ")
            )));
        }

        let foreign: Vec<_> = devices
            .iter()
            .filter(|device| !own.contains(&device.name) && is_uinput_name(&device.name))
            .collect();

        let mut disabled_ids = Vec::new();
        let mut disabled_names = Vec::new();
        let mut failure = None;
        for device in &foreign {
            let command =
                self.set_enabled_command(display, request.xauthority.as_ref(), false, device.id);
            match self.run_checked(&command) {
                Ok(_) => {
                    disabled_ids.push(device.id);
                    disabled_names.push(device.name.clone());
                }
                Err(error) => {
                    failure = Some(error);
                    break;
                }
            }
        }

        if let Some(error) = failure {
            // Re-enable every other-seat device already disabled so a partial
            // restriction is not left behind.
            for id in disabled_ids.iter().rev() {
                let command =
                    self.set_enabled_command(display, request.xauthority.as_ref(), true, *id);
                let _ = self.runner.run(&command);
            }
            return Err(error);
        }

        let mut enabled = Vec::new();
        for (name, id) in own.iter().zip(own_ids) {
            let command = self.set_enabled_command(display, request.xauthority.as_ref(), true, id);
            if let Err(error) = self.run_checked(&command) {
                for disabled in disabled_ids.iter().rev() {
                    let command = self.set_enabled_command(
                        display,
                        request.xauthority.as_ref(),
                        true,
                        *disabled,
                    );
                    let _ = self.runner.run(&command);
                }
                return Err(error);
            }
            enabled.push(name.clone());
        }
        enabled.sort();

        let targeting = SeatInputTargeting {
            seat_id: request.seat_id.clone(),
            display: display.to_owned(),
            enabled,
            disabled: {
                disabled_names.sort();
                disabled_names
            },
        };
        self.applied.insert(
            request.seat_id.clone(),
            AppliedTargeting {
                targeting: targeting.clone(),
                xauthority: request.xauthority.clone(),
            },
        );
        Ok(targeting)
    }

    fn release(&mut self, seat_id: &str) -> Result<SeatInputTargetingRelease, MultiseatError> {
        let Some(applied) = self.applied.get(seat_id).cloned() else {
            return Ok(SeatInputTargetingRelease {
                seat_id: seat_id.to_owned(),
                reenabled: Vec::new(),
                warnings: Vec::new(),
            });
        };
        if applied.targeting.disabled.is_empty() {
            self.applied.remove(seat_id);
            return Ok(SeatInputTargetingRelease {
                seat_id: seat_id.to_owned(),
                reenabled: Vec::new(),
                warnings: Vec::new(),
            });
        }

        let devices =
            match self.list_devices(&applied.targeting.display, applied.xauthority.as_ref()) {
                Ok(devices) => devices,
                Err(error) => {
                    // Keep state so a later stop while the display is still alive
                    // can retry the re-enable.
                    return Err(MultiseatError::InputTargeting(format!(
                        "listing {} to release seat {seat_id} targeting: {error}",
                        applied.targeting.display
                    )));
                }
            };

        let mut reenabled = Vec::new();
        let mut warnings = Vec::new();
        let mut failures = Vec::new();
        for name in &applied.targeting.disabled {
            let Some(device) = devices.iter().find(|device| &device.name == name) else {
                warnings.push(format!(
                    "{name} is no longer present on {}",
                    applied.targeting.display
                ));
                continue;
            };
            let command = self.set_enabled_command(
                &applied.targeting.display,
                applied.xauthority.as_ref(),
                true,
                device.id,
            );
            match self.run_checked(&command) {
                Ok(_) => reenabled.push(name.clone()),
                Err(error) => failures.push(format!("{name}: {error}")),
            }
        }

        if !failures.is_empty() {
            return Err(MultiseatError::InputTargeting(format!(
                "could not re-enable every other-seat device for {seat_id}: {}",
                failures.join("; ")
            )));
        }

        self.applied.remove(seat_id);
        Ok(SeatInputTargetingRelease {
            seat_id: seat_id.to_owned(),
            reenabled,
            warnings,
        })
    }

    fn status_for(&self, seat_id: &str) -> Option<SeatInputTargeting> {
        self.applied
            .get(seat_id)
            .map(|applied| applied.targeting.clone())
    }

    fn clear(&mut self, seat_id: &str) {
        self.applied.remove(seat_id);
    }
}

/// Command runner that kills an `xinput` invocation after a fixed budget.
///
/// The routing `ProcessCommandRunner` deliberately has no timeout so ownership
/// commands keep their historical behaviour. Targeting is applied while a seat
/// is starting, so a wedged X server must not stall the supervisor forever.
pub struct BoundedProcessCommandRunner {
    timeout: Duration,
}

impl BoundedProcessCommandRunner {
    /// Build a runner with an explicit per-command timeout.
    pub fn new(timeout: Duration) -> Self {
        Self {
            timeout: timeout.max(Duration::from_millis(1)),
        }
    }
}

impl CommandRunner for BoundedProcessCommandRunner {
    fn run(&self, command: &RouteCommand) -> Result<CommandOutput, MultiseatError> {
        let mut child = Command::new(&command.program)
            .args(&command.args)
            .envs(&command.env)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
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

        let deadline = Instant::now() + self.timeout;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) => {}
                Err(error) => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(MultiseatError::Io {
                        path: command.program.clone(),
                        source: error,
                    });
                }
            }
            if Instant::now() >= deadline {
                let _ = child.kill();
                let _ = child.wait();
                return Err(MultiseatError::InputTargeting(format!(
                    "{} {:?} timed out after {:?}",
                    command.program, command.args, self.timeout
                )));
            }
            std::thread::sleep(Duration::from_millis(10));
        }

        let output = child
            .wait_with_output()
            .map_err(|error| MultiseatError::Io {
                path: command.program.clone(),
                source: error,
            })?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}
