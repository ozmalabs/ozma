// SPDX-License-Identifier: AGPL-3.0-only
//! libvirt power backend using the `virsh` subprocess fallback.
//!
//! The Python `softnode/libvirt_power.py` prefers the libvirt Python API and
//! falls back to `virsh` subprocesses. This crate intentionally implements the
//! fallback only (there is no libvirt crate dependency), with the same argv the
//! Python fallback uses:
//!
//! | Operation | argv |
//! |---|---|
//! | connect / status | `virsh domstate <domain>` |
//! | cont | `virsh resume <domain>`, then `virsh start <domain>` |
//! | shutdown | `virsh shutdown <domain>` |
//! | reset | `virsh reset <domain>` |
//! | stop | `virsh destroy <domain>` |
//!
//! The configured URI is validated and retained, but — exactly like Python's
//! `_virsh` helper — it is not passed as `--connect`; `virsh` uses its default
//! connection (normally `qemu:///system`).

use std::fmt;

use async_trait::async_trait;
use tracing::{debug, info};

use crate::error::PowerError;
use crate::runner::{render_command, CommandRunner, TokioCommandRunner};
use crate::state::PowerState;
use crate::PowerBackend;

/// Default libvirt connection URI (Python default).
pub const DEFAULT_LIBVIRT_URI: &str = "qemu:///system";

/// Power control for one libvirt domain through `virsh`.
pub struct LibvirtPower {
    domain: String,
    uri: String,
    runner: Box<dyn CommandRunner>,
    connected: bool,
}

impl fmt::Debug for LibvirtPower {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LibvirtPower")
            .field("domain", &self.domain)
            .field("uri", &self.uri)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

impl LibvirtPower {
    /// Create a backend for `domain` using the default `virsh` runner and URI.
    ///
    /// Configuration is validated by [`LibvirtPower::connect`], which also
    /// proves that the domain exists.
    pub fn new(domain: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            uri: DEFAULT_LIBVIRT_URI.to_string(),
            runner: Box::new(TokioCommandRunner),
            connected: false,
        }
    }

    /// Set the libvirt URI. It is validated by [`LibvirtPower::connect`].
    pub fn with_uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = uri.into();
        self
    }

    /// Replace the subprocess runner (used by tests and supervisors).
    pub fn with_runner(mut self, runner: Box<dyn CommandRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// Configured libvirt domain name.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Configured libvirt URI.
    pub fn uri(&self) -> &str {
        &self.uri
    }

    /// True after a successful [`LibvirtPower::connect`].
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Validate the configuration and prove `virsh domstate <domain>` works.
    ///
    /// This is the only call that checks the domain exists; later operations
    /// return [`PowerError::NotConnected`] until it succeeds.
    pub async fn connect(&mut self) -> Result<(), PowerError> {
        self.validate()?;
        let state = self.run_domstate().await?;
        self.connected = true;
        info!(domain = %self.domain, state = %state, "libvirt power: connected");
        Ok(())
    }

    /// Mark the backend disconnected. A later [`LibvirtPower::connect`] runs
    /// the `virsh domstate` probe again.
    pub fn disconnect(&mut self) {
        self.connected = false;
    }

    fn validate(&self) -> Result<(), PowerError> {
        let domain = &self.domain;
        if domain.trim().is_empty() {
            return Err(PowerError::InvalidConfig {
                field: "domain",
                reason: "domain name is empty".to_string(),
            });
        }
        if domain.trim() != domain {
            return Err(PowerError::InvalidConfig {
                field: "domain",
                reason: "domain name has leading or trailing whitespace".to_string(),
            });
        }
        if domain.starts_with('-') {
            return Err(PowerError::InvalidConfig {
                field: "domain",
                reason: "domain name must not start with '-' (would be parsed as a virsh option)"
                    .to_string(),
            });
        }
        if domain.chars().any(char::is_control) {
            return Err(PowerError::InvalidConfig {
                field: "domain",
                reason: "domain name contains a control character".to_string(),
            });
        }

        let uri = &self.uri;
        if uri.trim().is_empty() {
            return Err(PowerError::InvalidConfig {
                field: "uri",
                reason: "libvirt URI is empty".to_string(),
            });
        }
        if uri.chars().any(char::is_whitespace) || uri.chars().any(char::is_control) {
            return Err(PowerError::InvalidConfig {
                field: "uri",
                reason: "libvirt URI contains whitespace or a control character".to_string(),
            });
        }
        let Some((scheme, rest)) = uri.split_once("://") else {
            return Err(PowerError::InvalidConfig {
                field: "uri",
                reason: "libvirt URI must include a scheme, for example qemu:///system".to_string(),
            });
        };
        if scheme.is_empty() || rest.is_empty() {
            return Err(PowerError::InvalidConfig {
                field: "uri",
                reason:
                    "libvirt URI must include a scheme and a target, for example qemu:///system"
                        .to_string(),
            });
        }

        Ok(())
    }

    fn ensure_connected(&self) -> Result<(), PowerError> {
        if self.connected {
            Ok(())
        } else {
            Err(PowerError::NotConnected)
        }
    }

    fn argv(&self, parts: &[&str]) -> Vec<String> {
        let mut argv = Vec::with_capacity(parts.len() + 2);
        argv.push("virsh".to_string());
        argv.extend(parts.iter().map(|part| (*part).to_string()));
        argv.push(self.domain.clone());
        argv
    }

    async fn run_domstate(&self) -> Result<PowerState, PowerError> {
        let argv = self.argv(&["domstate"]);
        let output = self.runner.run(&argv).await?;
        Ok(PowerState::from_libvirt(&output.stdout))
    }

    async fn run_action(&self, action: &str) -> Result<(), PowerError> {
        let argv = self.argv(&[action]);
        let rendered = render_command(&argv);
        self.runner.run(&argv).await?;
        debug!(domain = %self.domain, action, command = %rendered, "libvirt power: action succeeded");
        Ok(())
    }
}

#[async_trait]
impl PowerBackend for LibvirtPower {
    async fn query_status(&mut self) -> Result<PowerState, PowerError> {
        self.ensure_connected()?;
        self.run_domstate().await
    }

    /// Resume a paused VM, or start a stopped one.
    ///
    /// Python's `virsh` fallback tries `resume` and then `start`; the same order
    /// is used here. A missing/failed-to-spawn `virsh` is returned immediately
    /// because the fallback would fail identically.
    async fn cont(&mut self) -> Result<(), PowerError> {
        self.ensure_connected()?;
        match self.run_action("resume").await {
            Ok(()) => Ok(()),
            Err(error @ PowerError::MissingCommand { .. })
            | Err(error @ PowerError::Spawn { .. }) => Err(error),
            Err(error) => {
                debug!(
                    domain = %self.domain,
                    %error,
                    "libvirt power: virsh resume failed, falling back to virsh start"
                );
                self.run_action("start").await
            }
        }
    }

    async fn system_powerdown(&mut self) -> Result<(), PowerError> {
        self.ensure_connected()?;
        self.run_action("shutdown").await
    }

    async fn system_reset(&mut self) -> Result<(), PowerError> {
        self.ensure_connected()?;
        self.run_action("reset").await
    }

    /// Force the VM off (`virsh destroy`).
    async fn stop(&mut self) -> Result<(), PowerError> {
        self.ensure_connected()?;
        self.run_action("destroy").await
    }
}
