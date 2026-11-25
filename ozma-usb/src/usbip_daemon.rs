// SPDX-License-Identifier: AGPL-3.0-only
//! Minimal in-process USB/IP control-plane daemon.
//!
//! This module is the state/authorization layer, not the wire listener: the
//! version-`0x0111` control codec and bounded stream server are
//! [`crate::usbip_protocol`] and [`crate::usbip_server`]. This daemon does not
//! fake TLS.
//! It models the safe, testable part a node can own without `vhci_hcd`:
//! starting a bounded listener state, enforcing the existing host/token/key
//! authorization, listing/exporting local devices, and tracking peer
//! connections and hotplug events. The kernel-facing `attach`/`detach`
//! operations are delegated to an injected [`UsbIpKernelBackend`].
//!
//! The default backend is [`NoUsbIpKernel`], which returns a typed
//! [`UsbError::UsbIpKernelRequired`] for every operation that needs
//! `vhci_hcd`/`usbip-host` and root privileges. That is deliberate: status and
//! authorization are real, but a successful USB/IP attach is never fabricated
//! when the kernel data path is absent.
//!
//! TLS remains the historical fail-closed policy from [`UsbIpConfig`]:
//! [`UsbIpTlsPolicy::Required`] is rejected at validation, and
//! [`UsbIpTlsStatus::unsupported`] documents that an external `usbipd` must sit
//! behind a TLS terminator or tunnel. No in-process TLS is claimed.
//!
//! The daemon never stores a raw token or presented key fingerprint: accepted
//! peers are represented by [`UsbIpPeerGrant`], which retains only
//! non-secret metadata. The bounded [`UsbIpHotplugLog`] is reused for
//! export/unexport/attach/detach transitions.

use std::sync::{Arc, Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::usbip::{
    validate_bus_id, validate_host, validate_port_number, validate_vhci_port, UsbIpAttachment,
    UsbIpAuthorization, UsbIpConfig, UsbIpDevice, UsbIpHotplugEvent, UsbIpHotplugKind,
    UsbIpHotplugLog, UsbIpPeerGrant, UsbIpTlsPolicy, UsbIpTlsStatus, DEFAULT_USBIP_PORT,
    MAX_USBIP_HOTPLUG_EVENTS,
};
use crate::usbip_protocol::UsbIpDeviceDescriptor;

/// Maximum simultaneously tracked USB/IP peers.
pub const MAX_USBIP_DAEMON_CONNECTIONS: usize = 64;

/// Whether the in-process USB/IP listener state is active.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbIpDaemonState {
    /// Not started (or explicitly stopped).
    #[default]
    Stopped,
    /// Started and accepting bounded peer sessions.
    Listening,
}

impl UsbIpDaemonState {
    /// Stable lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::Listening => "listening",
        }
    }
}

/// Control-plane configuration for [`UsbIpDaemon`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpDaemonConfig {
    /// Advertised server host.
    pub host: String,
    /// Advertised USB/IP port.
    pub port: u16,
    /// Host/token/key authorization inherited from the USB/IP model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<UsbIpAuthorization>,
    /// TLS policy. `Required` fails closed because this crate does not own TLS.
    #[serde(default, skip_serializing_if = "UsbIpTlsPolicy::is_disabled")]
    pub tls: UsbIpTlsPolicy,
    /// Maximum tracked peer connections.
    pub max_connections: usize,
    /// Retained hotplug events (clamped through [`UsbIpHotplugLog`]).
    pub hotplug_capacity: usize,
}

impl Default for UsbIpDaemonConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_owned(),
            port: DEFAULT_USBIP_PORT,
            authorization: None,
            tls: UsbIpTlsPolicy::Disabled,
            max_connections: 8,
            hotplug_capacity: MAX_USBIP_HOTPLUG_EVENTS,
        }
    }
}

impl UsbIpDaemonConfig {
    /// A stopped-by-default server configuration.
    pub fn server(host: impl Into<String>, port: u16) -> Self {
        Self {
            host: host.into(),
            port,
            ..Self::default()
        }
    }

    /// Copy host/port/authorization/TLS from an existing USB/IP config.
    pub fn from_usbip(config: &UsbIpConfig) -> Self {
        Self {
            host: config.host.clone(),
            port: config.port,
            authorization: config.authorization.clone(),
            tls: config.tls,
            ..Self::default()
        }
    }

    /// Attach host/token/key authorization.
    pub fn with_authorization(mut self, authorization: UsbIpAuthorization) -> Self {
        self.authorization = Some(authorization);
        self
    }

    /// Set the TLS policy.
    pub fn with_tls(mut self, tls: UsbIpTlsPolicy) -> Self {
        self.tls = tls;
        self
    }

    /// Set connection/event bounds.
    pub fn with_limits(mut self, max_connections: usize, hotplug_capacity: usize) -> Self {
        self.max_connections = max_connections;
        self.hotplug_capacity = hotplug_capacity;
        self
    }

    /// Fixed capability answer for the configured TLS policy.
    pub fn tls_status(&self) -> UsbIpTlsStatus {
        UsbIpTlsStatus::unsupported(self.tls)
    }

    /// Validate host, port, authorization, TLS, and bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        validate_host(&self.host)?;
        validate_port_number(self.port)?;
        if !(1..=MAX_USBIP_DAEMON_CONNECTIONS).contains(&self.max_connections) {
            return Err(UsbError::InvalidUsbIp {
                field: "max_connections",
                reason: format!("max_connections must be 1..={MAX_USBIP_DAEMON_CONNECTIONS}"),
            });
        }
        if !(1..=MAX_USBIP_HOTPLUG_EVENTS).contains(&self.hotplug_capacity) {
            return Err(UsbError::InvalidUsbIp {
                field: "hotplug_capacity",
                reason: format!("hotplug_capacity must be 1..={MAX_USBIP_HOTPLUG_EVENTS}"),
            });
        }
        if matches!(self.tls, UsbIpTlsPolicy::Required) {
            return Err(UsbError::UsbIpTlsUnsupported {
                reason: self.tls_status().reason,
            });
        }
        if let Some(authorization) = &self.authorization {
            authorization.validate()?;
        }
        Ok(())
    }
}

/// Kernel side of USB/IP export/attach/detach.
///
/// Implementations must be explicit. The default [`NoUsbIpKernel`] never runs a
/// command or touches a kernel module and reports a typed unsupported error.
pub trait UsbIpKernelBackend: Send + Sync {
    /// Whether a real `vhci_hcd`/`usbip-host` path is available.
    fn available(&self) -> bool {
        false
    }

    /// Export a local device (`usbip bind -b <bus_id>`).
    fn export(&self, bus_id: &str) -> Result<(), UsbError> {
        let _ = bus_id;
        Err(kernel_required("export"))
    }

    /// Stop exporting a local device (`usbip unbind -b <bus_id>`).
    fn unexport(&self, bus_id: &str) -> Result<(), UsbError> {
        let _ = bus_id;
        Err(kernel_required("unexport"))
    }

    /// Attach a remote device and return the local `vhci` port.
    fn attach(&self, host: &str, bus_id: &str) -> Result<u16, UsbError> {
        let _ = (host, bus_id);
        Err(kernel_required("attach"))
    }

    /// Detach a local `vhci` port.
    fn detach(&self, port: u16) -> Result<(), UsbError> {
        let _ = port;
        Err(kernel_required("detach"))
    }
}

/// Explicitly unavailable kernel backend.
#[derive(Clone, Copy, Debug, Default)]
pub struct NoUsbIpKernel;

fn kernel_required(operation: &'static str) -> UsbError {
    UsbError::UsbIpKernelRequired {
        operation,
        reason: "the kernel vhci_hcd/usbip-host path is not available; load the modules, \
                 run with the required privileges, and inject a UsbIpKernelBackend. \
                 No usbip command was run and no device state was fabricated."
            .to_owned(),
    }
}

impl UsbIpKernelBackend for NoUsbIpKernel {}

/// One accepted peer session. Deliberately contains no token or fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpConnection {
    /// Monotonic connection id within this daemon instance.
    pub id: u64,
    /// Peer host.
    pub host: String,
    /// Non-secret pairing metadata.
    pub grant: UsbIpPeerGrant,
}

/// Current daemon state, exports, attachments, peer count, and TLS status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpDaemonStatus {
    /// Stopped or listening.
    pub state: UsbIpDaemonState,
    /// Advertised host.
    pub host: String,
    /// Advertised port.
    pub port: u16,
    /// Locally exported devices recorded by this daemon.
    pub exported: Vec<UsbIpDevice>,
    /// Remote devices attached through the kernel backend.
    pub attached: Vec<UsbIpAttachment>,
    /// Tracked peer connections.
    pub connections: usize,
    /// Connection ceiling.
    pub max_connections: usize,
    /// Retained hotplug events.
    pub hotplug_events: usize,
    /// Hotplug event capacity.
    pub hotplug_capacity: usize,
    /// Whether the injected kernel backend reports availability.
    pub kernel_available: bool,
    /// Fixed TLS capability answer (never claims in-process TLS).
    pub tls: UsbIpTlsStatus,
}

struct DaemonInner {
    state: UsbIpDaemonState,
    exported: Vec<UsbIpDevice>,
    descriptors: Vec<UsbIpDeviceDescriptor>,
    attached: Vec<UsbIpAttachment>,
    connections: Vec<UsbIpConnection>,
    next_connection: u64,
    hotplug: UsbIpHotplugLog,
}

/// In-process USB/IP daemon/control-plane state machine.
pub struct UsbIpDaemon {
    config: UsbIpDaemonConfig,
    kernel: Arc<dyn UsbIpKernelBackend>,
    inner: Mutex<DaemonInner>,
}

impl std::fmt::Debug for UsbIpDaemon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let inner = self.lock();
        formatter
            .debug_struct("UsbIpDaemon")
            .field("host", &self.config.host)
            .field("port", &self.config.port)
            .field("state", &inner.state)
            .field("exported", &inner.exported.len())
            .field("attached", &inner.attached.len())
            .field("connections", &inner.connections.len())
            .field("max_connections", &self.config.max_connections)
            .field("kernel_available", &self.kernel.available())
            .field("tls", &self.config.tls_status())
            .finish_non_exhaustive()
    }
}

impl UsbIpDaemon {
    /// Construct a daemon with the explicit [`NoUsbIpKernel`] backend.
    pub fn new(config: UsbIpDaemonConfig) -> Result<Self, UsbError> {
        config.validate()?;
        let hotplug = UsbIpHotplugLog::new(config.hotplug_capacity);
        Ok(Self {
            config,
            kernel: Arc::new(NoUsbIpKernel),
            inner: Mutex::new(DaemonInner {
                state: UsbIpDaemonState::Stopped,
                exported: Vec::new(),
                descriptors: Vec::new(),
                attached: Vec::new(),
                connections: Vec::new(),
                next_connection: 1,
                hotplug,
            }),
        })
    }

    /// Inject a kernel backend (a test fake or a real `usbip`-driven adapter).
    pub fn with_kernel(mut self, kernel: Arc<dyn UsbIpKernelBackend>) -> Self {
        self.kernel = kernel;
        self
    }

    /// Configuration.
    pub fn config(&self) -> &UsbIpDaemonConfig {
        &self.config
    }

    /// Whether the daemon has been started.
    pub fn is_running(&self) -> bool {
        self.lock().state == UsbIpDaemonState::Listening
    }

    /// Start accepting bounded peer sessions. Idempotent.
    pub fn start(&self) -> Result<UsbIpDaemonStatus, UsbError> {
        self.config.validate()?;
        self.lock().state = UsbIpDaemonState::Listening;
        Ok(self.status())
    }

    /// Stop the daemon and drop all tracked peer sessions. Idempotent.
    ///
    /// Exports/attachments remain recorded so the caller can explicitly
    /// `unexport`/`detach`; this method never pretends kernel cleanup happened.
    pub fn stop(&self) -> Result<UsbIpDaemonStatus, UsbError> {
        let mut inner = self.lock();
        inner.state = UsbIpDaemonState::Stopped;
        inner.connections.clear();
        Ok(self.status_locked(&inner))
    }

    /// Snapshot without changing state.
    pub fn status(&self) -> UsbIpDaemonStatus {
        let inner = self.lock();
        self.status_locked(&inner)
    }

    /// Currently exported local devices.
    pub fn list(&self) -> Result<Vec<UsbIpDevice>, UsbError> {
        let inner = self.require_running()?;
        Ok(inner.exported.clone())
    }

    /// Wire-protocol descriptors for the currently exported devices.
    ///
    /// Only devices exported through [`Self::export_with_descriptor`] (or
    /// [`Self::export_with_token_and_descriptor`]) have a descriptor. An
    /// export without one is deliberately not advertised by
    /// `OP_REP_DEVLIST`: this daemon never invents a path, bus number, device
    /// number, speed, or interface list.
    pub fn device_list(&self) -> Result<Vec<UsbIpDeviceDescriptor>, UsbError> {
        let inner = self.require_running()?;
        Ok(inner
            .descriptors
            .iter()
            .filter(|descriptor| {
                inner
                    .exported
                    .iter()
                    .any(|device| device.bus_id == descriptor.bus_id)
            })
            .cloned()
            .collect())
    }

    /// Export a local device without peer authorization (local administrator).
    pub fn export(&self, bus_id: &str) -> Result<UsbIpDevice, UsbError> {
        self.export_inner(bus_id, None, None)
    }

    /// Export a local device together with the descriptor served by
    /// `OP_REP_DEVLIST`.
    pub fn export_with_descriptor(
        &self,
        bus_id: &str,
        descriptor: UsbIpDeviceDescriptor,
    ) -> Result<UsbIpDevice, UsbError> {
        self.export_inner(bus_id, None, Some(descriptor))
    }

    /// Export a local device after pairing a peer with host/token/fingerprint.
    pub fn export_with_token(
        &self,
        bus_id: &str,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<UsbIpDevice, UsbError> {
        validate_host(host)?;
        let grant = self.authorize_peer(host, token, key_fingerprint)?;
        self.export_inner(bus_id, Some(grant), None)
    }

    /// Peer-authorized export with a wire-protocol descriptor.
    pub fn export_with_token_and_descriptor(
        &self,
        bus_id: &str,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
        descriptor: UsbIpDeviceDescriptor,
    ) -> Result<UsbIpDevice, UsbError> {
        validate_host(host)?;
        let grant = self.authorize_peer(host, token, key_fingerprint)?;
        self.export_inner(bus_id, Some(grant), Some(descriptor))
    }

    fn export_inner(
        &self,
        bus_id: &str,
        grant: Option<UsbIpPeerGrant>,
        descriptor: Option<UsbIpDeviceDescriptor>,
    ) -> Result<UsbIpDevice, UsbError> {
        validate_bus_id(bus_id)?;
        if let Some(descriptor) = &descriptor {
            descriptor.validate()?;
            if descriptor.bus_id != bus_id {
                return Err(UsbError::InvalidUsbIp {
                    field: "bus_id",
                    reason: format!(
                        "descriptor bus id {:?} does not match exported device {bus_id:?}",
                        descriptor.bus_id
                    ),
                });
            }
        }
        let mut inner = self.require_running()?;
        if inner.exported.iter().any(|device| device.bus_id == bus_id) {
            return Err(UsbError::InvalidUsbIp {
                field: "bus_id",
                reason: format!("device {bus_id:?} is already exported"),
            });
        }
        self.kernel.export(bus_id)?;
        let device = UsbIpDevice::local(bus_id);
        inner.exported.push(device.clone());
        if let Some(descriptor) = descriptor {
            inner.descriptors.push(descriptor);
        }
        let host = grant.as_ref().map(|grant| grant.host.clone());
        inner
            .hotplug
            .record(UsbIpHotplugKind::Export, bus_id, host, None);
        Ok(device)
    }

    /// Stop exporting a local device. Idempotent when it is not exported.
    pub fn unexport(&self, bus_id: &str) -> Result<(), UsbError> {
        validate_bus_id(bus_id)?;
        let mut inner = self.require_running()?;
        self.kernel.unexport(bus_id)?;
        let before = inner.exported.len();
        inner.exported.retain(|device| device.bus_id != bus_id);
        inner
            .descriptors
            .retain(|descriptor| descriptor.bus_id != bus_id);
        if inner.exported.len() != before {
            inner
                .hotplug
                .record(UsbIpHotplugKind::Unexport, bus_id, None, None);
        }
        Ok(())
    }

    /// Alias for [`Self::unexport`] matching `usbip unbind -b <bus_id>`.
    pub fn unbind(&self, bus_id: &str) -> Result<(), UsbError> {
        self.unexport(bus_id)
    }

    /// Attach a remote device after host/token/fingerprint authorization.
    pub fn attach(
        &self,
        host: &str,
        bus_id: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<UsbIpAttachment, UsbError> {
        validate_host(host)?;
        validate_bus_id(bus_id)?;
        let grant = self.authorize_peer(host, token, key_fingerprint)?;
        let mut inner = self.require_running()?;
        if inner
            .attached
            .iter()
            .any(|attachment| attachment.host == grant.host && attachment.bus_id == bus_id)
        {
            return Err(UsbError::InvalidUsbIp {
                field: "bus_id",
                reason: format!(
                    "device {bus_id:?} is already attached from {:?}",
                    grant.host
                ),
            });
        }
        let port = self.kernel.attach(&grant.host, bus_id)?;
        let attachment = UsbIpAttachment {
            host: grant.host.clone(),
            bus_id: bus_id.to_owned(),
            port: Some(port),
            vendor_id: None,
            product_id: None,
        };
        inner.attached.push(attachment.clone());
        inner.hotplug.record(
            UsbIpHotplugKind::Attach,
            bus_id,
            Some(grant.host),
            Some(port),
        );
        Ok(attachment)
    }

    /// Attach without a fingerprint; fails closed when one is configured.
    pub fn attach_with_token(
        &self,
        host: &str,
        bus_id: &str,
        token: Option<&str>,
    ) -> Result<UsbIpAttachment, UsbError> {
        self.attach(host, bus_id, token, None)
    }

    /// Detach a previously attached local port and record cleanup.
    pub fn detach(&self, port: u16) -> Result<(), UsbError> {
        validate_vhci_port(port)?;
        let mut inner = self.require_running()?;
        let Some(index) = inner
            .attached
            .iter()
            .position(|attachment| attachment.port == Some(port))
        else {
            return Err(UsbError::InvalidUsbIp {
                field: "port",
                reason: format!("no USB/IP attachment uses local port {port}"),
            });
        };
        self.kernel.detach(port)?;
        let attachment = inner.attached.remove(index);
        inner.hotplug.record(
            UsbIpHotplugKind::Detach,
            attachment.bus_id,
            Some(attachment.host),
            Some(port),
        );
        Ok(())
    }

    /// Accept a bounded peer connection after authorization.
    ///
    /// The daemon does not serve the USB/IP wire protocol; a connection is a
    /// control-plane authorization session whose secret material is not kept.
    pub fn accept_connection(
        &self,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<UsbIpConnection, UsbError> {
        validate_host(host)?;
        let grant = self.authorize_peer(host, token, key_fingerprint)?;
        let mut inner = self.require_running()?;
        if inner.connections.len() >= self.config.max_connections {
            return Err(UsbError::UsbIpConnectionLimit {
                limit: self.config.max_connections,
            });
        }
        let connection = UsbIpConnection {
            id: inner.next_connection,
            host: grant.host.clone(),
            grant,
        };
        inner.next_connection = inner.next_connection.saturating_add(1);
        inner.connections.push(connection.clone());
        Ok(connection)
    }

    /// Close one tracked peer connection.
    pub fn close_connection(&self, id: u64) -> Result<UsbIpConnection, UsbError> {
        let mut inner = self.require_running()?;
        let Some(index) = inner
            .connections
            .iter()
            .position(|connection| connection.id == id)
        else {
            return Err(UsbError::InvalidUsbIp {
                field: "connection",
                reason: format!("no USB/IP connection has id {id}"),
            });
        };
        Ok(inner.connections.remove(index))
    }

    /// Tracked peer connections (secrets already discarded).
    pub fn connections(&self) -> Vec<UsbIpConnection> {
        self.lock().connections.clone()
    }

    /// Retained hotplug events, oldest first.
    pub fn hotplug_events(&self) -> Vec<UsbIpHotplugEvent> {
        self.lock().hotplug.events()
    }

    /// Remove and return retained hotplug events.
    pub fn take_hotplug_events(&self) -> Vec<UsbIpHotplugEvent> {
        self.lock().hotplug.drain()
    }

    /// Retained-event capacity.
    pub fn hotplug_capacity(&self) -> usize {
        self.lock().hotplug.capacity()
    }

    fn authorize_peer(
        &self,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<UsbIpPeerGrant, UsbError> {
        match &self.config.authorization {
            Some(authorization) => authorization.authorize_peer(host, token, key_fingerprint),
            None => Ok(UsbIpPeerGrant {
                host: host.to_owned(),
                token_presented: token.is_some(),
                key: None,
            }),
        }
    }

    fn status_locked(&self, inner: &DaemonInner) -> UsbIpDaemonStatus {
        UsbIpDaemonStatus {
            state: inner.state,
            host: self.config.host.clone(),
            port: self.config.port,
            exported: inner.exported.clone(),
            attached: inner.attached.clone(),
            connections: inner.connections.len(),
            max_connections: self.config.max_connections,
            hotplug_events: inner.hotplug.len(),
            hotplug_capacity: inner.hotplug.capacity(),
            kernel_available: self.kernel.available(),
            tls: self.config.tls_status(),
        }
    }

    fn require_running(&self) -> Result<MutexGuard<'_, DaemonInner>, UsbError> {
        let inner = self.lock();
        if inner.state == UsbIpDaemonState::Listening {
            Ok(inner)
        } else {
            Err(UsbError::UsbIpDaemonStopped)
        }
    }

    fn lock(&self) -> MutexGuard<'_, DaemonInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}
