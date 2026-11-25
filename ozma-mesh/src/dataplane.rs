//! Typed, injectable Linux mesh dataplane programming.
//!
//! The mesh dataplane is deliberately separate from WireGuard tunnel state:
//! a peer is `established` only after a real boringtun handshake, while the
//! dataplane reports whether the local TUN device, routes, forwarding, and
//! optional NAT rules were actually programmed.
//!
//! Every kernel interaction goes through one of two seams:
//!
//! * [`CommandRunner`] — exact argv, no shell, for `ip` and `nft`.
//! * [`Sysfs`] / [`FileOpener`] — root detection, tool discovery, `/dev/net/tun`
//!   presence, and `/proc/sys/net/ipv4/ip_forward` reads/writes.
//!
//! Default tests inject fakes; nothing in this module is invoked by the library
//! unless a caller explicitly constructs a [`MeshDataplane`] and calls
//! [`MeshDataplane::start`].

use std::fmt;
use std::io;
use std::net::Ipv4Addr;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

use crate::error::MeshError;

/// Default TUN interface name used by the node.
pub const DEFAULT_INTERFACE: &str = "ozma0";
/// Default mesh TUN MTU (leaves room for WireGuard overhead).
pub const DEFAULT_MTU: u16 = 1420;
/// Default mesh prefix length.
pub const DEFAULT_PREFIX_LEN: u8 = 24;
/// Default nftables table name for mesh NAT.
pub const DEFAULT_NAT_TABLE: &str = "ozma_mesh";
/// Character device used by the Linux TUN driver.
pub const TUN_DEVICE: &str = "/dev/net/tun";
/// IPv4 forwarding sysctl.
pub const IPV4_FORWARD: &str = "/proc/sys/net/ipv4/ip_forward";
/// Smallest MTU accepted for an IPv4 mesh interface.
pub const MIN_MTU: u16 = 68;
/// Largest accepted MTU.
pub const MAX_MTU: u16 = 65_535;

const MAX_REASON_BYTES: usize = 512;

/// One exact subprocess invocation. `args` are passed directly; no shell is
/// involved anywhere in this module.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandSpec {
    pub program: String,
    pub args: Vec<String>,
}

impl CommandSpec {
    /// Build a command from a program and exact arguments.
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

    /// Program followed by arguments, for diagnostics only.
    pub fn argv(&self) -> Vec<&str> {
        let mut argv = Vec::with_capacity(self.args.len() + 1);
        argv.push(self.program.as_str());
        argv.extend(self.args.iter().map(String::as_str));
        argv
    }
}

impl fmt::Display for CommandSpec {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.program)?;
        for argument in &self.args {
            write!(formatter, " {argument}")?;
        }
        Ok(())
    }
}

/// Result of one subprocess invocation.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommandOutput {
    /// Process exit status (`-1` when the platform has no numeric status).
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
}

impl CommandOutput {
    pub fn success(&self) -> bool {
        self.status == 0
    }

    #[cfg(test)]
    fn success_output() -> Self {
        Self {
            status: 0,
            stdout: String::new(),
            stderr: String::new(),
        }
    }
}

/// Injectable subprocess seam.
pub trait CommandRunner: Send + Sync {
    fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput>;
}

/// Read-only/write access to the host filesystem and proc/sys entries.
pub trait FileOpener: Send + Sync {
    fn exists(&self, path: &Path) -> bool;
    fn read_to_string(&self, path: &Path) -> io::Result<String>;
    fn write(&self, path: &Path, contents: &str) -> io::Result<()>;
}

/// Host environment seam: root detection and PATH tool discovery on top of
/// [`FileOpener`].
pub trait Sysfs: FileOpener {
    fn is_root(&self) -> bool;
    fn command_available(&self, program: &str) -> bool;
}

/// Real subprocess runner. Uses `std::process::Command` directly, never a shell.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemCommandRunner;

impl SystemCommandRunner {
    pub fn new() -> Self {
        Self
    }
}

impl CommandRunner for SystemCommandRunner {
    fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput> {
        let output = std::process::Command::new(&command.program)
            .args(&command.args)
            .output()?;
        Ok(CommandOutput {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }
}

/// Real host filesystem/proc/sys seam.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemSysfs;

impl SystemSysfs {
    pub fn new() -> Self {
        Self
    }
}

impl FileOpener for SystemSysfs {
    fn exists(&self, path: &Path) -> bool {
        path.exists()
    }

    fn read_to_string(&self, path: &Path) -> io::Result<String> {
        std::fs::read_to_string(path)
    }

    fn write(&self, path: &Path, contents: &str) -> io::Result<()> {
        std::fs::write(path, contents)
    }
}

impl Sysfs for SystemSysfs {
    fn is_root(&self) -> bool {
        // SAFETY: geteuid has no preconditions and does not mutate memory.
        unsafe { libc::geteuid() == 0 }
    }

    fn command_available(&self, program: &str) -> bool {
        std::env::var_os("PATH")
            .map(|paths| {
                std::env::split_paths(&paths).any(|directory| directory.join(program).is_file())
            })
            .unwrap_or(false)
    }
}

/// Optional masquerade configuration. NAT is never programmed unless this is
/// explicitly supplied.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct NatConfig {
    /// nftables table name.
    pub table: String,
    /// Optional egress interface. `None` masquerades traffic leaving any
    /// interface other than the mesh TUN.
    pub out_interface: Option<String>,
}

impl NatConfig {
    pub fn new() -> Self {
        Self {
            table: DEFAULT_NAT_TABLE.to_owned(),
            out_interface: None,
        }
    }

    pub fn with_table(mut self, table: impl Into<String>) -> Self {
        self.table = table.into();
        self
    }

    pub fn with_out_interface(mut self, interface: impl Into<String>) -> Self {
        self.out_interface = Some(interface.into());
        self
    }
}

impl Default for NatConfig {
    fn default() -> Self {
        Self::new()
    }
}

/// Everything needed to program one mesh TUN interface.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataplaneConfig {
    /// TUN link name (Linux limits this to 15 characters).
    pub interface: String,
    /// Local mesh IPv4 address.
    pub address: Ipv4Addr,
    /// Mesh prefix length (`1..=32`).
    pub prefix_len: u8,
    /// Link MTU.
    pub mtu: u16,
    /// Peer mesh IPs that receive an explicit `/32` route through the TUN.
    pub peer_routes: Vec<Ipv4Addr>,
    /// Enable IPv4 forwarding while the dataplane is running.
    pub forwarding: bool,
    /// Optional NAT/masquerade block (opt-in).
    pub nat: Option<NatConfig>,
}

impl DataplaneConfig {
    pub fn new(interface: impl Into<String>, address: Ipv4Addr, prefix_len: u8, mtu: u16) -> Self {
        Self {
            interface: interface.into(),
            address,
            prefix_len,
            mtu,
            peer_routes: Vec::new(),
            forwarding: true,
            nat: None,
        }
    }

    pub fn with_peer_routes(mut self, routes: Vec<Ipv4Addr>) -> Self {
        self.peer_routes = routes;
        self
    }

    pub fn with_forwarding(mut self, forwarding: bool) -> Self {
        self.forwarding = forwarding;
        self
    }

    pub fn with_nat(mut self, nat: NatConfig) -> Self {
        self.nat = Some(nat);
        self
    }

    /// Local address in CIDR form (`10.200.9.1/24`).
    pub fn address_cidr(&self) -> String {
        format!("{}/{}", self.address, self.prefix_len)
    }

    /// Network address in CIDR form, used as the NAT source selector.
    pub fn network_cidr(&self) -> String {
        let mask = prefix_mask(self.prefix_len);
        let network = Ipv4Addr::from(u32::from(self.address) & mask);
        format!("{network}/{}", self.prefix_len)
    }

    /// Peer `/32` routes in their `ip route` form.
    pub fn route_cidrs(&self) -> Vec<String> {
        self.peer_routes
            .iter()
            .map(|route| format!("{route}/32"))
            .collect()
    }

    /// Reject anything that would produce an ambiguous or dangerous argv.
    pub fn validate(&self) -> Result<(), DataplaneError> {
        if !valid_interface_name(&self.interface) {
            return Err(DataplaneError::invalid(format!(
                "mesh TUN interface name {:?} is invalid (1-15 chars, letters/digits/._- only)",
                self.interface
            )));
        }
        if !valid_mesh_address(self.address) {
            return Err(DataplaneError::invalid(format!(
                "mesh dataplane address {} is not a usable unicast IPv4 address",
                self.address
            )));
        }
        if !(1..=32).contains(&self.prefix_len) {
            return Err(DataplaneError::invalid(format!(
                "mesh prefix length {} is invalid; expected 1..=32",
                self.prefix_len
            )));
        }
        if !(MIN_MTU..=MAX_MTU).contains(&self.mtu) {
            return Err(DataplaneError::invalid(format!(
                "mesh MTU {} is invalid; expected {MIN_MTU}..={MAX_MTU}",
                self.mtu
            )));
        }

        let mut seen = std::collections::BTreeSet::new();
        for route in &self.peer_routes {
            if !valid_mesh_address(*route) {
                return Err(DataplaneError::invalid(format!(
                    "peer route {route} is not a usable unicast IPv4 address"
                )));
            }
            if *route == self.address {
                return Err(DataplaneError::invalid(format!(
                    "peer route {route} is this node's own mesh address"
                )));
            }
            if !seen.insert(*route) {
                return Err(DataplaneError::invalid(format!(
                    "peer route {route} is configured more than once"
                )));
            }
        }

        if let Some(nat) = &self.nat {
            if !valid_nft_name(&nat.table) {
                return Err(DataplaneError::invalid(format!(
                    "nftables table name {:?} is invalid (1-32 letters/digits/_ only)",
                    nat.table
                )));
            }
            if let Some(interface) = &nat.out_interface {
                if !valid_interface_name(interface) {
                    return Err(DataplaneError::invalid(format!(
                        "NAT egress interface {interface:?} is invalid"
                    )));
                }
            }
        }
        Ok(())
    }
}

/// Typed dataplane failure. `code` is stable and controller-friendly; `reason`
/// never contains key material or secrets.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataplaneError {
    pub code: String,
    pub reason: String,
}

impl DataplaneError {
    pub fn new(code: impl Into<String>, reason: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            reason: reason.into(),
        }
    }

    /// Root is required to create links, routes, forwarding state, or nftables
    /// rules.
    pub fn root_required(reason: impl Into<String>) -> Self {
        Self::new("mesh.root-required", reason)
    }

    /// The kernel dataplane or a required tool is unavailable.
    pub fn unavailable(reason: impl Into<String>) -> Self {
        Self::new("mesh.dataplane-unavailable", reason)
    }

    /// The configuration would produce an unsafe or ambiguous invocation.
    pub fn invalid(reason: impl Into<String>) -> Self {
        Self::new("mesh.dataplane-invalid", reason)
    }

    /// A command exited non-zero.
    pub fn command_failed(reason: impl Into<String>) -> Self {
        Self::new("mesh.dataplane-command-failed", reason)
    }

    /// No dataplane configuration was supplied.
    pub fn not_configured(reason: impl Into<String>) -> Self {
        Self::new("mesh.dataplane-not-configured", reason)
    }

    /// A runtime route update was requested while the dataplane is not running.
    pub fn not_running(reason: impl Into<String>) -> Self {
        Self::new("mesh.dataplane-not-running", reason)
    }
}

impl fmt::Display for DataplaneError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "{}: {}", self.code, self.reason)
    }
}

impl std::error::Error for DataplaneError {}

impl From<DataplaneError> for MeshError {
    fn from(error: DataplaneError) -> Self {
        MeshError::Dataplane(error.to_string())
    }
}

/// Dataplane lifecycle. Distinct from peer/handshake state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum DataplaneState {
    /// No dataplane feature was requested.
    NotConfigured,
    /// Configured but not programmed.
    Stopped,
    /// Programming in progress.
    Starting,
    /// Link, routes, forwarding, and optional NAT are all applied.
    Running,
    /// Programming or teardown failed.
    Failed,
}

/// Honest dataplane snapshot.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DataplaneStatus {
    pub configured: bool,
    pub state: DataplaneState,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interface: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mtu: Option<u16>,
    /// True only while `state == running`.
    pub up: bool,
    /// True only while running with forwarding enabled.
    pub forwarding: bool,
    /// Peer `/32` routes, present only while running.
    pub routes: Vec<String>,
    /// True only while running with NAT explicitly configured.
    pub nat: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<DataplaneError>,
}

impl DataplaneStatus {
    pub fn not_configured() -> Self {
        Self {
            configured: false,
            state: DataplaneState::NotConfigured,
            interface: None,
            address: None,
            mtu: None,
            up: false,
            forwarding: false,
            routes: Vec::new(),
            nat: false,
            last_error: None,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Applied {
    Link(String),
    Forwarding { previous: Option<String> },
    Route(Ipv4Addr),
    NatTable(String),
}

struct Inner {
    state: DataplaneState,
    applied: Vec<Applied>,
    error: Option<DataplaneError>,
}

/// Programmable mesh dataplane for one TUN interface.
pub struct MeshDataplane {
    config: DataplaneConfig,
    runner: Arc<dyn CommandRunner>,
    sysfs: Arc<dyn Sysfs>,
    inner: Mutex<Inner>,
}

impl MeshDataplane {
    pub fn new(
        config: DataplaneConfig,
        runner: Arc<dyn CommandRunner>,
        sysfs: Arc<dyn Sysfs>,
    ) -> Self {
        Self {
            config,
            runner,
            sysfs,
            inner: Mutex::new(Inner {
                state: DataplaneState::Stopped,
                applied: Vec::new(),
                error: None,
            }),
        }
    }

    /// Construct with the real kernel/proc/sys seams.
    pub fn system(config: DataplaneConfig) -> Self {
        Self::new(
            config,
            Arc::new(SystemCommandRunner::new()),
            Arc::new(SystemSysfs::new()),
        )
    }

    pub fn config(&self) -> &DataplaneConfig {
        &self.config
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|error| error.into_inner())
    }

    fn set_state(
        &self,
        state: DataplaneState,
        error: Option<DataplaneError>,
        applied: Vec<Applied>,
    ) {
        let mut inner = self.lock();
        inner.state = state;
        inner.error = error;
        inner.applied = applied;
    }

    /// Current snapshot. Fields that imply a live link (`up`, `routes`, `nat`,
    /// `forwarding`) are only true while the state is `running`.
    pub fn status(&self) -> DataplaneStatus {
        let inner = self.lock();
        let running = inner.state == DataplaneState::Running;
        DataplaneStatus {
            configured: true,
            state: inner.state,
            interface: Some(self.config.interface.clone()),
            address: Some(self.config.address_cidr()),
            mtu: Some(self.config.mtu),
            up: running,
            forwarding: running && self.config.forwarding,
            routes: if running {
                inner
                    .applied
                    .iter()
                    .filter_map(|step| match step {
                        Applied::Route(route) => Some(format!("{route}/32")),
                        _ => None,
                    })
                    .collect()
            } else {
                Vec::new()
            },
            nat: running && self.config.nat.is_some(),
            last_error: inner.error.clone(),
        }
    }

    /// Check root, `/dev/net/tun`, `ip`, and (when NAT is configured) `nft`.
    pub fn preflight(&self) -> Result<(), DataplaneError> {
        if !self.sysfs.is_root() {
            return Err(DataplaneError::root_required(
                "mesh dataplane requires root to create/config its TUN link, routes, \
                 forwarding, and firewall state",
            ));
        }
        if !self.sysfs.exists(Path::new(TUN_DEVICE)) {
            return Err(DataplaneError::unavailable(format!(
                "{TUN_DEVICE} is not present; the Linux TUN driver is unavailable"
            )));
        }
        if !self.sysfs.command_available("ip") {
            return Err(DataplaneError::unavailable(
                "the ip binary is not on PATH; mesh dataplane cannot program links/routes",
            ));
        }
        if self.config.nat.is_some() && !self.sysfs.command_available("nft") {
            return Err(DataplaneError::unavailable(
                "NAT is configured but the nft binary is not on PATH",
            ));
        }
        Ok(())
    }

    /// Program the dataplane. Idempotent while already running.
    pub fn start(&self) -> Result<DataplaneStatus, DataplaneError> {
        if self.lock().state == DataplaneState::Running {
            return Ok(self.status());
        }

        // Retry any teardown left over from a previous failed run before
        // programming a fresh link.
        let leftover = self.lock().applied.clone();
        if !leftover.is_empty() {
            if let Some(error) = self.cleanup(&leftover) {
                self.set_state(DataplaneState::Failed, Some(error.clone()), leftover);
                return Err(error);
            }
            self.set_state(DataplaneState::Starting, None, Vec::new());
        }

        if let Err(error) = self.config.validate() {
            self.set_state(DataplaneState::Failed, Some(error.clone()), Vec::new());
            return Err(error);
        }
        if let Err(error) = self.preflight() {
            self.set_state(DataplaneState::Failed, Some(error.clone()), Vec::new());
            return Err(error);
        }

        self.set_state(DataplaneState::Starting, None, Vec::new());
        let mut applied = Vec::new();
        match self.apply(&mut applied) {
            Ok(()) => {
                self.set_state(DataplaneState::Running, None, applied);
                debug!(
                    interface = %self.config.interface,
                    address = %self.config.address_cidr(),
                    mtu = self.config.mtu,
                    routes = self.config.peer_routes.len(),
                    nat = self.config.nat.is_some(),
                    "mesh dataplane programmed"
                );
                Ok(self.status())
            }
            Err(error) => {
                let cleanup_error = self.cleanup(&applied);
                let mut error = error;
                if let Some(cleanup) = &cleanup_error {
                    error.reason =
                        format!("{}; rollback incomplete: {}", error.reason, cleanup.reason);
                }
                // Keep the applied list only when rollback could not complete,
                // so a later stop() can retry the idempotent teardown.
                let retry = if cleanup_error.is_some() {
                    applied
                } else {
                    Vec::new()
                };
                self.set_state(DataplaneState::Failed, Some(error.clone()), retry);
                warn!(
                    code = %error.code,
                    reason = %error.reason,
                    "mesh dataplane start failed"
                );
                Err(error)
            }
        }
    }

    /// Add or replace a peer `/32` route through the TUN while running.
    ///
    /// Idempotent: a route already present is a no-op.
    pub fn add_route(&self, route: Ipv4Addr) -> Result<(), DataplaneError> {
        self.validate_route(route)?;
        let mut inner = self.lock();
        if inner.state != DataplaneState::Running {
            return Err(DataplaneError::not_running(format!(
                "cannot add route {route}/32: mesh dataplane is {:?}",
                inner.state
            )));
        }
        if inner
            .applied
            .iter()
            .any(|step| matches!(step, Applied::Route(existing) if *existing == route))
        {
            return Ok(());
        }
        self.run(&CommandSpec::new(
            "ip",
            [
                "route".to_owned(),
                "replace".to_owned(),
                format!("{route}/32"),
                "dev".to_owned(),
                self.config.interface.clone(),
            ],
        ))?;
        inner.applied.push(Applied::Route(route));
        Ok(())
    }

    /// Remove a peer `/32` route. Absent routes are already clean.
    pub fn remove_route(&self, route: Ipv4Addr) -> Result<(), DataplaneError> {
        let mut inner = self.lock();
        if !inner
            .applied
            .iter()
            .any(|step| matches!(step, Applied::Route(existing) if *existing == route))
        {
            return Ok(());
        }
        self.run_teardown(&CommandSpec::new(
            "ip",
            [
                "route".to_owned(),
                "del".to_owned(),
                format!("{route}/32"),
                "dev".to_owned(),
                self.config.interface.clone(),
            ],
        ))?;
        inner
            .applied
            .retain(|step| !matches!(step, Applied::Route(existing) if *existing == route));
        Ok(())
    }

    fn validate_route(&self, route: Ipv4Addr) -> Result<(), DataplaneError> {
        if !valid_mesh_address(route) {
            return Err(DataplaneError::invalid(format!(
                "peer route {route} is not a usable unicast IPv4 address"
            )));
        }
        if route == self.config.address {
            return Err(DataplaneError::invalid(format!(
                "peer route {route} is this node's own mesh address"
            )));
        }
        Ok(())
    }

    /// Tear down in reverse order. Safe to call repeatedly; a second call with
    /// nothing applied issues no commands.
    pub fn stop(&self) -> Result<(), DataplaneError> {
        let applied = self.lock().applied.clone();
        if applied.is_empty() {
            self.set_state(DataplaneState::Stopped, None, Vec::new());
            return Ok(());
        }

        if let Some(error) = self.cleanup(&applied) {
            self.set_state(DataplaneState::Failed, Some(error.clone()), applied);
            return Err(error);
        }

        self.set_state(DataplaneState::Stopped, None, Vec::new());
        debug!(interface = %self.config.interface, "mesh dataplane stopped");
        Ok(())
    }

    fn apply(&self, applied: &mut Vec<Applied>) -> Result<(), DataplaneError> {
        let interface = &self.config.interface;

        self.run(&CommandSpec::new(
            "ip",
            ["tuntap", "add", "dev", interface, "mode", "tun", "persist"],
        ))?;
        applied.push(Applied::Link(interface.clone()));

        self.run(&CommandSpec::new(
            "ip",
            [
                "link".to_owned(),
                "set".to_owned(),
                "dev".to_owned(),
                interface.clone(),
                "mtu".to_owned(),
                self.config.mtu.to_string(),
            ],
        ))?;

        self.run(&CommandSpec::new(
            "ip",
            [
                "addr".to_owned(),
                "replace".to_owned(),
                self.config.address_cidr(),
                "dev".to_owned(),
                interface.clone(),
            ],
        ))?;

        self.run(&CommandSpec::new(
            "ip",
            [
                "link".to_owned(),
                "set".to_owned(),
                "dev".to_owned(),
                interface.clone(),
                "up".to_owned(),
            ],
        ))?;

        if self.config.forwarding {
            self.enable_forwarding(applied)?;
        }

        for route in &self.config.peer_routes {
            self.run(&CommandSpec::new(
                "ip",
                [
                    "route".to_owned(),
                    "replace".to_owned(),
                    format!("{route}/32"),
                    "dev".to_owned(),
                    interface.clone(),
                ],
            ))?;
            applied.push(Applied::Route(*route));
        }

        if let Some(nat) = &self.config.nat {
            let table = &nat.table;
            self.run(&CommandSpec::new(
                "nft",
                ["add", "table", "ip", table.as_str()],
            ))?;
            applied.push(Applied::NatTable(table.clone()));

            self.run(&CommandSpec::new(
                "nft",
                [
                    "add".to_owned(),
                    "chain".to_owned(),
                    "ip".to_owned(),
                    table.clone(),
                    "postrouting".to_owned(),
                    "{ type nat hook postrouting priority 100 ; }".to_owned(),
                ],
            ))?;

            let egress = match &nat.out_interface {
                Some(out) => format!("oifname \"{out}\""),
                None => format!("oifname != \"{interface}\""),
            };
            let rule = format!(
                "ip saddr {} {egress} masquerade",
                self.config.network_cidr()
            );
            self.run(&CommandSpec::new(
                "nft",
                [
                    "add".to_owned(),
                    "rule".to_owned(),
                    "ip".to_owned(),
                    table.clone(),
                    "postrouting".to_owned(),
                    rule,
                ],
            ))?;
        }

        Ok(())
    }

    fn enable_forwarding(&self, applied: &mut Vec<Applied>) -> Result<(), DataplaneError> {
        let path = Path::new(IPV4_FORWARD);
        let previous = self.sysfs.read_to_string(path).map_err(|error| {
            DataplaneError::unavailable(format!(
                "cannot read {IPV4_FORWARD} before enabling forwarding: {error}"
            ))
        })?;
        if previous.trim() == "1" {
            return Ok(());
        }
        self.sysfs.write(path, "1").map_err(|error| {
            DataplaneError::unavailable(format!("cannot enable IPv4 forwarding: {error}"))
        })?;
        applied.push(Applied::Forwarding {
            previous: Some(previous),
        });
        Ok(())
    }

    fn run(&self, command: &CommandSpec) -> Result<(), DataplaneError> {
        let output = self.runner.run(command).map_err(|error| {
            DataplaneError::command_failed(format!("could not execute `{command}`: {error}"))
        })?;
        if !output.success() {
            let detail = if output.stderr.trim().is_empty() {
                output.stdout.trim()
            } else {
                output.stderr.trim()
            };
            return Err(DataplaneError::command_failed(format!(
                "`{command}` exited with status {}: {}",
                output.status,
                short_reason(detail)
            )));
        }
        Ok(())
    }

    /// Best-effort reverse-order teardown. A missing link/route/table is
    /// treated as already clean so second stops are harmless.
    fn cleanup(&self, applied: &[Applied]) -> Option<DataplaneError> {
        let interface = self.config.interface.as_str();
        let mut first_error = None;
        for step in applied.iter().rev() {
            let result = match step {
                Applied::NatTable(table) => self.run_teardown(&CommandSpec::new(
                    "nft",
                    ["delete", "table", "ip", table.as_str()],
                )),
                Applied::Route(route) => self.run_teardown(&CommandSpec::new(
                    "ip",
                    [
                        "route".to_owned(),
                        "del".to_owned(),
                        format!("{route}/32"),
                        "dev".to_owned(),
                        interface.to_owned(),
                    ],
                )),
                Applied::Forwarding { previous } => {
                    if let Some(previous) = previous {
                        self.sysfs
                            .write(Path::new(IPV4_FORWARD), previous.trim())
                            .map_err(|error| {
                                DataplaneError::unavailable(format!(
                                    "cannot restore {IPV4_FORWARD}: {error}"
                                ))
                            })
                    } else {
                        Ok(())
                    }
                }
                Applied::Link(link) => self.run_teardown(&CommandSpec::new(
                    "ip",
                    ["link", "delete", "dev", link.as_str()],
                )),
            };
            if let Err(error) = result {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        first_error
    }

    fn run_teardown(&self, command: &CommandSpec) -> Result<(), DataplaneError> {
        match self.run(command) {
            Ok(()) => Ok(()),
            Err(error) if is_already_absent(&error.reason) => {
                debug!(command = %command, "mesh dataplane teardown: already absent");
                Ok(())
            }
            Err(error) => Err(error),
        }
    }
}

fn prefix_mask(prefix_len: u8) -> u32 {
    if prefix_len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(prefix_len))
    }
}

fn valid_interface_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 15
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && !name.starts_with('.')
        && name.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '_' | '-' | '.')
        })
}

fn valid_nft_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 32
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || character == '_')
}

fn valid_mesh_address(address: Ipv4Addr) -> bool {
    !address.is_unspecified()
        && !address.is_loopback()
        && !address.is_multicast()
        && address != Ipv4Addr::BROADCAST
}

fn short_reason(reason: &str) -> String {
    if reason.len() <= MAX_REASON_BYTES {
        return reason.to_owned();
    }
    let shortened: String = reason.chars().take(MAX_REASON_BYTES).collect();
    format!("{shortened}…")
}

fn is_already_absent(reason: &str) -> bool {
    [
        "Cannot find device",
        "No such file or directory",
        "No such file",
        "does not exist",
        "not found",
        "No such",
    ]
    .iter()
    .any(|needle| reason.contains(needle))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};
    use std::sync::Weak;

    #[derive(Default)]
    struct FakeRunner {
        calls: Mutex<Vec<CommandSpec>>,
        fail: Mutex<Option<CommandSpec>>,
        probe: Mutex<Option<Weak<MeshDataplane>>>,
        observed: Mutex<Option<DataplaneState>>,
    }

    impl FakeRunner {
        fn calls(&self) -> Vec<CommandSpec> {
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }

        fn fail_on(&self, command: CommandSpec) {
            *self.fail.lock().unwrap_or_else(|error| error.into_inner()) = Some(command);
        }

        fn probe(&self, dataplane: &Arc<MeshDataplane>) {
            *self.probe.lock().unwrap_or_else(|error| error.into_inner()) =
                Some(Arc::downgrade(dataplane));
        }

        fn observed_state(&self) -> Option<DataplaneState> {
            *self
                .observed
                .lock()
                .unwrap_or_else(|error| error.into_inner())
        }
    }

    impl CommandRunner for FakeRunner {
        fn run(&self, command: &CommandSpec) -> io::Result<CommandOutput> {
            if let Some(weak) = self
                .probe
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .take()
            {
                if let Some(dataplane) = weak.upgrade() {
                    *self
                        .observed
                        .lock()
                        .unwrap_or_else(|error| error.into_inner()) =
                        Some(dataplane.status().state);
                }
            }
            self.calls
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push(command.clone());
            if self
                .fail
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .as_ref()
                == Some(command)
            {
                return Ok(CommandOutput {
                    status: 1,
                    stdout: String::new(),
                    stderr: "injected failure".to_owned(),
                });
            }
            Ok(CommandOutput::success_output())
        }
    }

    struct FakeSysfs {
        root: bool,
        files: Mutex<BTreeMap<PathBuf, String>>,
        existing: Mutex<BTreeSet<PathBuf>>,
        tools: Mutex<BTreeSet<String>>,
        writes: Mutex<Vec<(PathBuf, String)>>,
    }

    impl FakeSysfs {
        fn rooted() -> Self {
            let files = BTreeMap::from([(PathBuf::from(IPV4_FORWARD), "0\n".to_owned())]);
            Self {
                root: true,
                files: Mutex::new(files),
                existing: Mutex::new(BTreeSet::from([PathBuf::from(TUN_DEVICE)])),
                tools: Mutex::new(BTreeSet::from(["ip".to_owned(), "nft".to_owned()])),
                writes: Mutex::new(Vec::new()),
            }
        }

        fn without_tool(mut self, tool: &str) -> Self {
            self.tools
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .remove(tool);
            self
        }

        fn without_tun(mut self) -> Self {
            self.existing
                .get_mut()
                .unwrap_or_else(|error| error.into_inner())
                .remove(Path::new(TUN_DEVICE));
            self
        }

        fn not_root(mut self) -> Self {
            self.root = false;
            self
        }

        fn writes(&self) -> Vec<(PathBuf, String)> {
            self.writes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .clone()
        }
    }

    impl FileOpener for FakeSysfs {
        fn exists(&self, path: &Path) -> bool {
            self.existing
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(path)
                || self
                    .files
                    .lock()
                    .unwrap_or_else(|error| error.into_inner())
                    .contains_key(path)
        }

        fn read_to_string(&self, path: &Path) -> io::Result<String> {
            self.files
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .get(path)
                .cloned()
                .ok_or_else(|| io::Error::new(io::ErrorKind::NotFound, "missing fake file"))
        }

        fn write(&self, path: &Path, contents: &str) -> io::Result<()> {
            self.files
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .insert(path.to_owned(), contents.to_owned());
            self.writes
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .push((path.to_owned(), contents.to_owned()));
            Ok(())
        }
    }

    impl Sysfs for FakeSysfs {
        fn is_root(&self) -> bool {
            self.root
        }

        fn command_available(&self, program: &str) -> bool {
            self.tools
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .contains(program)
        }
    }

    fn cmd(program: &str, args: &[&str]) -> CommandSpec {
        CommandSpec::new(program, args.iter().copied())
    }

    fn base_config() -> DataplaneConfig {
        DataplaneConfig::new(
            DEFAULT_INTERFACE,
            Ipv4Addr::new(10, 200, 9, 1),
            DEFAULT_PREFIX_LEN,
            DEFAULT_MTU,
        )
        .with_peer_routes(vec![
            Ipv4Addr::new(10, 200, 9, 2),
            Ipv4Addr::new(10, 200, 9, 3),
        ])
    }

    fn dataplane(
        config: DataplaneConfig,
        sysfs: FakeSysfs,
    ) -> (Arc<MeshDataplane>, Arc<FakeRunner>, Arc<FakeSysfs>) {
        let runner = Arc::new(FakeRunner::default());
        let sysfs = Arc::new(sysfs);
        let dataplane = Arc::new(MeshDataplane::new(config, runner.clone(), sysfs.clone()));
        (dataplane, runner, sysfs)
    }

    fn prefix_calls() -> Vec<CommandSpec> {
        vec![
            cmd(
                "ip",
                &["tuntap", "add", "dev", "ozma0", "mode", "tun", "persist"],
            ),
            cmd("ip", &["link", "set", "dev", "ozma0", "mtu", "1420"]),
            cmd("ip", &["addr", "replace", "10.200.9.1/24", "dev", "ozma0"]),
            cmd("ip", &["link", "set", "dev", "ozma0", "up"]),
        ]
    }

    #[test]
    fn start_programs_expected_commands_in_order() {
        let (dataplane, runner, sysfs) = dataplane(base_config(), FakeSysfs::rooted());
        let status = dataplane.start().unwrap();

        let mut expected = prefix_calls();
        expected.push(cmd(
            "ip",
            &["route", "replace", "10.200.9.2/32", "dev", "ozma0"],
        ));
        expected.push(cmd(
            "ip",
            &["route", "replace", "10.200.9.3/32", "dev", "ozma0"],
        ));
        assert_eq!(runner.calls(), expected);

        assert_eq!(status.state, DataplaneState::Running);
        assert!(status.configured);
        assert!(status.up);
        assert!(status.forwarding);
        assert!(!status.nat);
        assert_eq!(status.interface.as_deref(), Some("ozma0"));
        assert_eq!(status.address.as_deref(), Some("10.200.9.1/24"));
        assert_eq!(status.mtu, Some(1420));
        assert_eq!(
            status.routes,
            vec!["10.200.9.2/32".to_owned(), "10.200.9.3/32".to_owned()]
        );
        assert!(status.last_error.is_none());
        assert_eq!(
            sysfs.writes(),
            vec![(PathBuf::from(IPV4_FORWARD), "1".to_owned())]
        );

        // Starting twice must not reprogram the kernel.
        dataplane.start().unwrap();
        assert_eq!(runner.calls().len(), expected.len());
    }

    #[test]
    fn status_reports_starting_while_commands_run() {
        let runner = Arc::new(FakeRunner::default());
        let sysfs = Arc::new(FakeSysfs::rooted());
        let dataplane = Arc::new(MeshDataplane::new(base_config(), runner.clone(), sysfs));
        runner.probe(&dataplane);

        dataplane.start().unwrap();
        assert_eq!(runner.observed_state(), Some(DataplaneState::Starting));
        assert_eq!(dataplane.status().state, DataplaneState::Running);
    }

    #[test]
    fn nat_is_opt_in_and_uses_exact_nft_argv() {
        let config = base_config().with_nat(NatConfig::new());
        let (dataplane, runner, _sysfs) = dataplane(config, FakeSysfs::rooted());
        let status = dataplane.start().unwrap();

        let mut expected = prefix_calls();
        expected.push(cmd(
            "ip",
            &["route", "replace", "10.200.9.2/32", "dev", "ozma0"],
        ));
        expected.push(cmd(
            "ip",
            &["route", "replace", "10.200.9.3/32", "dev", "ozma0"],
        ));
        expected.push(cmd("nft", &["add", "table", "ip", "ozma_mesh"]));
        expected.push(cmd(
            "nft",
            &[
                "add",
                "chain",
                "ip",
                "ozma_mesh",
                "postrouting",
                "{ type nat hook postrouting priority 100 ; }",
            ],
        ));
        expected.push(cmd(
            "nft",
            &[
                "add",
                "rule",
                "ip",
                "ozma_mesh",
                "postrouting",
                "ip saddr 10.200.9.0/24 oifname != \"ozma0\" masquerade",
            ],
        ));
        assert_eq!(runner.calls(), expected);
        assert!(status.nat);
    }

    #[test]
    fn nat_egress_interface_is_honoured() {
        let config = base_config().with_nat(NatConfig::new().with_out_interface("eth0"));
        let (dataplane, runner, _sysfs) = dataplane(config, FakeSysfs::rooted());
        dataplane.start().unwrap();
        let calls = runner.calls();
        assert_eq!(
            calls.last(),
            Some(&cmd(
                "nft",
                &[
                    "add",
                    "rule",
                    "ip",
                    "ozma_mesh",
                    "postrouting",
                    "ip saddr 10.200.9.0/24 oifname \"eth0\" masquerade",
                ],
            ))
        );
    }

    #[test]
    fn rollback_runs_in_reverse_order_on_route_failure() {
        let (dataplane, runner, sysfs) = dataplane(base_config(), FakeSysfs::rooted());
        runner.fail_on(cmd(
            "ip",
            &["route", "replace", "10.200.9.3/32", "dev", "ozma0"],
        ));

        let error = dataplane.start().unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-command-failed");

        let mut expected = prefix_calls();
        expected.push(cmd(
            "ip",
            &["route", "replace", "10.200.9.2/32", "dev", "ozma0"],
        ));
        expected.push(cmd(
            "ip",
            &["route", "replace", "10.200.9.3/32", "dev", "ozma0"],
        ));
        // Reverse order: applied route, forwarding restore (sysfs), link.
        expected.push(cmd(
            "ip",
            &["route", "del", "10.200.9.2/32", "dev", "ozma0"],
        ));
        expected.push(cmd("ip", &["link", "delete", "dev", "ozma0"]));
        assert_eq!(runner.calls(), expected);
        assert_eq!(
            sysfs.writes(),
            vec![
                (PathBuf::from(IPV4_FORWARD), "1".to_owned()),
                (PathBuf::from(IPV4_FORWARD), "0".to_owned()),
            ]
        );

        let status = dataplane.status();
        assert_eq!(status.state, DataplaneState::Failed);
        assert!(!status.up);
        assert!(status.routes.is_empty());
        assert!(!status.nat);
        assert_eq!(
            status.last_error.as_ref().map(|error| error.code.as_str()),
            Some("mesh.dataplane-command-failed")
        );
    }

    #[test]
    fn rollback_after_nat_failure_deletes_table_before_link() {
        let (dataplane, runner, _sysfs) = dataplane(
            base_config().with_nat(NatConfig::new()),
            FakeSysfs::rooted(),
        );
        runner.fail_on(cmd(
            "nft",
            &[
                "add",
                "rule",
                "ip",
                "ozma_mesh",
                "postrouting",
                "ip saddr 10.200.9.0/24 oifname != \"ozma0\" masquerade",
            ],
        ));

        dataplane.start().unwrap_err();
        let calls = runner.calls();
        assert_eq!(
            &calls[calls.len() - 4..],
            &[
                cmd("nft", &["delete", "table", "ip", "ozma_mesh"]),
                cmd("ip", &["route", "del", "10.200.9.3/32", "dev", "ozma0"]),
                cmd("ip", &["route", "del", "10.200.9.2/32", "dev", "ozma0"]),
                cmd("ip", &["link", "delete", "dev", "ozma0"]),
            ]
        );
        assert_eq!(
            calls[calls.len() - 5],
            cmd(
                "nft",
                &[
                    "add",
                    "rule",
                    "ip",
                    "ozma_mesh",
                    "postrouting",
                    "ip saddr 10.200.9.0/24 oifname != \"ozma0\" masquerade",
                ],
            )
        );
    }

    #[test]
    fn routes_can_be_added_and_removed_while_running() {
        let config = DataplaneConfig::new(
            DEFAULT_INTERFACE,
            Ipv4Addr::new(10, 200, 9, 1),
            DEFAULT_PREFIX_LEN,
            DEFAULT_MTU,
        );
        let (dataplane, runner, _sysfs) = dataplane(config, FakeSysfs::rooted());
        dataplane.start().unwrap();
        let after_start = runner.calls().len();

        dataplane.add_route(Ipv4Addr::new(10, 200, 9, 2)).unwrap();
        assert_eq!(
            &runner.calls()[after_start..],
            &[cmd(
                "ip",
                &["route", "replace", "10.200.9.2/32", "dev", "ozma0"],
            )]
        );
        assert_eq!(dataplane.status().routes, vec!["10.200.9.2/32".to_owned()]);

        let after_add = runner.calls().len();
        dataplane.add_route(Ipv4Addr::new(10, 200, 9, 2)).unwrap();
        assert_eq!(
            runner.calls().len(),
            after_add,
            "add_route must be idempotent"
        );

        dataplane
            .remove_route(Ipv4Addr::new(10, 200, 9, 2))
            .unwrap();
        assert_eq!(
            &runner.calls()[after_add..],
            &[cmd(
                "ip",
                &["route", "del", "10.200.9.2/32", "dev", "ozma0"],
            )]
        );
        assert!(dataplane.status().routes.is_empty());

        let after_remove = runner.calls().len();
        dataplane
            .remove_route(Ipv4Addr::new(10, 200, 9, 2))
            .unwrap();
        assert_eq!(
            runner.calls().len(),
            after_remove,
            "remove_route must be idempotent"
        );
    }

    #[test]
    fn stop_is_idempotent_and_reverses_applied_steps() {
        let (dataplane, runner, sysfs) = dataplane(base_config(), FakeSysfs::rooted());
        dataplane.start().unwrap();
        let start_calls = runner.calls().len();

        dataplane.stop().unwrap();
        let calls = runner.calls();
        assert_eq!(
            &calls[start_calls..],
            &[
                cmd("ip", &["route", "del", "10.200.9.3/32", "dev", "ozma0"]),
                cmd("ip", &["route", "del", "10.200.9.2/32", "dev", "ozma0"]),
                cmd("ip", &["link", "delete", "dev", "ozma0"]),
            ]
        );
        assert_eq!(
            sysfs.writes().last(),
            Some(&(PathBuf::from(IPV4_FORWARD), "0".to_owned()))
        );

        let status = dataplane.status();
        assert_eq!(status.state, DataplaneState::Stopped);
        assert!(!status.up);
        assert!(status.routes.is_empty());
        assert!(status.last_error.is_none());

        let after_stop = runner.calls().len();
        dataplane.stop().unwrap();
        assert_eq!(runner.calls().len(), after_stop);
    }

    #[test]
    fn missing_root_tun_and_tools_are_typed() {
        let (no_root, no_root_runner, _) = dataplane(base_config(), FakeSysfs::rooted().not_root());
        let error = no_root.start().unwrap_err();
        assert_eq!(error.code, "mesh.root-required");
        assert!(no_root_runner.calls().is_empty());
        assert_eq!(no_root.status().state, DataplaneState::Failed);

        let (no_tun, no_tun_runner, _) =
            dataplane(base_config(), FakeSysfs::rooted().without_tun());
        let error = no_tun.start().unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-unavailable");
        assert!(error.reason.contains(TUN_DEVICE));
        assert!(no_tun_runner.calls().is_empty());

        let (no_ip, no_ip_runner, _) =
            dataplane(base_config(), FakeSysfs::rooted().without_tool("ip"));
        let error = no_ip.start().unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-unavailable");
        assert!(error.reason.contains("ip binary"));
        assert!(no_ip_runner.calls().is_empty());

        // nft is only required when NAT is explicitly configured.
        let (no_nft_no_nat, _, _) =
            dataplane(base_config(), FakeSysfs::rooted().without_tool("nft"));
        no_nft_no_nat.start().unwrap();

        let (no_nft_nat, no_nft_runner, _) = dataplane(
            base_config().with_nat(NatConfig::new()),
            FakeSysfs::rooted().without_tool("nft"),
        );
        let error = no_nft_nat.start().unwrap_err();
        assert_eq!(error.code, "mesh.dataplane-unavailable");
        assert!(error.reason.contains("nft"));
        assert!(no_nft_runner.calls().is_empty());
    }

    #[test]
    fn invalid_address_mtu_prefix_interface_and_nat_are_typed() {
        let cases = [
            DataplaneConfig::new("ozma0", Ipv4Addr::new(10, 200, 9, 1), 24, 67),
            DataplaneConfig::new("ozma0", Ipv4Addr::new(10, 200, 9, 1), 0, 1420),
            DataplaneConfig::new("ozma0", Ipv4Addr::new(10, 200, 9, 1), 33, 1420),
            DataplaneConfig::new("ozma0", Ipv4Addr::UNSPECIFIED, 24, 1420),
            DataplaneConfig::new("ozma0", Ipv4Addr::LOCALHOST, 24, 1420),
            DataplaneConfig::new(
                "interface-name-is-too-long",
                Ipv4Addr::new(10, 200, 9, 1),
                24,
                1420,
            ),
            DataplaneConfig::new("ozma/0", Ipv4Addr::new(10, 200, 9, 1), 24, 1420),
            base_config().with_nat(NatConfig::new().with_table("bad table")),
            base_config().with_peer_routes(vec![
                Ipv4Addr::new(10, 200, 9, 2),
                Ipv4Addr::new(10, 200, 9, 2),
            ]),
            base_config().with_peer_routes(vec![Ipv4Addr::new(10, 200, 9, 1)]),
        ];

        for (index, config) in cases.into_iter().enumerate() {
            let error = config.validate().unwrap_err();
            assert_eq!(
                error.code, "mesh.dataplane-invalid",
                "case {index} produced {error:?}"
            );

            let (dataplane, runner, _) = dataplane(config, FakeSysfs::rooted());
            let error = dataplane.start().unwrap_err();
            assert_eq!(error.code, "mesh.dataplane-invalid");
            assert!(runner.calls().is_empty());
        }
    }

    #[test]
    fn forwarding_disabled_does_not_touch_sysctl() {
        let config = base_config().with_forwarding(false);
        let (dataplane, runner, sysfs) = dataplane(config, FakeSysfs::rooted());
        let status = dataplane.start().unwrap();
        assert!(!status.forwarding);
        assert!(sysfs.writes().is_empty());

        let start_calls = runner.calls().len();
        dataplane.stop().unwrap();
        assert_eq!(
            &runner.calls()[start_calls..],
            &[
                cmd("ip", &["route", "del", "10.200.9.3/32", "dev", "ozma0"]),
                cmd("ip", &["route", "del", "10.200.9.2/32", "dev", "ozma0"]),
                cmd("ip", &["link", "delete", "dev", "ozma0"]),
            ]
        );
        assert!(sysfs.writes().is_empty());
    }

    #[test]
    fn teardown_treats_missing_link_as_already_clean() {
        struct MissingRunner;
        impl CommandRunner for MissingRunner {
            fn run(&self, _command: &CommandSpec) -> io::Result<CommandOutput> {
                Ok(CommandOutput {
                    status: 1,
                    stdout: String::new(),
                    stderr: "Cannot find device \"ozma0\"".to_owned(),
                })
            }
        }

        // Program with a successful runner, then swap is not possible; instead
        // exercise cleanup directly through a fresh instance with the failing
        // runner by starting from an already-failed state is not needed. This
        // test verifies the benign-error classifier used by teardown.
        assert!(is_already_absent("Cannot find device \"ozma0\""));
        assert!(is_already_absent("No such file or directory"));
        assert!(!is_already_absent("Operation not permitted"));

        let dataplane = MeshDataplane::new(
            base_config(),
            Arc::new(MissingRunner),
            Arc::new(FakeSysfs::rooted()),
        );
        // Nothing applied: stop must not invoke the runner at all.
        dataplane.stop().unwrap();
        assert_eq!(dataplane.status().state, DataplaneState::Stopped);
    }

    /// Live kernel test. Ignored by default and additionally gated by
    /// `OZMA_MESH_LIVE=1`; requires root, `/dev/net/tun`, and `ip`.
    #[test]
    #[ignore = "requires root, /dev/net/tun and ip; set OZMA_MESH_LIVE=1"]
    fn live_dataplane_roundtrip() {
        if std::env::var("OZMA_MESH_LIVE").ok().as_deref() != Some("1") {
            return;
        }
        let config = DataplaneConfig::new("ozmatest0", Ipv4Addr::new(10, 253, 77, 1), 24, 1420);
        let dataplane = MeshDataplane::system(config);
        if let Err(error) = dataplane.start() {
            panic!("live dataplane start failed: {error}");
        }
        assert_eq!(dataplane.status().state, DataplaneState::Running);
        dataplane.stop().unwrap();
        dataplane.stop().unwrap();
        assert_eq!(dataplane.status().state, DataplaneState::Stopped);
    }
}
