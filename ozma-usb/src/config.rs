// SPDX-License-Identifier: AGPL-3.0-only
//! Gadget and function configuration models.
//!
//! The model follows the Linux ConfigFS composite-gadget layout used by
//! `softnode/virtual_usb.py`: one gadget directory, string descriptors,
//! one configuration, and one or more function directories linked into that
//! configuration. It is deliberately pure: validation does not touch ConfigFS.

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::functionfs::FunctionFsConfig;
use crate::media::MassStorageImage;
use crate::uac2::Uac2Config;

/// Standard boot-keyboard HID report descriptor (8-byte reports).
pub const KEYBOARD_REPORT_DESCRIPTOR: [u8; 65] = [
    0x05, 0x01, 0x09, 0x06, 0xA1, 0x01, 0x05, 0x07, 0x19, 0xE0, 0x29, 0xE7, 0x15, 0x00, 0x25, 0x01,
    0x75, 0x01, 0x95, 0x08, 0x81, 0x02, 0x95, 0x01, 0x75, 0x08, 0x81, 0x01, 0x95, 0x05, 0x75, 0x01,
    0x05, 0x08, 0x19, 0x01, 0x29, 0x05, 0x91, 0x02, 0x95, 0x01, 0x75, 0x03, 0x91, 0x01, 0x95, 0x06,
    0x75, 0x08, 0x15, 0x00, 0x26, 0xFF, 0x00, 0x05, 0x07, 0x19, 0x00, 0x2A, 0xFF, 0x00, 0x81, 0x00,
    0xC0,
];

/// Absolute-pointer HID report descriptor (6-byte reports).
pub const MOUSE_REPORT_DESCRIPTOR: [u8; 64] = [
    0x05, 0x01, 0x09, 0x02, 0xA1, 0x01, 0x09, 0x01, 0xA1, 0x00, 0x05, 0x09, 0x19, 0x01, 0x29, 0x03,
    0x15, 0x00, 0x25, 0x01, 0x75, 0x01, 0x95, 0x03, 0x81, 0x02, 0x95, 0x01, 0x75, 0x05, 0x81, 0x01,
    0x05, 0x01, 0x09, 0x30, 0x09, 0x31, 0x16, 0x00, 0x00, 0x26, 0xFF, 0x7F, 0x75, 0x10, 0x95, 0x02,
    0x81, 0x02, 0x09, 0x38, 0x15, 0x81, 0x25, 0x7F, 0x75, 0x08, 0x95, 0x01, 0x81, 0x06, 0xC0, 0xC0,
];

/// Highest power the model accepts, in milliamps (USB 3 bus-powered limit).
pub const MAX_POWER_MA: u16 = 900;

/// A ConfigFS function type modelled by this crate.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum UsbFunctionKind {
    /// Human-interface device (`functions/hid.<instance>`).
    Hid,
    /// Mass storage (`functions/mass_storage.<instance>`).
    MassStorage,
    /// USB Audio Class 2 (`functions/uac2.<instance>`).
    Uac2,
    /// FunctionFS vendor function (`functions/ffs.<instance>`); needs a
    /// userspace FunctionFS endpoint daemon before it can enumerate.
    Vendor,
}

impl UsbFunctionKind {
    /// Stable lower-case name used in status and capability JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Hid => "hid",
            Self::MassStorage => "mass_storage",
            Self::Uac2 => "uac2",
            Self::Vendor => "vendor",
        }
    }

    /// ConfigFS directory prefix.
    pub fn configfs_prefix(self) -> &'static str {
        match self {
            Self::Hid => "hid",
            Self::MassStorage => "mass_storage",
            Self::Uac2 => "uac2",
            Self::Vendor => "ffs",
        }
    }

    /// Infer a kind from a ConfigFS function directory name.
    pub fn from_configfs_name(name: &str) -> Option<Self> {
        let (prefix, instance) = name.split_once('.')?;
        if instance.is_empty() {
            return None;
        }
        match prefix {
            "hid" => Some(Self::Hid),
            "mass_storage" => Some(Self::MassStorage),
            "uac2" => Some(Self::Uac2),
            "ffs" => Some(Self::Vendor),
            _ => None,
        }
    }
}

impl std::fmt::Display for UsbFunctionKind {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

/// HID boot protocol advertised by a `hid.<instance>` function.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HidProtocol {
    /// No boot protocol (`protocol=0`).
    #[default]
    None,
    /// Boot keyboard (`protocol=1`).
    Keyboard,
    /// Boot mouse (`protocol=2`).
    Mouse,
}

impl HidProtocol {
    /// Value written to the ConfigFS `protocol` attribute.
    pub fn configfs_value(self) -> u8 {
        match self {
            Self::None => 0,
            Self::Keyboard => 1,
            Self::Mouse => 2,
        }
    }
}

/// One function in a composite USB gadget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum UsbFunction {
    /// HID keyboard, mouse, or custom report descriptor.
    Hid {
        /// Instance name (the part after `hid.`).
        instance: String,
        /// Boot protocol.
        protocol: HidProtocol,
        /// HID subclass (`1` for boot devices).
        subclass: u8,
        /// Report length in bytes.
        report_length: u8,
        /// Binary HID report descriptor.
        report_descriptor: Vec<u8>,
    },
    /// USB mass storage backed by a file, ISO, or userspace-synthesised image.
    MassStorage {
        /// Instance name (the part after `mass_storage.`).
        instance: String,
        /// Initial backing image; `None` starts with an empty LUN.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        image: Option<MassStorageImage>,
        /// Whether the LUN is removable.
        removable: bool,
    },
    /// UAC2 audio function.
    Uac2 {
        /// Instance name (the part after `uac2.`).
        instance: String,
        /// Expanded descriptor/capability model.
        config: Uac2Config,
    },
    /// FunctionFS vendor function.
    Vendor {
        /// Expanded FunctionFS descriptor/daemon contract.
        config: FunctionFsConfig,
    },
}

impl UsbFunction {
    /// Standard keyboard HID function.
    pub fn keyboard(instance: impl Into<String>) -> Self {
        Self::Hid {
            instance: instance.into(),
            protocol: HidProtocol::Keyboard,
            subclass: 1,
            report_length: 8,
            report_descriptor: KEYBOARD_REPORT_DESCRIPTOR.to_vec(),
        }
    }

    /// Standard boot-mouse HID function.
    pub fn mouse(instance: impl Into<String>) -> Self {
        Self::Hid {
            instance: instance.into(),
            protocol: HidProtocol::Mouse,
            subclass: 1,
            report_length: 6,
            report_descriptor: MOUSE_REPORT_DESCRIPTOR.to_vec(),
        }
    }

    /// A mass-storage function with an optional initial image.
    pub fn mass_storage(instance: impl Into<String>, image: Option<MassStorageImage>) -> Self {
        Self::MassStorage {
            instance: instance.into(),
            image,
            removable: true,
        }
    }

    /// A UAC2 audio function.
    pub fn uac2(
        instance: impl Into<String>,
        playback_channels: u8,
        capture_channels: u8,
        sample_rate_hz: u32,
        bit_depth: u8,
    ) -> Self {
        Self::Uac2 {
            instance: instance.into(),
            config: Uac2Config::new(
                playback_channels,
                capture_channels,
                sample_rate_hz,
                bit_depth,
            ),
        }
    }

    /// A UAC2 function with an expanded descriptor/capability model.
    pub fn uac2_with_config(instance: impl Into<String>, config: Uac2Config) -> Self {
        Self::Uac2 {
            instance: instance.into(),
            config,
        }
    }

    /// A FunctionFS vendor function.
    pub fn vendor(instance: impl Into<String>, vendor_id: u16, product_id: u16) -> Self {
        Self::Vendor {
            config: FunctionFsConfig::new(instance, vendor_id, product_id),
        }
    }

    /// A FunctionFS vendor function with descriptors and daemon metadata.
    pub fn vendor_with_config(config: FunctionFsConfig) -> Self {
        Self::Vendor { config }
    }

    /// Function instance name.
    pub fn instance(&self) -> &str {
        match self {
            Self::Hid { instance, .. }
            | Self::MassStorage { instance, .. }
            | Self::Uac2 { instance, .. } => instance,
            Self::Vendor { config } => &config.instance,
        }
    }

    /// Function kind.
    pub fn kind(&self) -> UsbFunctionKind {
        match self {
            Self::Hid { .. } => UsbFunctionKind::Hid,
            Self::MassStorage { .. } => UsbFunctionKind::MassStorage,
            Self::Uac2 { .. } => UsbFunctionKind::Uac2,
            Self::Vendor { .. } => UsbFunctionKind::Vendor,
        }
    }

    /// Full ConfigFS function directory name (`hid.kbd`).
    pub fn configfs_name(&self) -> String {
        format!("{}.{}", self.kind().configfs_prefix(), self.instance())
    }

    /// Whether this function needs a userspace daemon before enumeration.
    pub fn requires_userspace(&self) -> bool {
        matches!(self, Self::Vendor { .. })
    }

    /// Validate this function's shape and attributes.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !is_safe_identifier(self.instance()) {
            return Err(UsbError::InvalidField {
                field: "function.instance",
                reason: format!(
                    "{:?} must be a non-empty ASCII name containing only letters, digits, '-' or '_'",
                    self.instance()
                ),
            });
        }

        match self {
            Self::Hid {
                protocol,
                report_length,
                report_descriptor,
                ..
            } => {
                if *report_length == 0 || *report_length > 64 {
                    return Err(UsbError::InvalidField {
                        field: "function.report_length",
                        reason: "HID report length must be 1..=64 bytes".to_owned(),
                    });
                }
                if report_descriptor.is_empty() {
                    return Err(UsbError::InvalidField {
                        field: "function.report_descriptor",
                        reason: "HID report descriptor must not be empty".to_owned(),
                    });
                }
                if report_descriptor.len() > 4096 {
                    return Err(UsbError::InvalidField {
                        field: "function.report_descriptor",
                        reason: "HID report descriptor must be at most 4096 bytes".to_owned(),
                    });
                }
                if matches!(protocol, HidProtocol::Keyboard) && *report_length != 8 {
                    return Err(UsbError::InvalidField {
                        field: "function.report_length",
                        reason: "boot keyboard reports are 8 bytes".to_owned(),
                    });
                }
                if matches!(protocol, HidProtocol::Mouse) && *report_length != 6 {
                    return Err(UsbError::InvalidField {
                        field: "function.report_length",
                        reason: "boot mouse reports are 6 bytes".to_owned(),
                    });
                }
            }
            Self::MassStorage {
                image, instance, ..
            } => {
                if instance.is_empty() {
                    return Err(UsbError::InvalidField {
                        field: "function.instance",
                        reason: "mass-storage instance name must not be empty".to_owned(),
                    });
                }
                if let Some(image) = image {
                    image.validate()?;
                }
            }
            Self::Uac2 { config, .. } => {
                config.validate()?;
            }
            Self::Vendor { config } => {
                config.validate()?;
            }
        }

        Ok(())
    }
}

/// A complete composite USB gadget description.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbGadgetConfig {
    /// Gadget directory name (`/sys/kernel/config/usb_gadget/<name>`).
    pub name: String,
    /// USB vendor ID.
    pub id_vendor: u16,
    /// USB product ID.
    pub id_product: u16,
    /// Device release number (`bcdDevice`).
    pub bcd_device: u16,
    /// USB specification version (`bcdUSB`).
    pub bcd_usb: u16,
    /// UDC to bind (`dummy_udc.0`, `fe980000.usb`, ...).
    pub udc: String,
    /// Manufacturer string.
    pub manufacturer: String,
    /// Product string.
    pub product: String,
    /// Serial-number string; may be empty.
    pub serial_number: String,
    /// Configuration string.
    pub configuration: String,
    /// Maximum power in milliamps.
    pub max_power_ma: u16,
    /// Functions in the composite gadget.
    pub functions: Vec<UsbFunction>,
}

impl UsbGadgetConfig {
    /// A gadget with the Ozma defaults and no functions.
    pub fn new(name: impl Into<String>, udc: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            id_vendor: 0x1d6b,
            id_product: 0x0104,
            bcd_device: 0x0100,
            bcd_usb: 0x0200,
            udc: udc.into(),
            manufacturer: "Ozma Labs".to_owned(),
            product: "Ozma USB Gadget".to_owned(),
            serial_number: "OZMA0001".to_owned(),
            configuration: "Ozma USB Configuration".to_owned(),
            max_power_ma: 500,
            functions: Vec::new(),
        }
    }

    /// Append a function.
    pub fn with_function(mut self, function: UsbFunction) -> Self {
        self.functions.push(function);
        self
    }

    /// Override vendor/product identity.
    pub fn with_identity(mut self, id_vendor: u16, id_product: u16) -> Self {
        self.id_vendor = id_vendor;
        self.id_product = id_product;
        self
    }

    /// Override the human-readable strings.
    pub fn with_strings(
        mut self,
        manufacturer: impl Into<String>,
        product: impl Into<String>,
        serial_number: impl Into<String>,
    ) -> Self {
        self.manufacturer = manufacturer.into();
        self.product = product.into();
        self.serial_number = serial_number.into();
        self
    }

    /// Override maximum power.
    pub fn with_max_power_ma(mut self, max_power_ma: u16) -> Self {
        self.max_power_ma = max_power_ma;
        self
    }

    /// Validate the gadget name, descriptors, strings, and every function.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !is_safe_identifier(&self.name) {
            return Err(UsbError::InvalidField {
                field: "name",
                reason: format!(
                    "{:?} must be a non-empty ASCII name of at most 63 characters containing \
                     only letters, digits, '-' or '_'",
                    self.name
                ),
            });
        }
        if !is_safe_udc(&self.udc) {
            return Err(UsbError::InvalidField {
                field: "udc",
                reason: format!(
                    "{:?} must be a non-empty UDC name (letters, digits, '-', '_', '.', ':')",
                    self.udc
                ),
            });
        }
        if self.id_vendor == 0 {
            return Err(UsbError::InvalidField {
                field: "id_vendor",
                reason: "vendor ID must be non-zero".to_owned(),
            });
        }
        if self.id_product == 0 {
            return Err(UsbError::InvalidField {
                field: "id_product",
                reason: "product ID must be non-zero".to_owned(),
            });
        }
        if self.bcd_usb == 0 || self.bcd_device == 0 {
            return Err(UsbError::InvalidField {
                field: "bcd",
                reason: "bcdUSB and bcdDevice must be non-zero".to_owned(),
            });
        }
        if self.max_power_ma > MAX_POWER_MA {
            return Err(UsbError::InvalidField {
                field: "max_power_ma",
                reason: format!("maximum power must be at most {MAX_POWER_MA} mA"),
            });
        }
        validate_string("manufacturer", &self.manufacturer, false)?;
        validate_string("product", &self.product, false)?;
        validate_string("serial_number", &self.serial_number, true)?;
        validate_string("configuration", &self.configuration, false)?;

        if self.functions.is_empty() {
            return Err(UsbError::InvalidField {
                field: "functions",
                reason: "a composite gadget needs at least one function".to_owned(),
            });
        }
        if self.functions.len() > 32 {
            return Err(UsbError::InvalidField {
                field: "functions",
                reason: "at most 32 functions are supported".to_owned(),
            });
        }

        let mut names = std::collections::BTreeSet::new();
        for function in &self.functions {
            function.validate()?;
            let name = function.configfs_name();
            if !names.insert(name.clone()) {
                return Err(UsbError::InvalidConfig(format!(
                    "duplicate ConfigFS function {name:?}"
                )));
            }
        }

        Ok(())
    }
}

/// Safe ConfigFS gadget/function identifier: non-empty ASCII letters, digits,
/// `-`, and `_`, at most 63 characters. This excludes `/`, `.`, `..`, spaces,
/// and control characters.
pub fn is_safe_identifier(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || character == '-' || character == '_'
        })
        && value
            .chars()
            .any(|character| character.is_ascii_alphanumeric())
}

pub(crate) fn is_safe_udc(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 63
        && value.chars().all(|character| {
            character.is_ascii_alphanumeric() || matches!(character, '-' | '_' | '.' | ':')
        })
        && !value.starts_with('.')
}

fn validate_string(field: &'static str, value: &str, allow_empty: bool) -> Result<(), UsbError> {
    if value.is_empty() && !allow_empty {
        return Err(UsbError::InvalidField {
            field,
            reason: "string must not be empty".to_owned(),
        });
    }
    if value.len() > 126 {
        return Err(UsbError::InvalidField {
            field,
            reason: "string must be at most 126 bytes".to_owned(),
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
