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
//! | pause | `virsh suspend <domain>` |
//! | resume | `virsh resume <domain>` |
//! | shutdown | `virsh shutdown <domain>` |
//! | reset | `virsh reset <domain>` |
//! | stop | `virsh destroy <domain>` |
//!
//! When a URI is configured (node `--libvirt-uri` / `OZMA_LIBVIRT_URI`), every
//! call becomes `virsh --connect <uri> <operation> <domain>`. When no URI is
//! configured the `--connect` argument is omitted, preserving `virsh`'s own
//! default connection resolution (historically `qemu:///session` for a normal
//! user session). The backend never silently substitutes a URI.
//!
//! `off`/`shutdown` is an ACPI request, not a host power cut. After sending it,
//! [`LibvirtPower::system_powerdown_outcome`] polls `domstate` for a bounded
//! interval and reports `pending: true` with the observed running state when the
//! guest has not shut down yet.

use std::fmt;
use std::time::Duration;

use async_trait::async_trait;
use tokio::time::{sleep, Instant};
use tracing::{debug, info};

use crate::error::PowerError;
use crate::runner::{render_command, CommandRunner, TokioCommandRunner};
use crate::state::{PowerOutcome, PowerState};
use crate::PowerBackend;

/// Connection URI `virsh` used in the live-validation environment when no
/// `--libvirt-uri` was supplied.
///
/// The backend does **not** inject this value: leaving the URI unset preserves
/// `virsh`'s own default resolution (which also honours `LIBVIRT_DEFAULT_URI`).
pub const DEFAULT_LIBVIRT_URI: &str = "qemu:///session";

/// Default bound for polling `domstate` after an ACPI shutdown request.
pub const DEFAULT_ACPI_SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(3);

/// Default interval between `domstate` polls during an ACPI shutdown request.
pub const DEFAULT_ACPI_SHUTDOWN_POLL_INTERVAL: Duration = Duration::from_millis(150);

/// Power control for one libvirt domain through `virsh`.
pub struct LibvirtPower {
    domain: String,
    uri: Option<String>,
    runner: Box<dyn CommandRunner>,
    shutdown_timeout: Duration,
    shutdown_poll_interval: Duration,
    connected: bool,
}

impl fmt::Debug for LibvirtPower {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LibvirtPower")
            .field("domain", &self.domain)
            .field("uri", &self.uri)
            .field("shutdown_timeout", &self.shutdown_timeout)
            .field("shutdown_poll_interval", &self.shutdown_poll_interval)
            .field("connected", &self.connected)
            .finish_non_exhaustive()
    }
}

impl LibvirtPower {
    /// Create a backend for `domain` using the default `virsh` runner.
    ///
    /// No `--connect` argument is added until [`LibvirtPower::with_uri`] is
    /// called. Configuration is validated by [`LibvirtPower::connect`], which
    /// also proves that the domain exists.
    pub fn new(domain: impl Into<String>) -> Self {
        Self {
            domain: domain.into(),
            uri: None,
            runner: Box::new(TokioCommandRunner),
            shutdown_timeout: DEFAULT_ACPI_SHUTDOWN_TIMEOUT,
            shutdown_poll_interval: DEFAULT_ACPI_SHUTDOWN_POLL_INTERVAL,
            connected: false,
        }
    }

    /// Set the libvirt URI. It is passed as `virsh --connect <uri>` on every
    /// call and is validated by [`LibvirtPower::connect`].
    pub fn with_uri(mut self, uri: impl Into<String>) -> Self {
        self.uri = Some(uri.into());
        self
    }

    /// Replace the subprocess runner (used by tests and supervisors).
    pub fn with_runner(mut self, runner: Box<dyn CommandRunner>) -> Self {
        self.runner = runner;
        self
    }

    /// Bound for polling `domstate` after an ACPI shutdown request.
    pub fn with_shutdown_timeout(mut self, timeout: Duration) -> Self {
        self.shutdown_timeout = timeout;
        self
    }

    /// Interval between `domstate` polls after an ACPI shutdown request.
    pub fn with_shutdown_poll_interval(mut self, interval: Duration) -> Self {
        self.shutdown_poll_interval = interval;
        self
    }

    /// Configured libvirt domain name.
    pub fn domain(&self) -> &str {
        &self.domain
    }

    /// Configured libvirt URI, or `None` when `virsh` default resolution is used.
    pub fn uri(&self) -> Option<&str> {
        self.uri.as_deref()
    }

    /// True after a successful [`LibvirtPower::connect`].
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// Validate the configuration and prove `virsh domstate <domain>` works.
    ///
    /// This is the only call that checks the domain exists; later operations
    /// return [`PowerError::NotConnected`] until it succeeds. A missing domain
    /// is returned as [`PowerError::DomainNotFound`], distinct from a missing
    /// `virsh` binary and a failed libvirt connection.
    pub async fn connect(&mut self) -> Result<(), PowerError> {
        self.validate()?;
        let state = self.run_domstate().await?;
        self.connected = true;
        info!(
            domain = %self.domain,
            uri = self.uri.as_deref().unwrap_or("<virsh-default>"),
            state = %state,
            "libvirt power: connected"
        );
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

        if let Some(uri) = &self.uri {
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
                    reason: "libvirt URI must include a scheme, for example qemu:///system"
                        .to_string(),
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
        let mut argv = Vec::with_capacity(parts.len() + 4);
        argv.push("virsh".to_string());
        if let Some(uri) = &self.uri {
            argv.push("--connect".to_string());
            argv.push(uri.clone());
        }
        argv.extend(parts.iter().map(|part| (*part).to_string()));
        argv.push(self.domain.clone());
        argv
    }

    async fn run_domstate(&self) -> Result<PowerState, PowerError> {
        let argv = self.argv(&["domstate"]);
        match self.runner.run(&argv).await {
            Ok(output) => Ok(PowerState::from_libvirt(&output.stdout)),
            Err(error) => Err(self.classify(error)),
        }
    }

    async fn run_action(&self, action: &str) -> Result<(), PowerError> {
        let argv = self.argv(&[action]);
        let rendered = render_command(&argv);
        match self.runner.run(&argv).await {
            Ok(_) => {
                debug!(domain = %self.domain, action, command = %rendered, "libvirt power: action succeeded");
                Ok(())
            }
            Err(error) => Err(self.classify(error)),
        }
    }

    /// Turn a `virsh` "failed to get domain" failure into a typed missing-domain
    /// error while leaving missing-binary and connection failures distinct.
    fn classify(&self, error: PowerError) -> PowerError {
        if let PowerError::CommandFailed { stderr, .. } = &error {
            if is_domain_not_found(stderr) {
                return PowerError::DomainNotFound {
                    domain: self.domain.clone(),
                    stderr: stderr.clone(),
                };
            }
        }
        error
    }

    /// Send ACPI shutdown, then poll `domstate` for a bounded interval.
    async fn shutdown_outcome(&mut self) -> Result<PowerOutcome, PowerError> {
        self.ensure_connected()?;

        // An action on an already-off domain is a no-op, not a reason to send a
        // shutdown request that `virsh` would reject. Report the real state.
        let before = self.run_domstate().await?;
        if before.is_shutoff() {
            debug!(
                domain = %self.domain,
                state = %before,
                "libvirt power: shutdown requested on an already-off domain"
            );
            return Ok(PowerOutcome::completed(before));
        }

        self.run_action("shutdown").await?;

        let deadline = Instant::now() + self.shutdown_timeout;
        loop {
            let state = self.run_domstate().await?;
            if state.is_shutoff() {
                debug!(
                    domain = %self.domain,
                    state = %state,
                    "libvirt power: ACPI shutdown completed"
                );
                return Ok(PowerOutcome::completed(state));
            }
            if Instant::now() >= deadline {
                debug!(
                    domain = %self.domain,
                    state = %state,
                    "libvirt power: ACPI shutdown still pending"
                );
                return Ok(PowerOutcome::pending(state, "acpi-requested"));
            }
            sleep(self.shutdown_poll_interval).await;
        }
    }
}

/// Detect the `virsh` messages used for a missing domain across libvirt
/// versions. Connection failures produce different wording (`Failed to connect
/// socket`, `authentication unavailable`, ...) and must not match.
fn is_domain_not_found(stderr: &str) -> bool {
    let lower = stderr.to_ascii_lowercase();
    lower.contains("failed to get domain")
        || lower.contains("domain not found")
        || lower.contains("no domain with matching name")
        || lower.contains("no domain with matching id")
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
    /// because the fallback would fail identically. A missing domain is also not
    /// retried with `start`.
    async fn cont(&mut self) -> Result<(), PowerError> {
        self.ensure_connected()?;
        match self.run_action("resume").await {
            Ok(()) => Ok(()),
            Err(error @ PowerError::MissingCommand { .. })
            | Err(error @ PowerError::Spawn { .. })
            | Err(error @ PowerError::DomainNotFound { .. }) => Err(error),
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

    /// Request ACPI shutdown and discard the readback detail.
    ///
    /// Callers that need to report a pending request should use
    /// [`PowerBackend::system_powerdown_outcome`].
    async fn system_powerdown(&mut self) -> Result<(), PowerError> {
        self.shutdown_outcome().await.map(|_| ())
    }

    async fn system_powerdown_outcome(&mut self) -> Result<PowerOutcome, PowerError> {
        self.shutdown_outcome().await
    }

    /// Pause the domain (`virsh suspend`).
    async fn pause(&mut self) -> Result<(), PowerError> {
        self.ensure_connected()?;
        self.run_action("suspend").await
    }

    /// Resume a paused domain (`virsh resume`).
    ///
    /// Unlike [`PowerBackend::cont`], this does not fall back to `virsh start`;
    /// a stopped domain should be started explicitly.
    async fn resume(&mut self) -> Result<(), PowerError> {
        self.ensure_connected()?;
        self.run_action("resume").await
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
