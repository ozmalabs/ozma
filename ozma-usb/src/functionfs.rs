// SPDX-License-Identifier: AGPL-3.0-only
//! FunctionFS vendor-function model.
//!
//! A `functions/ffs.<instance>` ConfigFS directory is only an anchor. Unlike
//! HID, mass storage, or UAC2, the kernel does not take the descriptors from
//! ConfigFS: a userspace daemon must mount FunctionFS, write the device,
//! configuration, and string descriptors to `ep0`, and then service the
//! endpoints. This module models that contract but never mounts FunctionFS,
//! opens an endpoint, spawns a daemon, or claims a daemon is ready without a
//! filesystem marker.

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::is_safe_identifier;
use crate::error::UsbError;

/// Maximum number of non-control endpoints a FunctionFS function may declare.
pub const MAX_FUNCTIONFS_ENDPOINTS: usize = 15;

/// Maximum size of the generated FunctionFS descriptor set, in bytes.
pub const MAX_FUNCTIONFS_DESCRIPTOR_BYTES: usize = 4096;

/// Maximum length of one USB string descriptor value, in bytes.
pub const MAX_FUNCTIONFS_STRING_BYTES: usize = 126;

/// USB transfer type for one FunctionFS endpoint.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionFsTransfer {
    /// Bulk endpoint (`bmAttributes = 2`).
    Bulk,
    /// Interrupt endpoint (`bmAttributes = 3`).
    Interrupt,
    /// Isochronous endpoint (`bmAttributes = 1`).
    Isochronous,
}

impl FunctionFsTransfer {
    /// Low two bits of the endpoint descriptor `bmAttributes` field.
    pub fn bm_attributes(self) -> u8 {
        match self {
            Self::Isochronous => 1,
            Self::Bulk => 2,
            Self::Interrupt => 3,
        }
    }

    /// Stable name used by status and errors.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Bulk => "bulk",
            Self::Interrupt => "interrupt",
            Self::Isochronous => "isochronous",
        }
    }
}

/// One non-control endpoint declared by a FunctionFS daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsEndpoint {
    /// Full USB endpoint address, including the direction bit
    /// (`0x01..=0x0f` OUT, `0x81..=0x8f` IN).
    pub address: u8,
    /// Transfer type.
    pub transfer: FunctionFsTransfer,
    /// Maximum packet size in bytes.
    pub max_packet_size: u16,
    /// Polling/service interval (`bInterval`).
    pub interval: u8,
}

impl FunctionFsEndpoint {
    /// A generic endpoint descriptor.
    pub fn new(
        address: u8,
        transfer: FunctionFsTransfer,
        max_packet_size: u16,
        interval: u8,
    ) -> Self {
        Self {
            address,
            transfer,
            max_packet_size,
            interval,
        }
    }

    /// A bulk OUT endpoint (`address` without the direction bit).
    pub fn bulk_out(number: u8, max_packet_size: u16) -> Self {
        Self {
            address: number & 0x0f,
            transfer: FunctionFsTransfer::Bulk,
            max_packet_size,
            interval: 0,
        }
    }

    /// A bulk IN endpoint (`address` with the direction bit).
    pub fn bulk_in(number: u8, max_packet_size: u16) -> Self {
        Self {
            address: 0x80 | (number & 0x0f),
            transfer: FunctionFsTransfer::Bulk,
            max_packet_size,
            interval: 0,
        }
    }

    /// An interrupt OUT endpoint.
    pub fn interrupt_out(number: u8, max_packet_size: u16, interval: u8) -> Self {
        Self {
            address: number & 0x0f,
            transfer: FunctionFsTransfer::Interrupt,
            max_packet_size,
            interval,
        }
    }

    /// An interrupt IN endpoint.
    pub fn interrupt_in(number: u8, max_packet_size: u16, interval: u8) -> Self {
        Self {
            address: 0x80 | (number & 0x0f),
            transfer: FunctionFsTransfer::Interrupt,
            max_packet_size,
            interval,
        }
    }

    /// An isochronous OUT endpoint.
    pub fn isochronous_out(number: u8, max_packet_size: u16, interval: u8) -> Self {
        Self {
            address: number & 0x0f,
            transfer: FunctionFsTransfer::Isochronous,
            max_packet_size,
            interval,
        }
    }

    /// An isochronous IN endpoint.
    pub fn isochronous_in(number: u8, max_packet_size: u16, interval: u8) -> Self {
        Self {
            address: 0x80 | (number & 0x0f),
            transfer: FunctionFsTransfer::Isochronous,
            max_packet_size,
            interval,
        }
    }

    /// Endpoint number (`1..=15`).
    pub fn number(&self) -> u8 {
        self.address & 0x0f
    }

    /// Whether the endpoint is IN (device-to-host).
    pub fn is_in(&self) -> bool {
        self.address & 0x80 != 0
    }

    /// Human-readable direction.
    pub fn direction(&self) -> &'static str {
        if self.is_in() {
            "in"
        } else {
            "out"
        }
    }

    /// Validate the endpoint descriptor and its bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.address & 0x70 != 0 || self.number() == 0 {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.endpoint.address",
                reason: format!(
                    "endpoint address 0x{:02x} must be 0x01..=0x0f or 0x81..=0x8f",
                    self.address
                ),
            });
        }
        let max = match self.transfer {
            FunctionFsTransfer::Bulk => 512,
            FunctionFsTransfer::Interrupt | FunctionFsTransfer::Isochronous => 1024,
        };
        if self.max_packet_size == 0 || self.max_packet_size > max {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.endpoint.max_packet_size",
                reason: format!(
                    "{} endpoint 0x{:02x} max packet size must be 1..={max} bytes",
                    self.transfer.as_str(),
                    self.address
                ),
            });
        }
        match self.transfer {
            FunctionFsTransfer::Bulk => {
                if self.interval != 0 {
                    return Err(UsbError::InvalidField {
                        field: "function.functionfs.endpoint.interval",
                        reason: "bulk endpoints use bInterval 0".to_owned(),
                    });
                }
            }
            FunctionFsTransfer::Interrupt | FunctionFsTransfer::Isochronous => {
                if !(1..=16).contains(&self.interval) {
                    return Err(UsbError::InvalidField {
                        field: "function.functionfs.endpoint.interval",
                        reason: format!(
                            "{} endpoint 0x{:02x} interval must be 1..=16",
                            self.transfer.as_str(),
                            self.address
                        ),
                    });
                }
            }
        }
        Ok(())
    }

    /// Seven-byte standard endpoint descriptor.
    pub fn descriptor(&self) -> [u8; 7] {
        let [low, high] = self.max_packet_size.to_le_bytes();
        [
            7,
            0x05,
            self.address,
            self.transfer.bm_attributes(),
            low,
            high,
            self.interval,
        ]
    }
}

impl Default for FunctionFsEndpoint {
    fn default() -> Self {
        Self::bulk_in(1, 512)
    }
}

/// String descriptors supplied by the FunctionFS daemon.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsStrings {
    /// Manufacturer string (index 1).
    pub manufacturer: String,
    /// Product string (index 2).
    pub product: String,
    /// Serial-number string (index 3); may be empty.
    pub serial_number: String,
    /// Configuration string (index 4).
    pub configuration: String,
}

impl Default for FunctionFsStrings {
    fn default() -> Self {
        Self {
            manufacturer: "Ozma Labs".to_owned(),
            product: "Ozma FunctionFS".to_owned(),
            serial_number: "OZMA-FFS".to_owned(),
            configuration: "Ozma FunctionFS Configuration".to_owned(),
        }
    }
}

impl FunctionFsStrings {
    /// Strings for a named instance.
    pub fn for_instance(instance: &str) -> Self {
        Self {
            product: format!("Ozma FunctionFS {instance}"),
            serial_number: format!("OZMA-{instance}"),
            ..Self::default()
        }
    }

    fn validate(&self) -> Result<(), UsbError> {
        validate_string("manufacturer", &self.manufacturer, false)?;
        validate_string("product", &self.product, false)?;
        validate_string("serial_number", &self.serial_number, true)?;
        validate_string("configuration", &self.configuration, false)
    }

    /// USB string descriptors in index order, including the language
    /// descriptor at index 0.
    pub fn descriptors(&self) -> Vec<Vec<u8>> {
        vec![
            string_descriptor(&[0x09, 0x04]),
            string_descriptor(&utf16_units(&self.manufacturer)),
            string_descriptor(&utf16_units(&self.product)),
            string_descriptor(&utf16_units(&self.serial_number)),
            string_descriptor(&utf16_units(&self.configuration)),
        ]
    }
}

/// Userspace daemon expected to service a FunctionFS function.
///
/// This is metadata only. `ozma-usb` never spawns `program`, never passes
/// `args`, and never treats a missing program or readiness marker as success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsDaemonSpec {
    /// Absolute path to the daemon executable.
    pub program: PathBuf,
    /// Fixed argv elements (never interpolated through a shell).
    pub args: Vec<String>,
    /// Optional absolute path whose existence means the daemon has written the
    /// FunctionFS descriptors and opened its endpoints.
    pub ready_marker: Option<PathBuf>,
}

impl FunctionFsDaemonSpec {
    /// A daemon specification with no args and no readiness marker.
    pub fn new(program: impl Into<PathBuf>) -> Self {
        Self {
            program: program.into(),
            args: Vec::new(),
            ready_marker: None,
        }
    }

    /// Append one fixed argv element.
    pub fn with_arg(mut self, arg: impl Into<String>) -> Self {
        self.args.push(arg.into());
        self
    }

    /// Require a readiness marker before the function is considered ready.
    pub fn with_ready_marker(mut self, marker: impl Into<PathBuf>) -> Self {
        self.ready_marker = Some(marker.into());
        self
    }

    fn validate(&self) -> Result<(), UsbError> {
        if !self.program.is_absolute() {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.daemon.program",
                reason: "daemon path must be absolute".to_owned(),
            });
        }
        if self.args.len() > 16 {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.daemon.args",
                reason: "at most 16 daemon arguments are supported".to_owned(),
            });
        }
        for arg in &self.args {
            if arg.len() > 256 || arg.chars().any(char::is_control) {
                return Err(UsbError::InvalidField {
                    field: "function.functionfs.daemon.args",
                    reason: "daemon arguments must be at most 256 bytes without control characters"
                        .to_owned(),
                });
            }
        }
        if let Some(marker) = &self.ready_marker {
            if !marker.is_absolute() {
                return Err(UsbError::InvalidField {
                    field: "function.functionfs.daemon.ready_marker",
                    reason: "readiness marker path must be absolute".to_owned(),
                });
            }
        }
        Ok(())
    }

    /// Observe the daemon without executing it.
    ///
    /// The daemon is only `Ready` when the program exists as a regular file
    /// *and* a configured readiness marker exists. No marker means
    /// `DaemonNotReady`, not success.
    pub fn readiness(&self) -> FunctionFsReadiness {
        if !self.program.is_file() {
            return FunctionFsReadiness::DaemonMissing;
        }
        match &self.ready_marker {
            Some(marker) if marker.is_file() => FunctionFsReadiness::Ready,
            _ => FunctionFsReadiness::DaemonNotReady,
        }
    }
}

/// Explicit FunctionFS readiness state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FunctionFsReadiness {
    /// No daemon path was configured on the function model.
    DaemonNotConfigured,
    /// A daemon path was configured but the program is absent.
    DaemonMissing,
    /// The daemon program exists but it has not signalled readiness.
    DaemonNotReady,
    /// The daemon program and readiness marker both exist.
    Ready,
    /// A ConfigFS status reader has no daemon metadata for this function.
    Unknown,
}

impl FunctionFsReadiness {
    /// Stable lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::DaemonNotConfigured => "daemon_not_configured",
            Self::DaemonMissing => "daemon_missing",
            Self::DaemonNotReady => "daemon_not_ready",
            Self::Ready => "ready",
            Self::Unknown => "unknown",
        }
    }

    /// Whether a daemon has explicitly reported readiness.
    pub fn is_ready(self) -> bool {
        matches!(self, Self::Ready)
    }
}

/// A userspace FunctionFS descriptor/endpoint contract.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsConfig {
    /// Instance name (the part after `ffs.`).
    pub instance: String,
    /// Vendor ID written into the FunctionFS device descriptor.
    pub vendor_id: u16,
    /// Product ID written into the FunctionFS device descriptor.
    pub product_id: u16,
    /// Device release number (`bcdDevice`).
    pub bcd_device: u16,
    /// Interface/device class.
    pub class: u8,
    /// Interface/device subclass.
    pub subclass: u8,
    /// Interface/device protocol.
    pub protocol: u8,
    /// Declared non-control endpoints.
    pub endpoints: Vec<FunctionFsEndpoint>,
    /// String descriptors.
    pub strings: FunctionFsStrings,
    /// Optional daemon metadata and readiness source.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon: Option<FunctionFsDaemonSpec>,
}

impl Default for FunctionFsConfig {
    fn default() -> Self {
        Self::new("vendor0", 0x1d6b, 0x0104)
    }
}

impl FunctionFsConfig {
    /// A vendor-class FunctionFS function with no endpoints or daemon yet.
    pub fn new(instance: impl Into<String>, vendor_id: u16, product_id: u16) -> Self {
        let instance = instance.into();
        Self {
            strings: FunctionFsStrings::for_instance(&instance),
            instance,
            vendor_id,
            product_id,
            bcd_device: 0x0100,
            class: 0xff,
            subclass: 0,
            protocol: 0,
            endpoints: Vec::new(),
            daemon: None,
        }
    }

    /// Set the interface class/subclass/protocol.
    pub fn with_interface(mut self, class: u8, subclass: u8, protocol: u8) -> Self {
        self.class = class;
        self.subclass = subclass;
        self.protocol = protocol;
        self
    }

    /// Append an endpoint.
    pub fn with_endpoint(mut self, endpoint: FunctionFsEndpoint) -> Self {
        self.endpoints.push(endpoint);
        self
    }

    /// Append several endpoints.
    pub fn with_endpoints(
        mut self,
        endpoints: impl IntoIterator<Item = FunctionFsEndpoint>,
    ) -> Self {
        self.endpoints.extend(endpoints);
        self
    }

    /// Replace the string descriptors.
    pub fn with_strings(mut self, strings: FunctionFsStrings) -> Self {
        self.strings = strings;
        self
    }

    /// Attach daemon metadata.
    pub fn with_daemon(mut self, daemon: FunctionFsDaemonSpec) -> Self {
        self.daemon = Some(daemon);
        self
    }

    /// Full ConfigFS function directory name (`ffs.<instance>`).
    pub fn configfs_name(&self) -> String {
        format!("ffs.{}", self.instance)
    }

    /// Observe readiness from configured metadata. This never executes the
    /// daemon.
    pub fn readiness(&self) -> FunctionFsReadiness {
        match &self.daemon {
            None => FunctionFsReadiness::DaemonNotConfigured,
            Some(daemon) => daemon.readiness(),
        }
    }

    /// A status snapshot derived from this model.
    pub fn status(&self) -> FunctionFsStatus {
        FunctionFsStatus::from_config(self)
    }

    /// Total generated descriptor bytes.
    pub fn descriptor_bytes(&self) -> usize {
        self.descriptors().total_len()
    }

    /// Fail with a typed unsupported state unless the daemon is ready.
    pub fn require_ready(&self) -> Result<(), UsbError> {
        let readiness = self.readiness();
        if readiness.is_ready() {
            Ok(())
        } else {
            Err(UsbError::UnsupportedFunctionFs {
                instance: self.instance.clone(),
                reason: format!(
                    "FunctionFS {} is {}; a userspace daemon must write descriptors \
                     and open the endpoints",
                    self.configfs_name(),
                    readiness.as_str()
                ),
            })
        }
    }

    /// Validate names, IDs, endpoint bounds, strings, daemon metadata, and the
    /// generated descriptor size.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !is_safe_identifier(&self.instance) {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.instance",
                reason: format!(
                    "{:?} must be a non-empty ASCII name containing only letters, digits, '-' or '_'",
                    self.instance
                ),
            });
        }
        if self.vendor_id == 0 || self.product_id == 0 {
            return Err(UsbError::InvalidField {
                field: "function.functionfs",
                reason: "vendor and product IDs must be non-zero".to_owned(),
            });
        }
        if self.bcd_device == 0 {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.bcd_device",
                reason: "device release number must be non-zero".to_owned(),
            });
        }
        if self.endpoints.len() > MAX_FUNCTIONFS_ENDPOINTS {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.endpoints",
                reason: format!(
                    "a FunctionFS function supports at most {MAX_FUNCTIONFS_ENDPOINTS} \
                     non-control endpoints"
                ),
            });
        }
        let mut addresses = std::collections::BTreeSet::new();
        for endpoint in &self.endpoints {
            endpoint.validate()?;
            if !addresses.insert(endpoint.address) {
                return Err(UsbError::InvalidConfig(format!(
                    "duplicate FunctionFS endpoint address 0x{:02x}",
                    endpoint.address
                )));
            }
        }
        self.strings.validate()?;
        if let Some(daemon) = &self.daemon {
            daemon.validate()?;
        }
        let descriptors = self.descriptors();
        let total = descriptors.total_len();
        if total == 0 || total > MAX_FUNCTIONFS_DESCRIPTOR_BYTES {
            return Err(UsbError::InvalidField {
                field: "function.functionfs.descriptors",
                reason: format!(
                    "generated descriptors must be 1..={MAX_FUNCTIONFS_DESCRIPTOR_BYTES} bytes"
                ),
            });
        }
        Ok(())
    }

    /// Generated descriptors a daemon would write to FunctionFS `ep0`.
    pub fn descriptors(&self) -> FunctionFsDescriptorSet {
        let strings = self.strings.descriptors();
        let mut device = Vec::with_capacity(18);
        device.push(18); // bLength
        device.push(0x01); // DEVICE
        device.extend_from_slice(&0x0200u16.to_le_bytes()); // bcdUSB
        device.push(self.class);
        device.push(self.subclass);
        device.push(self.protocol);
        device.push(64); // bMaxPacketSize0
        device.extend_from_slice(&self.vendor_id.to_le_bytes());
        device.extend_from_slice(&self.product_id.to_le_bytes());
        device.extend_from_slice(&self.bcd_device.to_le_bytes());
        device.push(1); // iManufacturer
        device.push(2); // iProduct
        device.push(3); // iSerialNumber
        device.push(1); // bNumConfigurations

        let total = 9 + 9 + self.endpoints.len() * 7;
        let mut configuration = Vec::with_capacity(total);
        configuration.push(9); // bLength
        configuration.push(0x02); // CONFIGURATION
        configuration.extend_from_slice(&(total as u16).to_le_bytes());
        configuration.push(1); // bNumInterfaces
        configuration.push(1); // bConfigurationValue
        configuration.push(4); // iConfiguration
        configuration.push(0x80); // bmAttributes: bus powered
        configuration.push(250); // bMaxPower: 500 mA

        configuration.push(9); // bLength
        configuration.push(0x04); // INTERFACE
        configuration.push(0); // bInterfaceNumber
        configuration.push(0); // bAlternateSetting
        configuration.push(self.endpoints.len() as u8);
        configuration.push(self.class);
        configuration.push(self.subclass);
        configuration.push(self.protocol);
        configuration.push(0); // iInterface

        for endpoint in &self.endpoints {
            configuration.extend_from_slice(&endpoint.descriptor());
        }

        FunctionFsDescriptorSet {
            device,
            configuration,
            strings,
        }
    }
}

/// Descriptor bytes generated for a FunctionFS daemon.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FunctionFsDescriptorSet {
    /// Eighteen-byte device descriptor.
    pub device: Vec<u8>,
    /// Configuration + interface + endpoint descriptors.
    pub configuration: Vec<u8>,
    /// String descriptors, index 0 first.
    pub strings: Vec<Vec<u8>>,
}

impl FunctionFsDescriptorSet {
    /// Total bytes across all descriptors.
    pub fn total_len(&self) -> usize {
        self.device.len()
            + self.configuration.len()
            + self.strings.iter().map(Vec::len).sum::<usize>()
    }

    /// Number of declared endpoints.
    pub fn endpoint_count(&self) -> usize {
        (self.configuration.len().saturating_sub(18)) / 7
    }
}

/// Readiness/status of a FunctionFS function.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionFsStatus {
    /// Function instance (`vendor0`).
    pub instance: String,
    /// Observed readiness state.
    pub readiness: FunctionFsReadiness,
    /// Whether the daemon has reported readiness.
    pub ready: bool,
    /// FunctionFS always needs a userspace daemon.
    pub requires_userspace: bool,
    /// Configured daemon executable, if any.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub daemon_path: Option<PathBuf>,
    /// Declared non-control endpoint count.
    pub endpoints: u8,
    /// Generated descriptor byte count.
    pub descriptor_bytes: usize,
}

impl FunctionFsStatus {
    /// Status derived from a complete function model.
    pub fn from_config(config: &FunctionFsConfig) -> Self {
        let readiness = config.readiness();
        Self {
            instance: config.instance.clone(),
            readiness,
            ready: readiness.is_ready(),
            requires_userspace: true,
            daemon_path: config.daemon.as_ref().map(|daemon| daemon.program.clone()),
            endpoints: config.endpoints.len() as u8,
            descriptor_bytes: config.descriptors().total_len(),
        }
    }

    /// Status for a ConfigFS reader that has no daemon/descriptor metadata.
    pub fn unobserved(instance: impl Into<String>) -> Self {
        Self {
            instance: instance.into(),
            readiness: FunctionFsReadiness::Unknown,
            ready: false,
            requires_userspace: true,
            daemon_path: None,
            endpoints: 0,
            descriptor_bytes: 0,
        }
    }

    /// Explicit missing-daemon status for an instance.
    pub fn missing(instance: impl Into<String>) -> Self {
        Self {
            instance: instance.into(),
            readiness: FunctionFsReadiness::DaemonMissing,
            ready: false,
            requires_userspace: true,
            daemon_path: None,
            endpoints: 0,
            descriptor_bytes: 0,
        }
    }
}

/// A userspace FunctionFS daemon status source.
///
/// Implementations only observe readiness. They must not report `Ready` unless
/// the daemon has actually written descriptors and opened its endpoints.
pub trait FunctionFsDaemon {
    /// Status for one FunctionFS function.
    fn status(&self, config: &FunctionFsConfig) -> FunctionFsStatus;
}

/// A daemon that is known to be absent; always reports `DaemonMissing`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct MissingFunctionFsDaemon;

impl FunctionFsDaemon for MissingFunctionFsDaemon {
    fn status(&self, config: &FunctionFsConfig) -> FunctionFsStatus {
        FunctionFsStatus::missing(config.instance.clone())
    }
}

/// A daemon model that observes only the configured program and marker files.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ObservedFunctionFsDaemon;

impl FunctionFsDaemon for ObservedFunctionFsDaemon {
    fn status(&self, config: &FunctionFsConfig) -> FunctionFsStatus {
        config.status()
    }
}

/// Reject a FunctionFS function whose daemon is not ready.
pub fn require_functionfs_ready(config: &FunctionFsConfig) -> Result<(), UsbError> {
    config.require_ready()
}

/// Whether `path` names the FunctionFS device directory a daemon would mount.
pub fn is_functionfs_function_dir(path: impl AsRef<Path>) -> bool {
    path.as_ref()
        .file_name()
        .and_then(|name| name.to_str())
        .and_then(|name| name.split_once('.'))
        .is_some_and(|(prefix, instance)| prefix == "ffs" && !instance.is_empty())
}

fn validate_string(field: &'static str, value: &str, allow_empty: bool) -> Result<(), UsbError> {
    if value.is_empty() && !allow_empty {
        return Err(UsbError::InvalidField {
            field,
            reason: "string must not be empty".to_owned(),
        });
    }
    if value.len() > MAX_FUNCTIONFS_STRING_BYTES {
        return Err(UsbError::InvalidField {
            field,
            reason: format!("string must be at most {MAX_FUNCTIONFS_STRING_BYTES} bytes"),
        });
    }
    if value.chars().any(char::is_control) {
        return Err(UsbError::InvalidField {
            field,
            reason: "string must not contain control characters".to_owned(),
        });
    }
    Ok(())
}

fn utf16_units(value: &str) -> Vec<u8> {
    value
        .encode_utf16()
        .flat_map(|unit| unit.to_le_bytes())
        .collect()
}

fn string_descriptor(units: &[u8]) -> Vec<u8> {
    let mut descriptor = Vec::with_capacity(2 + units.len());
    descriptor.push((2 + units.len()) as u8);
    descriptor.push(0x03); // STRING
    descriptor.extend_from_slice(units);
    descriptor
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn endpoint_addresses_and_descriptors() {
        let input = FunctionFsEndpoint::bulk_in(1, 512);
        assert_eq!(input.address, 0x81);
        assert!(input.is_in());
        assert_eq!(input.descriptor(), [7, 0x05, 0x81, 0x02, 0x00, 0x02, 0]);
        assert!(input.validate().is_ok());

        let output = FunctionFsEndpoint::isochronous_out(2, 192, 1);
        assert_eq!(output.address, 0x02);
        assert!(!output.is_in());
        assert!(output.validate().is_ok());
        assert!(output.descriptor()[3] == 1);
    }

    #[test]
    fn descriptor_set_is_bounded_and_consistent() {
        let config = FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
            .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
            .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512));
        assert!(config.validate().is_ok());
        let descriptors = config.descriptors();
        assert_eq!(descriptors.device.len(), 18);
        assert_eq!(descriptors.configuration.len(), 9 + 9 + 2 * 7);
        assert_eq!(descriptors.endpoint_count(), 2);
        assert_eq!(
            descriptors.total_len(),
            18 + 9 + 9 + 2 * 7 + descriptors.strings.iter().map(Vec::len).sum::<usize>()
        );
    }
}
