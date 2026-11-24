// SPDX-License-Identifier: AGPL-3.0-only
//! USB/IP configuration, state, and subprocess control.
//!
//! The model covers the parts of `usbip` that a node can safely configure
//! without a shell: whether USB/IP is enabled, the default server/client host
//! and port, optional pairing metadata (allowed hosts and key fingerprints),
//! local exportable devices, and currently imported devices.
//!
//! The controller is argv-only and delegates every operation to an injected
//! [`CommandRunner`]. It never loads `vhci_hcd`, starts a daemon, opens a
//! socket, or implements the USB/IP wire protocol. Host/token allow-lists and
//! key-fingerprint pairing are enforced before `usbip attach` and before a
//! peer-authorized export; tokens and fingerprints are compared in constant
//! time and are never retained in grants or emitted in errors. TLS is **not**
//! terminated in-process (`UsbIpTlsPolicy::Required` fails closed) because this
//! crate does not own the USB/IP control socket; an external `usbipd`,
//! firewall, or TLS terminator must protect the wire protocol. Export, attach,
//! and detach transitions are recorded in a bounded [`UsbIpHotplugLog`].

use std::collections::VecDeque;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::subprocess::{CommandOutput, CommandRunner, CommandSpec, SystemCommandRunner};

/// Default USB/IP TCP port.
pub const DEFAULT_USBIP_PORT: u16 = 3240;

/// Maximum number of devices modelled in one configuration/status snapshot.
pub const MAX_USBIP_DEVICES: usize = 256;

/// Maximum number of retained hotplug events.
pub const MAX_USBIP_HOTPLUG_EVENTS: usize = 64;

/// Whether USB/IP transport encryption is required.
///
/// `ozma-usb` drives the external `usbip` CLI and never owns a USB/IP socket,
/// so it cannot terminate TLS for the USB/IP wire protocol. `Required` fails
/// closed with [`UsbError::UsbIpTlsUnsupported`] rather than silently running
/// plaintext; `Disabled` leaves TLS to an external `usbipd`/terminator.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbIpTlsPolicy {
    /// Do not require in-process TLS (the historical behaviour).
    #[default]
    Disabled,
    /// Require TLS. Unsupported in this crate, so validation fails closed.
    Required,
}

/// Capability snapshot for USB/IP TLS.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpTlsStatus {
    /// Configured policy.
    pub policy: UsbIpTlsPolicy,
    /// Whether this crate can terminate USB/IP TLS in-process.
    pub available: bool,
    /// Why TLS is unavailable (stable prose for API surfaces).
    pub reason: String,
}

impl UsbIpTlsStatus {
    /// The fixed capability answer for this crate.
    pub fn unsupported(policy: UsbIpTlsPolicy) -> Self {
        Self {
            policy,
            available: false,
            reason: "ozma-usb delegates USB/IP to the external usbip/usbipd tools \
                     and does not own the control socket or the kernel vhci data path, \
                     so it cannot wrap the USB/IP wire protocol in TLS. Run usbipd \
                     behind a TLS terminator or tunnel (for example WireGuard or stunnel) \
                     and keep the USB/IP port on a trusted network."
                .to_owned(),
        }
    }
}

/// A USB device as reported by `usbip` or named in node configuration.
///
/// `host` is `None` for a local (exportable) device and `Some(host)` for a
/// device imported from a remote node.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpDevice {
    /// USB/IP bus id (`1-1.2`, `3-4`).
    pub bus_id: String,
    /// Vendor ID when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u16>,
    /// Product ID when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_id: Option<u16>,
    /// Human-readable description from the listing.
    #[serde(default)]
    pub description: String,
    /// Remote host for an imported device; `None` for a local export.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
}

impl UsbIpDevice {
    /// A local exportable device.
    pub fn local(bus_id: impl Into<String>) -> Self {
        Self {
            bus_id: bus_id.into(),
            vendor_id: None,
            product_id: None,
            description: String::new(),
            host: None,
        }
    }

    /// A device expected on a remote USB/IP host.
    pub fn remote(host: impl Into<String>, bus_id: impl Into<String>) -> Self {
        Self {
            bus_id: bus_id.into(),
            vendor_id: None,
            product_id: None,
            description: String::new(),
            host: Some(host.into()),
        }
    }

    /// Set the USB vendor/product IDs.
    pub fn with_ids(mut self, vendor_id: u16, product_id: u16) -> Self {
        self.vendor_id = Some(vendor_id);
        self.product_id = Some(product_id);
        self
    }

    /// Set the description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.description = description.into();
        self
    }

    /// The `vid:pid` pair, when both IDs are known.
    pub fn vid_pid(&self) -> Option<(u16, u16)> {
        match (self.vendor_id, self.product_id) {
            (Some(vendor), Some(product)) => Some((vendor, product)),
            _ => None,
        }
    }

    /// Whether this describes a remote (imported) device.
    pub fn is_remote(&self) -> bool {
        self.host.is_some()
    }

    /// Validate the bus id, optional host, and IDs.
    pub fn validate(&self) -> Result<(), UsbError> {
        validate_bus_id(&self.bus_id)?;
        if let Some(host) = &self.host {
            validate_host(host)?;
        }
        if self.vendor_id.is_some() != self.product_id.is_some() {
            return Err(UsbError::InvalidUsbIp {
                field: "vendor_id/product_id",
                reason: "vendor and product IDs must be provided together".to_owned(),
            });
        }
        if self.description.len() > 512 || self.description.chars().any(char::is_control) {
            return Err(UsbError::InvalidUsbIp {
                field: "description",
                reason: "device description must be at most 512 printable bytes".to_owned(),
            });
        }
        Ok(())
    }
}

/// One advisory pairing key fingerprint.
///
/// This is metadata only. It can be written to an external `usbipd`
/// configuration or compared by a controller; this crate never performs a
/// cryptographic handshake.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpKey {
    /// Fingerprint such as `SHA256:ab:cd:...` or a hex digest.
    pub fingerprint: String,
    /// Optional human-readable owner label.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}
impl UsbIpKey {
    /// Build and validate a fingerprint record.
    pub fn new(fingerprint: impl Into<String>) -> Result<Self, UsbError> {
        let key = Self {
            fingerprint: fingerprint.into(),
            label: None,
        };
        key.validate()?;
        Ok(key)
    }

    /// Validate the fingerprint and optional label.
    pub fn validate(&self) -> Result<(), UsbError> {
        let fingerprint = self.fingerprint.trim();
        if fingerprint.is_empty() || fingerprint.len() > 128 {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.keys.fingerprint",
                reason: "fingerprint must be 1..=128 bytes".to_owned(),
            });
        }
        if !fingerprint.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, ':' | '-' | '_' | '=')
        }) {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.keys.fingerprint",
                reason: "fingerprint must contain only letters, digits, ':', '-', '_', or '='"
                    .to_owned(),
            });
        }
        if fingerprint
            .chars()
            .filter(|character| character.is_ascii_hexdigit())
            .count()
            < 8
        {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.keys.fingerprint",
                reason: "fingerprint must contain at least eight hex digits".to_owned(),
            });
        }
        if let Some(label) = &self.label {
            if label.len() > 64 || label.chars().any(char::is_control) {
                return Err(UsbError::InvalidUsbIp {
                    field: "authorization.keys.label",
                    reason: "label must be at most 64 printable bytes".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Compare a presented fingerprint in constant time.
    ///
    /// Fingerprints are public metadata, but using the same comparison shape as
    /// tokens keeps pairing checks free of early-exit length/content leaks.
    pub fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.fingerprint.as_bytes(), candidate.as_bytes())
    }
}
///
/// Tokens are compared without an early return on a length mismatch (the
/// comparison time still depends on the longer input's length), but this is a
/// simple allow-list check, **not** TLS, password hashing, or a cryptographic
/// protocol. Configuration/debug output redacts the token value; the JSON
/// configuration still contains it so an operator can provision it.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct UsbIpToken(String);

impl UsbIpToken {
    /// Validate and wrap a bearer token.
    pub fn new(token: impl Into<String>) -> Result<Self, UsbError> {
        let token = token.into();
        Self::validate_value(&token)?;
        Ok(Self(token))
    }

    /// Validate the token shape without keeping it.
    pub fn validate_value(token: &str) -> Result<(), UsbError> {
        if token.len() < 16 || token.len() > 256 {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.allowed_tokens",
                reason: "tokens must be 16..=256 bytes".to_owned(),
            });
        }
        if !token.chars().all(|character| character.is_ascii_graphic()) {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.allowed_tokens",
                reason: "tokens must contain printable ASCII without whitespace".to_owned(),
            });
        }
        Ok(())
    }

    /// Whether `candidate` matches this token.
    pub fn matches(&self, candidate: &str) -> bool {
        constant_time_eq(self.0.as_bytes(), candidate.as_bytes())
    }
}

impl std::fmt::Debug for UsbIpToken {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("UsbIpToken(<redacted>)")
    }
}

fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    // Do not early-return on a length mismatch: fold the length difference into
    // the same accumulator so equal-prefix wrong-length inputs are not
    // distinguishable by an immediate return.
    let mut difference = (left.len() ^ right.len()) as u64;
    let longest = left.len().max(right.len());
    for index in 0..longest {
        let left_byte = left.get(index).copied().unwrap_or(0);
        let right_byte = right.get(index).copied().unwrap_or(0);
        difference |= u64::from(left_byte ^ right_byte);
    }
    difference == 0
}

/// Advisory USB/IP authorization/pairing metadata plus an enforceable token
/// allow-list.
///
/// There is no cryptographic enforcement here; see the module documentation.
/// When the controller is given an `UsbIpAuthorization`, the host allow-list
/// and token list are checked before any `usbip attach` argv is built.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpAuthorization {
    /// Hostnames or addresses permitted to import/export devices.
    #[serde(default)]
    pub allowed_hosts: Vec<String>,
    /// Advisory key fingerprints.
    #[serde(default)]
    pub allowed_keys: Vec<UsbIpKey>,
    /// Bearer tokens accepted for a remote host. Empty means no token check;
    /// if the host list is also empty, authorization fails closed.
    #[serde(default)]
    pub allowed_tokens: Vec<UsbIpToken>,
}

impl UsbIpAuthorization {
    /// Validate every allowed host, key, and token.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.allowed_hosts.len() > 64 {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.allowed_hosts",
                reason: "at most 64 allowed hosts are supported".to_owned(),
            });
        }
        if self.allowed_hosts.is_empty() && self.allowed_tokens.is_empty() {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization",
                reason: "authorization needs at least one allowed host or token".to_owned(),
            });
        }
        for host in &self.allowed_hosts {
            validate_host(host)?;
        }
        if self.allowed_keys.len() > 64 {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.allowed_keys",
                reason: "at most 64 allowed keys are supported".to_owned(),
            });
        }
        for key in &self.allowed_keys {
            key.validate()?;
        }
        if self.allowed_tokens.len() > 64 {
            return Err(UsbError::InvalidUsbIp {
                field: "authorization.allowed_tokens",
                reason: "at most 64 tokens are supported".to_owned(),
            });
        }
        let mut seen = std::collections::HashSet::new();
        for token in &self.allowed_tokens {
            UsbIpToken::validate_value(&token.0)?;
            if !seen.insert(token.0.clone()) {
                return Err(UsbError::InvalidUsbIp {
                    field: "authorization.allowed_tokens",
                    reason: "duplicate tokens are not allowed".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Whether a host appears in the allow-list (exact match).
    pub fn host_allowed(&self, host: &str) -> bool {
        self.allowed_hosts.iter().any(|allowed| allowed == host)
    }

    /// Whether a token appears in the allow-list.
    pub fn token_allowed(&self, token: &str) -> bool {
        self.allowed_tokens
            .iter()
            .any(|allowed| allowed.matches(token))
    }

    /// Whether a token is required for authorization.
    pub fn requires_token(&self) -> bool {
        !self.allowed_tokens.is_empty()
    }

    /// Enforce this authorization for a remote host and optional token.
    ///
    /// The check fails closed: an authorization with no hosts and no tokens
    /// denies everything, a non-empty host list requires an exact host match,
    /// and a non-empty token list requires a matching token. Denial messages
    /// never include the supplied token.
    pub fn authorize(&self, host: &str, token: Option<&str>) -> Result<(), UsbError> {
        self.validate()?;
        if !self.allowed_hosts.is_empty() && !self.host_allowed(host) {
            return Err(UsbError::UsbIpUnauthorized {
                host: host.to_owned(),
                reason: "host is not in the USB/IP allow-list".to_owned(),
            });
        }
        if self.requires_token() {
            match token {
                Some(token) if self.token_allowed(token) => {}
                Some(_) => {
                    return Err(UsbError::UsbIpUnauthorized {
                        host: host.to_owned(),
                        reason: "token is not in the USB/IP allow-list".to_owned(),
                    });
                }
                None => {
                    return Err(UsbError::UsbIpUnauthorized {
                        host: host.to_owned(),
                        reason: "host requires an authorization token".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Enforce server-side pairing for an inbound export/attach request.
    ///
    /// This is the peer-facing sibling of [`UsbIpAuthorization::authorize`].
    /// The host must be present when a host allow-list is configured; a token is
    /// required and compared in constant time when any token is configured; and
    /// when key fingerprints are configured the presented fingerprint must
    /// match one of them. A grant records only non-secret pairing metadata, so
    /// it is safe to expose through a status route. Denial reasons never include
    /// the supplied token or fingerprint.
    pub fn authorize_peer(
        &self,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<UsbIpPeerGrant, UsbError> {
        self.validate()?;
        if !self.allowed_hosts.is_empty() && !self.host_allowed(host) {
            return Err(UsbError::UsbIpUnauthorized {
                host: host.to_owned(),
                reason: "peer host is not in the USB/IP allow-list".to_owned(),
            });
        }
        if self.requires_token() {
            match token {
                Some(token) if self.token_allowed(token) => {}
                Some(_) => {
                    return Err(UsbError::UsbIpUnauthorized {
                        host: host.to_owned(),
                        reason: "peer token is not in the USB/IP allow-list".to_owned(),
                    });
                }
                None => {
                    return Err(UsbError::UsbIpUnauthorized {
                        host: host.to_owned(),
                        reason: "peer must present an authorization token".to_owned(),
                    });
                }
            }
        }
        let matched_key = if self.allowed_keys.is_empty() {
            None
        } else {
            let presented = key_fingerprint.ok_or_else(|| UsbError::UsbIpUnauthorized {
                host: host.to_owned(),
                reason: "peer must present a paired key fingerprint".to_owned(),
            })?;
            match self.allowed_keys.iter().find(|key| key.matches(presented)) {
                Some(key) => Some(key.clone()),
                None => {
                    return Err(UsbError::UsbIpUnauthorized {
                        host: host.to_owned(),
                        reason: "peer key fingerprint is not paired".to_owned(),
                    });
                }
            }
        };
        Ok(UsbIpPeerGrant {
            host: host.to_owned(),
            token_presented: token.is_some(),
            key: matched_key,
        })
    }
}

/// Non-secret metadata for an accepted USB/IP peer.
///
/// The raw token and presented fingerprint are deliberately not retained.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpPeerGrant {
    /// Authorized peer host.
    pub host: String,
    /// Whether a token was presented (never the value).
    pub token_presented: bool,
    /// Matched pairing key, when a key allow-list was configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub key: Option<UsbIpKey>,
}

/// Node-side USB/IP configuration.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpConfig {
    /// Whether USB/IP operations are permitted.
    pub enabled: bool,
    /// Default remote/bind host (loopback unless configured otherwise).
    pub host: String,
    /// Default USB/IP TCP port.
    pub port: u16,
    /// Optional host/token authorization and advisory pairing metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<UsbIpAuthorization>,
    /// Whether the USB/IP transport must be TLS-protected by this crate.
    ///
    /// `Disabled` (the default) leaves TLS to an external terminator;
    /// `Required` fails validation with `usbip-tls-unsupported` because this
    /// crate delegates the wire protocol to external `usbip`/`usbipd`.
    #[serde(default, skip_serializing_if = "UsbIpTlsPolicy::is_disabled")]
    pub tls: UsbIpTlsPolicy,
    /// Devices named by configuration (local exports or remote devices).
    #[serde(default)]
    pub devices: Vec<UsbIpDevice>,
}

impl UsbIpTlsPolicy {
    /// Whether this is the default `Disabled` policy.
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled)
    }
}

impl Default for UsbIpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            host: "127.0.0.1".to_owned(),
            port: DEFAULT_USBIP_PORT,
            authorization: None,
            tls: UsbIpTlsPolicy::Disabled,
            devices: Vec::new(),
        }
    }
}

impl UsbIpConfig {
    /// A disabled configuration with the default host/port.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// An enabled USB/IP server configuration.
    pub fn server(host: impl Into<String>, port: u16) -> Self {
        Self {
            enabled: true,
            host: host.into(),
            port,
            ..Self::default()
        }
    }

    /// An enabled USB/IP client configuration for a remote host.
    pub fn client(host: impl Into<String>, port: u16) -> Self {
        Self {
            enabled: true,
            host: host.into(),
            port,
            ..Self::default()
        }
    }

    /// Attach host/token authorization metadata.
    pub fn with_authorization(mut self, authorization: UsbIpAuthorization) -> Self {
        self.authorization = Some(authorization);
        self
    }

    /// Set the USB/IP TLS policy.
    pub fn with_tls(mut self, tls: UsbIpTlsPolicy) -> Self {
        self.tls = tls;
        self
    }

    /// Capability status for USB/IP TLS.
    pub fn tls_status(&self) -> UsbIpTlsStatus {
        UsbIpTlsStatus::unsupported(self.tls)
    }

    /// Add a configured device.
    pub fn with_device(mut self, device: UsbIpDevice) -> Self {
        self.devices.push(device);
        self
    }

    /// Validate host, port, pairing metadata, TLS policy, and devices.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.host.is_empty() {
            if self.enabled {
                return Err(UsbError::InvalidUsbIp {
                    field: "host",
                    reason: "an enabled USB/IP configuration needs a host".to_owned(),
                });
            }
        } else {
            validate_host(&self.host)?;
        }
        validate_port_number(self.port)?;
        if matches!(self.tls, UsbIpTlsPolicy::Required) {
            return Err(UsbError::UsbIpTlsUnsupported {
                reason: self.tls_status().reason,
            });
        }
        if let Some(authorization) = &self.authorization {
            authorization.validate()?;
        }
        if self.devices.len() > MAX_USBIP_DEVICES {
            return Err(UsbError::InvalidUsbIp {
                field: "devices",
                reason: format!("at most {MAX_USBIP_DEVICES} devices are supported"),
            });
        }
        let mut seen = std::collections::BTreeSet::new();
        for device in &self.devices {
            device.validate()?;
            let key = (
                device.host.clone().unwrap_or_default(),
                device.bus_id.clone(),
            );
            if !seen.insert(key) {
                return Err(UsbError::InvalidUsbIp {
                    field: "devices",
                    reason: format!(
                        "duplicate device {:?} on host {:?}",
                        device.bus_id,
                        device.host.as_deref().unwrap_or("<local>")
                    ),
                });
            }
        }
        Ok(())
    }

    /// Whether the configuration is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Empty state matching this configuration.
    pub fn initial_state(&self) -> UsbIpState {
        UsbIpState {
            enabled: self.enabled,
            host: self.host.clone(),
            port: self.port,
            exported: Vec::new(),
            attached: Vec::new(),
        }
    }
}

/// One currently imported USB/IP device.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpAttachment {
    /// Remote host the device was imported from.
    pub host: String,
    /// Remote bus id.
    pub bus_id: String,
    /// Local `vhci` port assigned by the kernel, when `usbip port` reports it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
    /// Vendor ID when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vendor_id: Option<u16>,
    /// Product ID when reported.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub product_id: Option<u16>,
}

impl UsbIpAttachment {
    /// Build an attachment record from a remote device.
    pub fn from_device(host: impl Into<String>, device: &UsbIpDevice) -> Self {
        Self {
            host: host.into(),
            bus_id: device.bus_id.clone(),
            port: None,
            vendor_id: device.vendor_id,
            product_id: device.product_id,
        }
    }

    /// Validate host and bus id.
    pub fn validate(&self) -> Result<(), UsbError> {
        validate_host(&self.host)?;
        validate_bus_id(&self.bus_id)?;
        if let Some(port) = self.port {
            validate_port_number(port)?;
        }
        Ok(())
    }
}

/// Snapshot of USB/IP server/client state.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpState {
    /// Whether USB/IP is enabled.
    pub enabled: bool,
    /// Configured server/client host.
    pub host: String,
    /// Configured TCP port.
    pub port: u16,
    /// Devices currently exported or available for export.
    #[serde(default)]
    pub exported: Vec<UsbIpDevice>,
    /// Devices currently imported into this host.
    #[serde(default)]
    pub attached: Vec<UsbIpAttachment>,
}

impl Default for UsbIpState {
    fn default() -> Self {
        Self {
            enabled: false,
            host: String::new(),
            port: DEFAULT_USBIP_PORT,
            exported: Vec::new(),
            attached: Vec::new(),
        }
    }
}

impl UsbIpState {
    /// Disabled state with no devices.
    pub fn disabled() -> Self {
        Self::default()
    }

    /// Whether USB/IP is enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Record an attachment, replacing an entry with the same host/bus id.
    pub fn record_attachment(&mut self, attachment: UsbIpAttachment) {
        self.attached.retain(|existing| {
            !(existing.host == attachment.host && existing.bus_id == attachment.bus_id)
        });
        self.attached.push(attachment);
    }

    /// Remove an attachment by local port.
    ///
    /// If the local port was not learned yet (`port: None`, for example after
    /// a successful attach but before a `usbip port` status refresh), the
    /// first unported attachment is removed instead.
    pub fn record_detach(&mut self, port: u16) -> Option<UsbIpAttachment> {
        let index = self
            .attached
            .iter()
            .position(|attachment| attachment.port == Some(port))
            .or_else(|| {
                self.attached
                    .iter()
                    .position(|attachment| attachment.port.is_none())
            })?;
        Some(self.attached.remove(index))
    }

    /// Whether a remote bus id is currently attached.
    pub fn is_attached(&self, bus_id: &str) -> bool {
        self.attached
            .iter()
            .any(|attachment| attachment.bus_id == bus_id)
    }
}

/// A USB/IP export/attach/detach transition.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbIpHotplugKind {
    /// A local device started being exported.
    Export,
    /// A local device stopped being exported.
    Unexport,
    /// A remote device was attached.
    Attach,
    /// An imported device was detached.
    Detach,
}

impl UsbIpHotplugKind {
    /// Stable lower-case event name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Export => "export",
            Self::Unexport => "unexport",
            Self::Attach => "attach",
            Self::Detach => "detach",
        }
    }
}

/// One bounded hotplug event, suitable for a future node route or SSE stream.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbIpHotplugEvent {
    /// Monotonic sequence number within one controller instance.
    pub sequence: u64,
    /// Transition kind.
    pub kind: UsbIpHotplugKind,
    /// Local or remote bus id.
    pub bus_id: String,
    /// Peer host for attach/detach events.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub host: Option<String>,
    /// Local `vhci` port for detach events, when known.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub port: Option<u16>,
}

/// A fixed-capacity, drop-oldest USB/IP hotplug event log.
///
/// Capacity is clamped to `1..=MAX_USBIP_HOTPLUG_EVENTS`; the oldest event is
/// discarded once full, so a future node route can poll or drain a bounded
/// stream without unbounded memory growth.
#[derive(Clone, Debug)]
pub struct UsbIpHotplugLog {
    next_sequence: u64,
    capacity: usize,
    events: VecDeque<UsbIpHotplugEvent>,
}

impl Default for UsbIpHotplugLog {
    fn default() -> Self {
        Self::new(MAX_USBIP_HOTPLUG_EVENTS)
    }
}

impl UsbIpHotplugLog {
    /// Create a log with the given capacity (clamped to the supported range).
    pub fn new(capacity: usize) -> Self {
        Self {
            next_sequence: 1,
            capacity: capacity.clamp(1, MAX_USBIP_HOTPLUG_EVENTS),
            events: VecDeque::new(),
        }
    }

    /// Retained-event capacity.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of retained events.
    pub fn len(&self) -> usize {
        self.events.len()
    }

    /// Whether no events are retained.
    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }

    /// Record a transition, returning the stored event (with its sequence).
    pub fn record(
        &mut self,
        kind: UsbIpHotplugKind,
        bus_id: impl Into<String>,
        host: Option<String>,
        port: Option<u16>,
    ) -> UsbIpHotplugEvent {
        let event = UsbIpHotplugEvent {
            sequence: self.next_sequence,
            kind,
            bus_id: bus_id.into(),
            host,
            port,
        };
        self.next_sequence = self.next_sequence.saturating_add(1);
        if self.events.len() == self.capacity {
            self.events.pop_front();
        }
        self.events.push_back(event.clone());
        event
    }

    /// Retained events, oldest first.
    pub fn events(&self) -> Vec<UsbIpHotplugEvent> {
        self.events.iter().cloned().collect()
    }

    /// The most recent event.
    pub fn latest(&self) -> Option<&UsbIpHotplugEvent> {
        self.events.back()
    }

    /// Remove and return all retained events.
    pub fn drain(&mut self) -> Vec<UsbIpHotplugEvent> {
        self.events.drain(..).collect()
    }
}

/// List, export, import, detach, and inspect USB/IP devices.
pub trait UsbIpController: Send + Sync {
    /// List local exportable devices (`usbip list -l`).
    fn list(&self) -> Result<Vec<UsbIpDevice>, UsbError>;

    /// Export a local device (`usbip bind -b <busid>`).
    fn export(&self, bus_id: &str) -> Result<(), UsbError>;

    /// Stop exporting a local device (`usbip unbind -b <busid>`).
    fn unexport(&self, bus_id: &str) -> Result<(), UsbError>;

    /// Import a remote device (`usbip attach -r <host> -b <busid>`).
    fn attach(&self, host: &str, bus_id: &str) -> Result<UsbIpAttachment, UsbError>;

    /// Detach a local import port (`usbip detach -p <port>`).
    fn detach(&self, port: u16) -> Result<(), UsbError>;

    /// Current exported/imported state, refreshed from `usbip port` when
    /// enabled. Implementation failures are typed; state is never fabricated.
    fn status(&self) -> Result<UsbIpState, UsbError>;
}

/// A USB/IP controller that runs the `usbip` CLI through an injected runner.
#[derive(Debug)]
pub struct SubprocessUsbIpController<R = SystemCommandRunner> {
    runner: R,
    program: String,
    config: UsbIpConfig,
    state: Mutex<UsbIpState>,
    hotplug: Mutex<UsbIpHotplugLog>,
}

impl SubprocessUsbIpController<SystemCommandRunner> {
    /// Use `usbip` from `PATH`.
    pub fn system(config: UsbIpConfig) -> Self {
        Self::new(SystemCommandRunner, "usbip", config)
    }
}

impl<R: CommandRunner> SubprocessUsbIpController<R> {
    /// Build a controller with an injected runner, program, and configuration.
    pub fn new(runner: R, program: impl Into<String>, config: UsbIpConfig) -> Self {
        let state = config.initial_state();
        Self {
            runner,
            program: program.into(),
            config,
            state: Mutex::new(state),
            hotplug: Mutex::new(UsbIpHotplugLog::default()),
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

    /// The validated configuration.
    pub fn config(&self) -> &UsbIpConfig {
        &self.config
    }

    /// Locally recorded state without running `usbip`.
    ///
    /// This is the last state observed by a successful operation. Use
    /// [`UsbIpController::status`] for a refreshed snapshot.
    pub fn known_state(&self) -> UsbIpState {
        self.lock_state().clone()
    }

    /// Retained hotplug events, oldest first (does not drain).
    pub fn hotplug_events(&self) -> Vec<UsbIpHotplugEvent> {
        self.lock_hotplug().events()
    }

    /// Remove and return retained hotplug events.
    pub fn take_hotplug_events(&self) -> Vec<UsbIpHotplugEvent> {
        self.lock_hotplug().drain()
    }

    /// Hotplug log geometry.
    pub fn hotplug_capacity(&self) -> usize {
        self.lock_hotplug().capacity()
    }

    fn lock_hotplug(&self) -> MutexGuard<'_, UsbIpHotplugLog> {
        self.hotplug
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Parse `usbip list -l` output without running anything.
    pub fn parse_list(output: &str) -> Vec<UsbIpDevice> {
        parse_usbip_list(output)
    }

    /// Parse `usbip port` output without running anything.
    pub fn parse_ports(output: &str) -> Vec<UsbIpAttachment> {
        parse_usbip_ports(output)
    }

    fn lock_state(&self) -> MutexGuard<'_, UsbIpState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn ensure_enabled(&self) -> Result<(), UsbError> {
        self.config.validate()?;
        if self.config.enabled {
            Ok(())
        } else {
            Err(UsbError::UsbIpDisabled)
        }
    }

    /// Enforce the configured host allow-list/token before an attach.
    ///
    /// A configuration without `authorization` keeps the historical behaviour
    /// (no controller-level check). A present authorization fails closed and
    /// must name the host or accept a token.
    pub fn authorize_host(&self, host: &str, token: Option<&str>) -> Result<(), UsbError> {
        validate_host(host)?;
        match &self.config.authorization {
            Some(authorization) => authorization.authorize(host, token),
            None => Ok(()),
        }
    }

    /// Enforce server-side pairing for an inbound export/attach request.
    ///
    /// The controller does not serve the USB/IP wire protocol, so this is the
    /// reusable gate a future node attach/export route (or a small proxy in
    /// front of `usbipd`) must call before exporting or permitting a peer. A
    /// configuration without authorization keeps the historical behaviour.
    pub fn authorize_peer(
        &self,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<UsbIpPeerGrant, UsbError> {
        self.ensure_enabled()?;
        validate_host(host)?;
        let key_required = self
            .config
            .authorization
            .as_ref()
            .is_some_and(|authorization| !authorization.allowed_keys.is_empty());
        if key_required {
            if let Some(fingerprint) = key_fingerprint {
                // Validate shape before comparing so malformed peer metadata is
                // still a typed authorization failure, not a panic or skip.
                let key = UsbIpKey {
                    fingerprint: fingerprint.to_owned(),
                    label: None,
                };
                if key.validate().is_err() {
                    return Err(UsbError::UsbIpUnauthorized {
                        host: host.to_owned(),
                        reason: "peer key fingerprint is malformed".to_owned(),
                    });
                }
            }
        }
        match &self.config.authorization {
            Some(authorization) => authorization.authorize_peer(host, token, key_fingerprint),
            None => Ok(UsbIpPeerGrant {
                host: host.to_owned(),
                token_presented: token.is_some(),
                key: None,
            }),
        }
    }

    /// Export a local device after validating the exporting peer.
    ///
    /// This is the server-side pairing entry point: `host`, `token`, and
    /// `key_fingerprint` describe the peer that requested the export and are
    /// checked before `usbip bind` argv is built. [`UsbIpController::export`]
    /// remains the local-administration path with no peer check.
    pub fn export_with_token(
        &self,
        bus_id: &str,
        host: &str,
        token: Option<&str>,
        key_fingerprint: Option<&str>,
    ) -> Result<(), UsbError> {
        self.ensure_enabled()?;
        validate_bus_id(bus_id)?;
        let grant = self.authorize_peer(host, token, key_fingerprint)?;
        let _ = grant;
        self.run(&["bind", "-b", bus_id])?;
        let mut state = self.lock_state();
        if !state.exported.iter().any(|device| device.bus_id == bus_id) {
            state.exported.push(UsbIpDevice::local(bus_id));
        }
        drop(state);
        self.lock_hotplug().record(
            UsbIpHotplugKind::Export,
            bus_id,
            Some(host.to_owned()),
            None,
        );
        Ok(())
    }

    /// Attach a remote device after checking the authorization allow-list.
    ///
    /// This is the token-aware entry point; [`UsbIpController::attach`] calls
    /// it with no token and therefore fails when the configuration requires
    /// one.
    pub fn attach_with_token(
        &self,
        host: &str,
        bus_id: &str,
        token: Option<&str>,
    ) -> Result<UsbIpAttachment, UsbError> {
        self.ensure_enabled()?;
        validate_host(host)?;
        validate_bus_id(bus_id)?;
        self.authorize_host(host, token)?;
        self.run(&["attach", "-r", host, "-b", bus_id])?;
        let attachment = UsbIpAttachment {
            host: host.to_owned(),
            bus_id: bus_id.to_owned(),
            port: None,
            vendor_id: None,
            product_id: None,
        };
        self.lock_state().record_attachment(attachment.clone());
        self.lock_hotplug().record(
            UsbIpHotplugKind::Attach,
            bus_id,
            Some(host.to_owned()),
            None,
        );
        Ok(attachment)
    }

    fn run(&self, args: &[&str]) -> Result<CommandOutput, UsbError> {
        let command = CommandSpec::new(
            self.program.clone(),
            args.iter().map(|argument| (*argument).to_owned()),
        );
        self.runner.run(&command)
    }
}

impl<R: CommandRunner> UsbIpController for SubprocessUsbIpController<R> {
    fn list(&self) -> Result<Vec<UsbIpDevice>, UsbError> {
        self.config.validate()?;
        let output = self.run(&["list", "-l"])?;
        let devices = parse_usbip_list(&output.stdout);
        self.lock_state().exported = devices.clone();
        Ok(devices)
    }

    fn export(&self, bus_id: &str) -> Result<(), UsbError> {
        self.ensure_enabled()?;
        validate_bus_id(bus_id)?;
        self.run(&["bind", "-b", bus_id])?;
        let mut state = self.lock_state();
        if !state.exported.iter().any(|device| device.bus_id == bus_id) {
            state.exported.push(UsbIpDevice::local(bus_id));
        }
        drop(state);
        self.lock_hotplug()
            .record(UsbIpHotplugKind::Export, bus_id, None, None);
        Ok(())
    }

    fn unexport(&self, bus_id: &str) -> Result<(), UsbError> {
        self.ensure_enabled()?;
        validate_bus_id(bus_id)?;
        self.run(&["unbind", "-b", bus_id])?;
        self.lock_state()
            .exported
            .retain(|device| device.bus_id != bus_id);
        self.lock_hotplug()
            .record(UsbIpHotplugKind::Unexport, bus_id, None, None);
        Ok(())
    }

    fn attach(&self, host: &str, bus_id: &str) -> Result<UsbIpAttachment, UsbError> {
        self.attach_with_token(host, bus_id, None)
    }

    fn detach(&self, port: u16) -> Result<(), UsbError> {
        self.ensure_enabled()?;
        validate_port_number(port)?;
        let port_text = port.to_string();
        self.run(&["detach", "-p", &port_text])?;
        let removed = self.lock_state().record_detach(port);
        if let Some(attachment) = removed {
            self.lock_hotplug().record(
                UsbIpHotplugKind::Detach,
                attachment.bus_id,
                Some(attachment.host),
                Some(port),
            );
        }
        Ok(())
    }

    fn status(&self) -> Result<UsbIpState, UsbError> {
        if !self.config.enabled {
            return Ok(self.config.initial_state());
        }
        self.config.validate()?;
        let output = self.run(&["port"])?;
        let attached = parse_usbip_ports(&output.stdout);
        let mut state = self.lock_state().clone();
        state.enabled = true;
        state.host = self.config.host.clone();
        state.port = self.config.port;
        state.attached = attached;
        *self.lock_state() = state.clone();
        Ok(state)
    }
}

/// Validate a USB/IP bus id (`1-1`, `1-2.3`, `10-1:1.0`).
pub(crate) fn validate_bus_id(bus_id: &str) -> Result<(), UsbError> {
    let valid = !bus_id.is_empty()
        && bus_id.len() <= 32
        && !bus_id.starts_with('-')
        && bus_id.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '.' | ':' | '_')
        })
        && bus_id.chars().any(|character| character.is_ascii_digit());
    if valid {
        Ok(())
    } else {
        Err(UsbError::InvalidUsbIp {
            field: "bus_id",
            reason: format!(
                "{bus_id:?} must be a non-empty USB/IP bus id (letters, digits, '-', '.', ':', '_')"
            ),
        })
    }
}

/// Validate a USB/IP host or address without whitespace/option prefixes.
pub(crate) fn validate_host(host: &str) -> Result<(), UsbError> {
    let valid = !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host.chars().all(|character| {
            character.is_ascii_alphanumeric()
                || matches!(character, '.' | '-' | ':' | '_' | '[' | ']' | '%')
        });
    if valid {
        Ok(())
    } else {
        Err(UsbError::InvalidUsbIp {
            field: "host",
            reason: format!(
                "{host:?} must be a hostname or address without whitespace or option prefixes"
            ),
        })
    }
}

/// Validate a numeric USB/IP port.
pub(crate) fn validate_port_number(port: u16) -> Result<(), UsbError> {
    if port > 0 {
        Ok(())
    } else {
        Err(UsbError::InvalidUsbIp {
            field: "port",
            reason: "USB/IP port must be in 1..=65535".to_owned(),
        })
    }
}

/// Validate a textual USB/IP port (`usbip detach -p`).
pub(crate) fn validate_port_text(port: &str) -> Result<(), UsbError> {
    let parsed = port.parse::<u16>().map_err(|_| UsbError::InvalidUsbIp {
        field: "port",
        reason: format!("{port:?} must be a USB/IP port in 1..=65535"),
    })?;
    validate_port_number(parsed)
}

pub(crate) fn parse_usbip_list(output: &str) -> Vec<UsbIpDevice> {
    let lines: Vec<&str> = output.lines().collect();
    let mut devices = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        let Some(rest) = trimmed.strip_prefix("- busid ") else {
            index += 1;
            continue;
        };

        let mut parts = rest.split_whitespace();
        let Some(bus_id) = parts.next() else {
            index += 1;
            continue;
        };
        let ids = parts
            .find(|token| token.starts_with('(') && token.ends_with(')'))
            .and_then(parse_ids);
        let (vendor_id, product_id) = match ids {
            Some((vendor, product)) => (Some(vendor), Some(product)),
            None => (None, None),
        };

        let mut description = String::new();
        if index + 1 < lines.len() && !lines[index + 1].trim().starts_with("- busid ") {
            description = lines[index + 1].trim().to_owned();
            index += 1;
        }

        devices.push(UsbIpDevice {
            bus_id: bus_id.to_owned(),
            vendor_id,
            product_id,
            description,
            host: None,
        });
        index += 1;
    }
    devices
}

fn parse_usbip_ports(output: &str) -> Vec<UsbIpAttachment> {
    let lines: Vec<&str> = output.lines().collect();
    let mut attachments = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let trimmed = lines[index].trim();
        let Some(port) = parse_port_header(trimmed) else {
            index += 1;
            continue;
        };

        let mut attachment: Option<UsbIpAttachment> = None;
        let mut ids: Option<(u16, u16)> = None;
        index += 1;
        while index < lines.len() {
            let line = lines[index].trim();
            if parse_port_header(line).is_some() {
                break;
            }
            if ids.is_none() {
                ids = line.split_whitespace().find_map(parse_ids);
            }
            if attachment.is_none() {
                attachment = parse_usbip_url_line(line).map(|(host, bus_id)| UsbIpAttachment {
                    host,
                    bus_id,
                    port: Some(port),
                    vendor_id: None,
                    product_id: None,
                });
            }
            index += 1;
        }

        if let Some(mut attachment) = attachment {
            if let Some((vendor, product)) = ids {
                attachment.vendor_id = Some(vendor);
                attachment.product_id = Some(product);
            }
            attachments.push(attachment);
        }
    }
    attachments
}

fn parse_port_header(line: &str) -> Option<u16> {
    let rest = line.strip_prefix("Port ")?;
    let (number, _) = rest.split_once(':')?;
    number.trim().parse::<u16>().ok()
}

fn parse_usbip_url_line(line: &str) -> Option<(String, String)> {
    let after_scheme = line.split_once("usbip://")?.1;
    let url = after_scheme.split_whitespace().next()?;
    let (authority, bus_id) = url.split_once('/')?;
    let (host, port) = if let Some(rest) = authority.strip_prefix('[') {
        let (host, remainder) = rest.split_once(']')?;
        let port = remainder.strip_prefix(':')?;
        (host.to_owned(), port)
    } else {
        let (host, port) = authority.rsplit_once(':')?;
        (host.to_owned(), port)
    };
    validate_host(&host).ok()?;
    validate_bus_id(bus_id).ok()?;
    port.parse::<u16>().ok().filter(|port| *port > 0)?;
    Some((host, bus_id.to_owned()))
}

fn parse_ids(token: &str) -> Option<(u16, u16)> {
    let inner = token.trim_start_matches('(').trim_end_matches(')');
    let (vendor, product) = inner.split_once(':')?;
    Some((
        u16::from_str_radix(vendor, 16).ok()?,
        u16::from_str_radix(product, 16).ok()?,
    ))
}
