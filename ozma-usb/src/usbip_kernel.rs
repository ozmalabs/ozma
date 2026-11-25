// SPDX-License-Identifier: AGPL-3.0-only
//! Linux kernel-backed USB/IP host adapter.
//!
//! [`LinuxUsbIpHostBackend`] implements [`UsbIpKernelBackend`] over the
//! userspace `usbip` CLI plus a configurable sysfs root. It is the real
//! counterpart to [`crate::usbip_daemon::NoUsbIpKernel`]:
//!
//! * `export` runs `usbip bind -b <busid>` only after `usbip_host` and the
//!   `usbip-host` driver directory exist, then verifies the sysfs binding and
//!   rolls back with `usbip unbind` when the kernel did not bind the device.
//! * `unexport` runs `usbip unbind -b <busid>`.
//! * `attach` runs `usbip attach -r <host> -b <busid>` only after the real
//!   `vhci_hcd` platform device with an `attach` attribute exists. The local
//!   port comes from the command output or `usbip port`; if neither reports one
//!   the call fails with `usbip-kernel-required` and never invents a port.
//! * `detach` runs `usbip detach -p <port>`.
//!
//! Every command is an argv vector through the existing [`CommandRunner`]
//! (never a shell). Host, bus id, and port are validated before they reach
//! argv. Peer-facing entry points ([`LinuxUsbIpHostBackend::export_for_peer`],
//! [`LinuxUsbIpHostBackend::attach_authorized`]) enforce the existing host,
//! token, and key-fingerprint allow-lists in constant time before any command or
//! sysfs probe. Tokens and fingerprints are never stored in the backend, never
//! placed in argv, and never rendered in errors.
//!
//! The sysfs root is injectable so tests never need `/sys` or root privileges.
//! Missing modules, missing platform attributes, missing `usbip`, and an
//! unreported vhci port are typed `usbip-kernel-required` failures; a non-zero
//! command exit stays `command-failed`. No backend state, port, or attachment is
//! fabricated.

use std::path::{Path, PathBuf};

use crate::error::UsbError;
use crate::subprocess::{CommandOutput, CommandRunner, CommandSpec, SystemCommandRunner};
use crate::usbip::{
    parse_usbip_list, parse_usbip_ports, validate_bus_id, validate_host, validate_vhci_port,
    UsbIpAttachment, UsbIpAuthorization, UsbIpDevice, UsbIpPeerGrant,
};
use crate::usbip_daemon::UsbIpKernelBackend;

/// Default sysfs root inspected by the kernel adapter.
pub const DEFAULT_USBIP_SYSFS_ROOT: &str = "/sys";

/// Default `usbip` executable name.
pub const DEFAULT_USBIP_PROGRAM: &str = "usbip";

/// Sysfs layout used by the USB/IP kernel modules.
///
/// Production code uses [`Self::system`] (`/sys`). Tests build a temporary tree
/// with `module/vhci_hcd`, `module/usbip_host`,
/// `devices/platform/vhci_hcd.0/attach`, and
/// `bus/usb/drivers/usbip-host/<busid>` entries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UsbIpKernelSysfs {
    root: PathBuf,
}

impl Default for UsbIpKernelSysfs {
    fn default() -> Self {
        Self::system()
    }
}

impl UsbIpKernelSysfs {
    /// Inspect the real `/sys` tree.
    pub fn system() -> Self {
        Self::with_root(DEFAULT_USBIP_SYSFS_ROOT)
    }

    /// Inspect a caller-supplied sysfs root.
    pub fn with_root(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// Configured root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// `<root>/module/<name>`.
    pub fn module_path(&self, name: &str) -> PathBuf {
        self.root.join("module").join(name)
    }

    /// `<root>/devices/platform`.
    pub fn platform_root(&self) -> PathBuf {
        self.root.join("devices").join("platform")
    }

    /// `<root>/bus/usb/drivers/usbip-host`.
    pub fn host_driver_root(&self) -> PathBuf {
        self.root
            .join("bus")
            .join("usb")
            .join("drivers")
            .join("usbip-host")
    }

    /// Whether the `vhci_hcd` module directory is visible.
    pub fn vhci_module_present(&self) -> bool {
        self.module_path("vhci_hcd").is_dir()
    }

    /// Whether the `usbip_host` module directory is visible.
    pub fn host_module_present(&self) -> bool {
        self.module_path("usbip_host").is_dir()
    }

    /// First `vhci_hcd*` platform directory that exposes an `attach` attribute.
    pub fn vhci_attach_path(&self) -> Option<PathBuf> {
        let platform = self.platform_root();
        let mut names: Vec<String> = std::fs::read_dir(&platform)
            .ok()?
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                name.starts_with("vhci_hcd").then_some(name)
            })
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|name| platform.join(name))
            .find_map(|directory| {
                let attach = directory.join("attach");
                attach.is_file().then_some(attach)
            })
    }

    /// Whether `vhci_hcd` and its attach attribute are present.
    pub fn vhci_present(&self) -> bool {
        self.vhci_module_present() && self.vhci_attach_path().is_some()
    }

    /// Currently bound (exported) bus ids under the `usbip-host` driver.
    ///
    /// A missing driver directory yields an empty list rather than an error; the
    /// caller decides whether that is fatal for the operation at hand.
    pub fn bound_bus_ids(&self) -> Vec<String> {
        let root = self.host_driver_root();
        let mut ids: Vec<String> = std::fs::read_dir(root)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let name = entry.file_name().to_string_lossy().into_owned();
                validate_bus_id(&name).ok().map(|()| name)
            })
            .collect();
        ids.sort();
        ids
    }

    /// Whether a bus id appears bound under the `usbip-host` driver.
    pub fn is_bound(&self, bus_id: &str) -> bool {
        self.host_driver_root().join(bus_id).exists()
    }

    /// Whether the driver directory exists (so binding can be verified).
    pub fn has_host_driver_dir(&self) -> bool {
        self.host_driver_root().is_dir()
    }

    /// Require the `usbip_host` module and driver directory for an export.
    pub fn require_host(&self, operation: &'static str) -> Result<(), UsbError> {
        if !self.host_module_present() {
            return Err(UsbError::UsbIpKernelRequired {
                operation,
                reason: format!(
                    "the kernel usbip-host module is absent at {}; load usbip-host before exporting",
                    self.module_path("usbip_host").display()
                ),
            });
        }
        if !self.has_host_driver_dir() {
            return Err(UsbError::UsbIpKernelRequired {
                operation,
                reason: format!(
                    "the usbip-host driver directory is absent at {}; no export can be verified",
                    self.host_driver_root().display()
                ),
            });
        }
        Ok(())
    }

    /// Require the `vhci_hcd` module and attach attribute for an import.
    pub fn require_vhci(&self, operation: &'static str) -> Result<(), UsbError> {
        if !self.vhci_module_present() {
            return Err(UsbError::UsbIpKernelRequired {
                operation,
                reason: format!(
                    "the kernel vhci_hcd module is absent at {}; load vhci-hcd before importing",
                    self.module_path("vhci_hcd").display()
                ),
            });
        }
        if self.vhci_attach_path().is_none() {
            return Err(UsbError::UsbIpKernelRequired {
                operation,
                reason: format!(
                    "no vhci_hcd platform device with an attach attribute exists under {}",
                    self.platform_root().display()
                ),
            });
        }
        Ok(())
    }
}

/// A kernel USB/IP backend driven by `usbip` and a sysfs tree.
#[derive(Debug, Clone)]
pub struct LinuxUsbIpHostBackend<R = SystemCommandRunner> {
    runner: R,
    program: String,
    sysfs: UsbIpKernelSysfs,
    authorization: Option<UsbIpAuthorization>,
}

impl LinuxUsbIpHostBackend<SystemCommandRunner> {
    /// Use `usbip` from `PATH` and the real `/sys`.
    pub fn system() -> Self {
        Self::new(
            SystemCommandRunner,
            DEFAULT_USBIP_PROGRAM,
            UsbIpKernelSysfs::system(),
        )
    }
}

impl<R: CommandRunner> LinuxUsbIpHostBackend<R> {
    /// Build a backend from an injected runner, program, and sysfs root.
    pub fn new(runner: R, program: impl Into<String>, sysfs: UsbIpKernelSysfs) -> Self {
        Self {
            runner,
            program: program.into(),
            sysfs,
            authorization: None,
        }
    }

    /// Attach the peer host/token/key allow-list enforced by the peer-facing
    /// entry points.
    pub fn with_authorization(mut self, authorization: UsbIpAuthorization) -> Self {
        self.authorization = Some(authorization);
        self
    }

    /// Injected runner.
    pub fn runner(&self) -> &R {
        &self.runner
    }

    /// Configured `usbip` program.
    pub fn program(&self) -> &str {
        &self.program
    }

    /// Configured sysfs layout.
    pub fn sysfs(&self) -> &UsbIpKernelSysfs {
        &self.sysfs
    }

    /// Configured authorization, if any.
    pub fn authorization(&self) -> Option<&UsbIpAuthorization> {
        self.authorization.as_ref()
    }

    /// Enforce the host allow-list before a client-side attach.
    pub fn authorize_host(&self, host: &str, token: Option<&str>) -> Result<(), UsbError> {
        validate_host(host)?;
        match &self.authorization {
            Some(authorization) => authorization.authorize(host, token),
            None => Ok(()),
        }
    }

    /// Enforce host, token, and key-fingerprint pairing before a peer-facing
    /// export or import. The denial never includes the presented secrets.
    pub fn authorize_peer(
        &self,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<UsbIpPeerGrant, UsbError> {
        validate_host(host)?;
        match &self.authorization {
            Some(authorization) => authorization.authorize_peer(host, token, key_fingerprint),
            None => Ok(UsbIpPeerGrant {
                host: host.to_owned(),
                token_presented: token.is_some(),
                key: None,
            }),
        }
    }

    /// Export a local device after authorizing the requesting peer.
    pub fn export_for_peer(
        &self,
        bus_id: &str,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<(), UsbError> {
        let _grant = self.authorize_peer(host, token, key_fingerprint)?;
        self.export_verified(bus_id)
    }

    /// Import a remote device after authorizing the peer.
    pub fn attach_authorized(
        &self,
        host: &str,
        bus_id: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<u16, UsbError> {
        let _grant = self.authorize_peer(host, token, key_fingerprint)?;
        self.attach_unchecked(host, bus_id)
    }

    /// List `usbip port` attachments after checking the vhci sysfs path.
    pub fn ports(&self) -> Result<Vec<UsbIpAttachment>, UsbError> {
        self.sysfs.require_vhci("list ports")?;
        let output = self.run_kernel(&["port"], "list ports")?;
        Ok(parse_usbip_ports(&output.stdout))
    }

    /// List local exportable devices via `usbip list -l`.
    pub fn list_local(&self) -> Result<Vec<UsbIpDevice>, UsbError> {
        let output = self.run_kernel(&["list", "-l"], "list local")?;
        Ok(parse_usbip_list(&output.stdout))
    }

    fn run_kernel(
        &self,
        args: &[&str],
        operation: &'static str,
    ) -> Result<CommandOutput, UsbError> {
        let command = CommandSpec::new(
            self.program.clone(),
            args.iter().map(|argument| (*argument).to_owned()),
        );
        self.runner.run(&command).map_err(|error| match error {
            UsbError::MissingCommand { command, .. } => UsbError::UsbIpKernelRequired {
                operation,
                reason: format!(
                    "the {command:?} executable is unavailable; install the usbip tools and \
                     load vhci-hcd/usbip-host"
                ),
            },
            other => other,
        })
    }

    fn export_verified(&self, bus_id: &str) -> Result<(), UsbError> {
        validate_bus_id(bus_id)?;
        self.sysfs.require_host("export")?;
        self.run_kernel(&["bind", "-b", bus_id], "export")?;

        // `usbip bind` is asynchronous at the sysfs level only in the sense that
        // the driver link appears after the netlink reply. When the driver
        // directory is visible, require the kernel to have created the binding;
        // otherwise roll the bind back rather than reporting a false export.
        if self.sysfs.has_host_driver_dir() && !self.sysfs.is_bound(bus_id) {
            let _ = self.run_kernel(&["unbind", "-b", bus_id], "export rollback");
            return Err(UsbError::UsbIpKernelRequired {
                operation: "export",
                reason: format!(
                    "usbip bind returned success but {} has no usbip-host binding; the bind was \
                     rolled back and no export is reported",
                    self.sysfs.host_driver_root().display()
                ),
            });
        }
        Ok(())
    }

    fn attach_unchecked(&self, host: &str, bus_id: &str) -> Result<u16, UsbError> {
        validate_host(host)?;
        validate_bus_id(bus_id)?;
        self.sysfs.require_vhci("attach")?;
        let output = self.run_kernel(&["attach", "-r", host, "-b", bus_id], "attach")?;

        if let Some(port) = parse_attach_port(&output.stdout) {
            return Ok(port);
        }

        // Kernel builds differ: prefer the port list when the attach command did
        // not print its assigned port.
        let ports = self.ports()?;
        ports
            .into_iter()
            .find(|attachment| attachment.host == host && attachment.bus_id == bus_id)
            .and_then(|attachment| attachment.port)
            .ok_or_else(|| UsbError::UsbIpKernelRequired {
                operation: "attach",
                reason:
                    "usbip attach succeeded but neither its output nor `usbip port` reported a \
                         local vhci port; no port was fabricated"
                        .to_owned(),
            })
    }
}

impl<R: CommandRunner> UsbIpKernelBackend for LinuxUsbIpHostBackend<R> {
    fn available(&self) -> bool {
        self.sysfs.vhci_present() && self.sysfs.host_module_present()
    }

    fn export(&self, bus_id: &str) -> Result<(), UsbError> {
        self.export_verified(bus_id)
    }

    fn unexport(&self, bus_id: &str) -> Result<(), UsbError> {
        validate_bus_id(bus_id)?;
        self.sysfs.require_host("unexport")?;
        self.run_kernel(&["unbind", "-b", bus_id], "unexport")
            .map(|_| ())
    }

    fn attach(&self, host: &str, bus_id: &str) -> Result<u16, UsbError> {
        self.attach_unchecked(host, bus_id)
    }

    fn detach(&self, port: u16) -> Result<(), UsbError> {
        validate_vhci_port(port)?;
        self.sysfs.require_vhci("detach")?;
        let port_text = port.to_string();
        self.run_kernel(&["detach", "-p", &port_text], "detach")
            .map(|_| ())
    }
}

/// Parse the local vhci port printed by `usbip attach`.
///
/// Common output is `usbip: info: using port 00`. A full `usbip port` listing is
/// also accepted, so callers can pass either command's stdout.
pub fn parse_attach_port(output: &str) -> Option<u16> {
    for line in output.lines() {
        let lower = line.to_ascii_lowercase();
        if let Some(index) = lower.find("using port") {
            let rest = &line[index + "using port".len()..];
            if let Some(token) = rest.split_whitespace().next() {
                let token = token.trim_end_matches(':');
                if let Ok(port) = token.parse::<u16>() {
                    return Some(port);
                }
            }
        }
    }
    parse_usbip_ports(output)
        .into_iter()
        .find_map(|attachment| attachment.port)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_the_kernel_attach_port_line() {
        assert_eq!(parse_attach_port("usbip: info: using port 00\n"), Some(0));
        assert_eq!(
            parse_attach_port("usbip: info: using port 07 (usbip://host/1-1)\n"),
            Some(7)
        );
        assert_eq!(parse_attach_port("no port here\n"), None);
    }
}
