// SPDX-License-Identifier: AGPL-3.0-only
//! Per-seat display ownership models and xrandr command builders.
//!
//! The supported Linux path is an **Xrandr output** on one X server: each seat
//! gets an output name, a mode, and a position. [`DisplayOwnership`] models that
//! intent and builds `xrandr --output <output> --mode <mode> --pos <x>x<y>`
//! commands. Execution goes through an injected
//! [`crate::routing::CommandRunner`], so tests inspect argv and never change a
//! real mode.
//!
//! ## Single-X-server limitation
//!
//! Xrandr output positioning is **not** seat isolation. All outputs share one
//! root window and one input focus. Keyboard focus, the window manager, and
//! `ozma-node`'s uinput injection still target the one X server. True ownership
//! needs one of:
//!
//! * separate X screens (`:0.0`, `:0.1`) configured in `xorg.conf`, which this
//!   crate does not create;
//! * one X server (Xephyr/Xorg) or a compositor session per seat;
//! * DRM leasing / a Wayland compositor with per-seat outputs, not implemented
//!   here.
//!
//! The model is deliberately explicit about this: [`DisplayOwnershipKind`]
//! distinguishes [`XrandrOutput`](DisplayOwnershipKind::XrandrOutput) from the
//! unimplemented screen/server/compositor kinds, and applying a non-xrandr kind
//! is a typed [`MultiseatError::Unsupported`] error.

use serde::{Deserialize, Serialize};

use crate::display::DisplayOutput;
use crate::error::MultiseatError;
use crate::routing::{CommandOutput, CommandRunner, RouteCommand};

/// Default `xrandr` program name.
pub const XRANDR_PROGRAM: &str = "xrandr";

/// Environment variable naming the ownership kind.
pub const OZMA_DISPLAY_OWNERSHIP_ENV: &str = "OZMA_DISPLAY_OWNERSHIP";

/// Environment variable naming the X output owned by the seat.
pub const OZMA_X11_OUTPUT_ENV: &str = "OZMA_X11_OUTPUT";

/// Environment variable naming the X mode owned by the seat.
pub const OZMA_X11_MODE_ENV: &str = "OZMA_X11_MODE";

/// Human-readable limitation attached to diagnostics and docs.
pub const SINGLE_X_SERVER_LIMITATION: &str = "xrandr output positioning shares one X \
     root window and one keyboard focus; it captures a region and routes pointers, but it is \
     not per-seat screen isolation. Separate X screens, a per-seat X server/compositor, or DRM \
     leasing are required for true display ownership.";

/// How a seat's display is owned.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DisplayOwnershipKind {
    /// Distinct xrandr output on one X server. Implemented as a command model.
    XrandrOutput,
    /// Distinct X screen on one server. Requires `xorg.conf`; not created here.
    XScreen,
    /// Separate X server or compositor process per seat. Not implemented.
    SeparateServer,
    /// Wayland compositor seat. Not implemented.
    WaylandCompositor,
}

impl DisplayOwnershipKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::XrandrOutput => "xrandr-output",
            Self::XScreen => "x-screen",
            Self::SeparateServer => "separate-server",
            Self::WaylandCompositor => "wayland-compositor",
        }
    }
}

/// One seat's display ownership intent.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DisplayOwnership {
    /// Seat id.
    pub seat_id: String,
    /// Ownership kind.
    pub kind: DisplayOwnershipKind,
    /// X display string, for example `:0`.
    pub x_screen: String,
    /// Xrandr output name, for example `HDMI-1`.
    pub output: String,
    /// Requested mode name, normally `WIDTHxHEIGHT`.
    pub mode: String,
    /// Mode width in pixels.
    pub width: u32,
    /// Mode height in pixels.
    pub height: u32,
    /// X position within the root screen.
    pub x: i32,
    /// Y position within the root screen.
    pub y: i32,
    /// Whether this output is marked primary.
    #[serde(default)]
    pub primary: bool,
    /// Whether this ownership refers to a virtual display.
    #[serde(default)]
    pub virtual_display: bool,
}

impl DisplayOwnership {
    /// Build xrandr ownership from an enumerated output.
    pub fn from_output(
        seat_id: impl Into<String>,
        display: &DisplayOutput,
        x_screen: impl Into<String>,
    ) -> Self {
        Self {
            seat_id: seat_id.into(),
            kind: DisplayOwnershipKind::XrandrOutput,
            x_screen: x_screen.into(),
            output: display.name.clone(),
            mode: format!("{}x{}", display.width, display.height),
            width: display.width,
            height: display.height,
            x: display.x,
            y: display.y,
            primary: display.primary,
            virtual_display: display.virtual_display,
        }
    }

    /// Override the mode name while keeping the output geometry.
    pub fn with_mode(mut self, mode: impl Into<String>) -> Self {
        self.mode = mode.into();
        self
    }

    /// Validate the values needed for a command.
    pub fn validate(&self) -> Result<(), MultiseatError> {
        if self.seat_id.trim().is_empty() {
            return Err(MultiseatError::DisplayOwnership(
                "display ownership is missing a seat id".to_owned(),
            ));
        }
        if self.output.trim().is_empty() {
            return Err(MultiseatError::DisplayOwnership(format!(
                "seat {} has no X output for display ownership",
                self.seat_id
            )));
        }
        if self.x_screen.trim().is_empty() {
            return Err(MultiseatError::DisplayOwnership(format!(
                "seat {} has no X screen for display ownership",
                self.seat_id
            )));
        }
        if self.mode.trim().is_empty() {
            return Err(MultiseatError::DisplayOwnership(format!(
                "seat {} has no mode for output {}",
                self.seat_id, self.output
            )));
        }
        if self.width == 0 || self.height == 0 {
            return Err(MultiseatError::DisplayOwnership(format!(
                "seat {} output {} has invalid geometry {}x{}",
                self.seat_id, self.output, self.width, self.height
            )));
        }
        Ok(())
    }

    /// `--pos` value, `<x>x<y>`.
    pub fn position(&self) -> String {
        format!("{}x{}", self.x, self.y)
    }

    /// Arguments to `xrandr` for this ownership.
    pub fn xrandr_args(&self) -> Vec<String> {
        let mut args = vec![
            "--output".to_owned(),
            self.output.clone(),
            "--mode".to_owned(),
            self.mode.clone(),
            "--pos".to_owned(),
            self.position(),
        ];
        if self.primary {
            args.push("--primary".to_owned());
        }
        args
    }

    /// Full `xrandr` command with `DISPLAY` set.
    pub fn apply_command(&self, program: &str) -> RouteCommand {
        RouteCommand::new(program)
            .args(self.xrandr_args())
            .with_display(&self.x_screen)
    }

    /// Best-effort release arguments. The previous mode/position is unknown, so
    /// this asks xrandr for the output's auto mode; it does not restore a prior
    /// layout.
    pub fn release_args(&self) -> Vec<String> {
        vec![
            "--output".to_owned(),
            self.output.clone(),
            "--auto".to_owned(),
        ]
    }

    /// Full best-effort release command with `DISPLAY` set.
    pub fn release_command(&self, program: &str) -> RouteCommand {
        RouteCommand::new(program)
            .args(self.release_args())
            .with_display(&self.x_screen)
    }

    /// Environment additions for the node process.
    pub fn environment(&self) -> std::collections::BTreeMap<String, String> {
        let mut env = std::collections::BTreeMap::new();
        if !self.x_screen.trim().is_empty() {
            env.insert("DISPLAY".to_owned(), self.x_screen.clone());
        }
        env.insert("OZMA_SEAT_ID".to_owned(), self.seat_id.clone());
        env.insert(
            OZMA_DISPLAY_OWNERSHIP_ENV.to_owned(),
            self.kind.as_str().to_owned(),
        );
        env.insert(OZMA_X11_OUTPUT_ENV.to_owned(), self.output.clone());
        env.insert(OZMA_X11_MODE_ENV.to_owned(), self.mode.clone());
        env
    }
}

/// Apply/release seam for display ownership.
pub trait DisplayOwnershipController {
    /// Controller name for status/diagnostics.
    fn controller_name(&self) -> &'static str;

    /// Apply one ownership command. `Ok` means the command exited zero and the
    /// ownership is recorded.
    fn apply(&mut self, ownership: &DisplayOwnership) -> Result<(), MultiseatError>;

    /// Release one seat's recorded ownership.
    fn release(&mut self, seat_id: &str) -> Result<(), MultiseatError>;

    /// Currently applied ownerships, ordered by seat id.
    fn status(&self) -> Vec<DisplayOwnership>;
}

/// xrandr-backed ownership controller.
pub struct XrandrOwnershipController {
    runner: Box<dyn CommandRunner>,
    program: String,
    applied: std::collections::BTreeMap<String, DisplayOwnership>,
}

impl XrandrOwnershipController {
    /// Build a controller with `xrandr` on `PATH`.
    pub fn new(runner: Box<dyn CommandRunner>) -> Self {
        Self {
            runner,
            program: XRANDR_PROGRAM.to_owned(),
            applied: std::collections::BTreeMap::new(),
        }
    }

    /// Override the `xrandr` program name or path.
    pub fn with_program(mut self, program: impl Into<String>) -> Self {
        self.program = program.into();
        self
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
}

impl DisplayOwnershipController for XrandrOwnershipController {
    fn controller_name(&self) -> &'static str {
        "xrandr-output"
    }

    fn apply(&mut self, ownership: &DisplayOwnership) -> Result<(), MultiseatError> {
        ownership.validate()?;
        if ownership.kind != DisplayOwnershipKind::XrandrOutput {
            return Err(MultiseatError::Unsupported(format!(
                "{} ownership is not implemented; only xrandr outputs are modelled",
                ownership.kind.as_str()
            )));
        }
        if self.applied.contains_key(&ownership.seat_id) {
            return Err(MultiseatError::DisplayOwnership(format!(
                "seat {} already owns a display; release it first",
                ownership.seat_id
            )));
        }
        if self
            .applied
            .values()
            .any(|applied| applied.output == ownership.output)
        {
            return Err(MultiseatError::DisplayOwnership(format!(
                "output {} is already owned by another seat",
                ownership.output
            )));
        }

        self.run_checked(&ownership.apply_command(&self.program))?;
        self.applied
            .insert(ownership.seat_id.clone(), ownership.clone());
        Ok(())
    }

    fn release(&mut self, seat_id: &str) -> Result<(), MultiseatError> {
        let Some(ownership) = self.applied.get(seat_id).cloned() else {
            return Err(MultiseatError::DisplayOwnership(format!(
                "seat {seat_id} has no display ownership to release"
            )));
        };

        self.run_checked(&ownership.release_command(&self.program))?;
        self.applied.remove(seat_id);
        Ok(())
    }

    fn status(&self) -> Vec<DisplayOwnership> {
        self.applied.values().cloned().collect()
    }
}

/// Apply ownership for several seats, rolling back earlier seats on failure.
///
/// Duplicate seat ids and duplicate outputs are rejected before the first
/// command runs. This function never runs a command itself; it delegates to the
/// injected controller.
pub fn apply_ownership(
    controller: &mut dyn DisplayOwnershipController,
    ownerships: &[DisplayOwnership],
) -> Result<Vec<DisplayOwnership>, MultiseatError> {
    let mut seat_ids = std::collections::BTreeSet::new();
    let mut outputs = std::collections::BTreeSet::new();
    for ownership in ownerships {
        ownership.validate()?;
        if !seat_ids.insert(ownership.seat_id.clone()) {
            return Err(MultiseatError::DisplayOwnership(format!(
                "duplicate seat id {:?} in ownership plan",
                ownership.seat_id
            )));
        }
        if !outputs.insert(ownership.output.clone()) {
            return Err(MultiseatError::DisplayOwnership(format!(
                "output {:?} is assigned to more than one seat",
                ownership.output
            )));
        }
    }

    let mut applied = Vec::with_capacity(ownerships.len());
    for ownership in ownerships {
        match controller.apply(ownership) {
            Ok(()) => applied.push(ownership.clone()),
            Err(error) => {
                let mut rollback_failures = Vec::new();
                for previous in applied.iter().rev() {
                    if let Err(rollback) = controller.release(&previous.seat_id) {
                        rollback_failures.push(rollback.to_string());
                    }
                }
                if rollback_failures.is_empty() {
                    return Err(error);
                }
                return Err(MultiseatError::DisplayOwnership(format!(
                    "{error}; rollback also failed: {}",
                    rollback_failures.join("; ")
                )));
            }
        }
    }
    Ok(applied)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ownership() -> DisplayOwnership {
        DisplayOwnership {
            seat_id: "host-seat-0".to_owned(),
            kind: DisplayOwnershipKind::XrandrOutput,
            x_screen: ":0".to_owned(),
            output: "HDMI-1".to_owned(),
            mode: "1920x1080".to_owned(),
            width: 1920,
            height: 1080,
            x: 1920,
            y: 0,
            primary: false,
            virtual_display: false,
        }
    }

    #[test]
    fn xrandr_argv_has_output_mode_and_position() {
        assert_eq!(
            ownership().xrandr_args(),
            vec![
                "--output",
                "HDMI-1",
                "--mode",
                "1920x1080",
                "--pos",
                "1920x0",
            ]
        );
        let command = ownership().apply_command("xrandr");
        assert_eq!(command.argv()[0], "xrandr");
        assert_eq!(command.env.get("DISPLAY").map(String::as_str), Some(":0"));
    }

    #[test]
    fn primary_output_adds_primary_flag() {
        let mut ownership = ownership();
        ownership.primary = true;
        assert!(ownership.xrandr_args().last() == Some(&"--primary".to_owned()));
    }

    #[test]
    fn ownership_rejects_invalid_geometry_and_missing_screen() {
        let mut ownership = ownership();
        ownership.width = 0;
        assert!(matches!(
            ownership.validate(),
            Err(MultiseatError::DisplayOwnership(_))
        ));

        let mut ownership = self::ownership();
        ownership.x_screen.clear();
        assert!(ownership.validate().is_err());
    }

    #[test]
    fn environment_names_output_and_mode() {
        let env = ownership().environment();
        assert_eq!(
            env.get(OZMA_X11_OUTPUT_ENV).map(String::as_str),
            Some("HDMI-1")
        );
        assert_eq!(
            env.get(OZMA_X11_MODE_ENV).map(String::as_str),
            Some("1920x1080")
        );
        assert_eq!(
            env.get(OZMA_DISPLAY_OWNERSHIP_ENV).map(String::as_str),
            Some("xrandr-output")
        );
    }
}
