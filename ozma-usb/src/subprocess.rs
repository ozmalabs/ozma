// SPDX-License-Identifier: AGPL-3.0-only
//! Subprocess fallback for USB/IP and system tools.
//!
//! Commands are always built as an argv vector and passed directly to
//! [`std::process::Command`]; there is no shell and no string interpolation.
//! Bus IDs, hosts, and ports are validated and may not begin with `-`, so a
//! caller cannot smuggle an extra option into `usbip`.

use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
pub use crate::usbip::UsbIpDevice;
use crate::usbip::{parse_usbip_list, validate_bus_id, validate_host, validate_port_text};

/// One argv-style command. `program` is element zero; it is never a shell.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    /// Executable name or path.
    pub program: String,
    /// Arguments, passed verbatim.
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Build a command from a program and arguments.
    pub fn new<I, S>(program: impl Into<String>, args: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Self {
            program: program.into(),
            args: args.into_iter().map(Into::into).collect(),
        }
    }

    /// Program followed by arguments.
    pub fn argv(&self) -> Vec<String> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.clone());
        argv.extend(self.args.iter().cloned());
        argv
    }
}

/// Captured result of a successful subprocess.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status (`0` on success).
    pub status: i32,
    /// Captured standard output.
    pub stdout: String,
    /// Trimmed standard error.
    pub stderr: String,
}

impl CommandOutput {
    /// Build an output value, typically from a test fake.
    pub fn new(status: i32, stdout: impl Into<String>, stderr: impl Into<String>) -> Self {
        Self {
            status,
            stdout: stdout.into(),
            stderr: stderr.into(),
        }
    }

    /// Successful output with stdout.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self::new(0, stdout, "")
    }
}

/// Runs a full argv without a shell.
pub trait CommandRunner: Send + Sync {
    /// Run `command` and capture output. Non-zero exits are typed failures.
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, UsbError>;
}

/// Production runner using [`std::process::Command`].
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemCommandRunner;

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &CommandSpec) -> Result<CommandOutput, UsbError> {
        if command.program.trim().is_empty() {
            return Err(UsbError::InvalidField {
                field: "command",
                reason: "program must not be empty".to_owned(),
            });
        }

        let rendered = render_command(command);
        let output = Command::new(&command.program)
            .args(&command.args)
            .stdin(Stdio::null())
            .output()
            .map_err(|source| {
                if source.kind() == std::io::ErrorKind::NotFound {
                    UsbError::MissingCommand {
                        command: command.program.clone(),
                        remediation: "install the usbip tools (linux-tools package)".to_owned(),
                    }
                } else {
                    UsbError::Spawn {
                        command: rendered.clone(),
                        source,
                    }
                }
            })?;

        let status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        if !output.status.success() {
            return Err(UsbError::CommandFailed {
                command: rendered,
                status,
                stderr,
            });
        }

        Ok(CommandOutput {
            status,
            stdout,
            stderr,
        })
    }
}

/// USB/IP operations exposed by a subprocess controller.
pub trait UsbIpControl: Send + Sync {
    /// List local exportable devices.
    fn list_local(&self) -> Result<Vec<UsbIpDevice>, UsbError>;
    /// Export a local device (`usbip bind`).
    fn bind(&self, bus_id: &str) -> Result<(), UsbError>;
    /// Stop exporting a local device (`usbip unbind`).
    fn unbind(&self, bus_id: &str) -> Result<(), UsbError>;
    /// Import a remote device (`usbip attach`).
    fn attach(&self, host: &str, bus_id: &str) -> Result<(), UsbError>;
    /// Detach an imported device by port (`usbip detach`).
    fn detach(&self, port: &str) -> Result<(), UsbError>;
}

/// A USB/IP controller backed by a configurable command runner.
#[derive(Debug, Clone)]
pub struct SubprocessUsbController<R = SystemCommandRunner> {
    runner: R,
    program: String,
}

impl SubprocessUsbController<SystemCommandRunner> {
    /// Use `usbip` from `PATH`.
    pub fn system() -> Self {
        Self::new(SystemCommandRunner, "usbip")
    }
}

impl<R: CommandRunner> SubprocessUsbController<R> {
    /// Build a controller with an injected runner and program.
    pub fn new(runner: R, program: impl Into<String>) -> Self {
        Self {
            runner,
            program: program.into(),
        }
    }

    /// The injected runner.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// The configured program (normally `usbip`).
    pub fn program(&self) -> &str {
        &self.program
    }

    fn run(&self, args: &[&str]) -> Result<CommandOutput, UsbError> {
        let command = CommandSpec::new(
            self.program.clone(),
            args.iter().map(|argument| (*argument).to_owned()),
        );
        self.runner.run(&command)
    }

    /// Parse `usbip list -l` output without running anything.
    pub fn parse_list(output: &str) -> Vec<UsbIpDevice> {
        parse_usbip_list(output)
    }
}

impl<R: CommandRunner> UsbIpControl for SubprocessUsbController<R> {
    fn list_local(&self) -> Result<Vec<UsbIpDevice>, UsbError> {
        let output = self.run(&["list", "-l"])?;
        Ok(parse_usbip_list(&output.stdout))
    }

    fn bind(&self, bus_id: &str) -> Result<(), UsbError> {
        validate_bus_id(bus_id)?;
        self.run(&["bind", "-b", bus_id]).map(|_| ())
    }

    fn unbind(&self, bus_id: &str) -> Result<(), UsbError> {
        validate_bus_id(bus_id)?;
        self.run(&["unbind", "-b", bus_id]).map(|_| ())
    }

    fn attach(&self, host: &str, bus_id: &str) -> Result<(), UsbError> {
        validate_host(host)?;
        validate_bus_id(bus_id)?;
        self.run(&["attach", "-r", host, "-b", bus_id]).map(|_| ())
    }

    fn detach(&self, port: &str) -> Result<(), UsbError> {
        validate_port_text(port)?;
        self.run(&["detach", "-p", port]).map(|_| ())
    }
}

fn render_command(command: &CommandSpec) -> String {
    std::iter::once(command.program.clone())
        .chain(command.args.iter().cloned())
        .map(|argument| {
            if argument.is_empty() || argument.chars().any(char::is_whitespace) {
                format!("{argument:?}")
            } else {
                argument
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
