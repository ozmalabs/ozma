// SPDX-License-Identifier: AGPL-3.0-only
//! Subprocess execution used by the libvirt/virsh backend.
//!
//! Commands are always passed as an argv vector; no shell is involved and no
//! string interpolation or quoting is performed. This keeps a domain name such
//! as `my vm` a single argument instead of two shell words.

use std::io;
use std::process::Stdio;

use async_trait::async_trait;
use tokio::process::Command;

use crate::error::{PowerError, VIRSH_REMEDIATION};

/// Captured result of a successful subprocess.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandOutput {
    /// Exit status (`0` on success).
    pub status: i32,
    /// Captured standard output.
    pub stdout: String,
    /// Captured standard error.
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

    /// Convenience constructor for a successful command with stdout.
    pub fn success(stdout: impl Into<String>) -> Self {
        Self::new(0, stdout, "")
    }
}

/// Runs a full argv (program in element 0).
///
/// Implementations must capture stdout, stderr, and the exit status. A missing
/// executable and a non-zero exit must become
/// [`PowerError::MissingCommand`] / [`PowerError::CommandFailed`].
#[async_trait]
pub trait CommandRunner: Send + Sync {
    /// Run `argv`, returning captured output or a typed failure.
    async fn run(&self, argv: &[String]) -> Result<CommandOutput, PowerError>;
}

/// The production runner: `tokio::process::Command` with no shell.
#[derive(Debug, Default, Clone, Copy)]
pub struct TokioCommandRunner;

#[async_trait]
impl CommandRunner for TokioCommandRunner {
    async fn run(&self, argv: &[String]) -> Result<CommandOutput, PowerError> {
        let Some((program, arguments)) = argv.split_first() else {
            return Err(PowerError::InvalidConfig {
                field: "argv",
                reason: "command argv is empty".to_string(),
            });
        };

        let rendered = render_command(argv);
        let output = Command::new(program)
            .args(arguments)
            .stdin(Stdio::null())
            .output()
            .await
            .map_err(|source| {
                if source.kind() == io::ErrorKind::NotFound {
                    PowerError::MissingCommand {
                        command: program.clone(),
                        remediation: VIRSH_REMEDIATION.to_string(),
                    }
                } else {
                    PowerError::Spawn {
                        command: rendered.clone(),
                        source,
                    }
                }
            })?;

        let status = output.status.code().unwrap_or(-1);
        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();

        if !output.status.success() {
            return Err(PowerError::CommandFailed {
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

/// Render an argv for diagnostics and error messages.
///
/// This is display-only; it is never passed to a shell. Arguments containing
/// whitespace are quoted so the rendered command is unambiguous.
pub(crate) fn render_command(argv: &[String]) -> String {
    argv.iter()
        .map(|argument| {
            if argument.is_empty() || argument.chars().any(char::is_whitespace) {
                format!("{argument:?}")
            } else {
                argument.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}
