// SPDX-License-Identifier: AGPL-3.0-only
//! Status and capability shapes shared by the node, controller API, and tests.

use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::config::UsbFunctionKind;
use crate::media::MediaState;

/// Capability advertisement for a node's USB/media subsystem.
///
/// This is intentionally a superset of `ozma_protocol::UsbCapability`: it keeps
/// the protocol booleans and adds the ConfigFS availability and function kinds
/// needed by the node status route.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsbGadgetCapabilities {
    /// A ConfigFS gadget can be created on this host.
    pub gadget: bool,
    /// USB/IP system tools are available/configured.
    pub usbip: bool,
    /// Virtual media can be attached to a mass-storage function.
    pub virtual_media: bool,
    /// Mass-storage functions are modelled and supported.
    pub mass_storage: bool,
    /// The configured ConfigFS root exists.
    pub configfs: bool,
    /// Function kinds this build can describe.
    #[serde(default)]
    pub functions: Vec<UsbFunctionKind>,
}

impl Default for UsbGadgetCapabilities {
    fn default() -> Self {
        Self::unavailable()
    }
}

impl UsbGadgetCapabilities {
    /// No gadget support is available.
    pub fn unavailable() -> Self {
        Self {
            gadget: false,
            usbip: false,
            virtual_media: false,
            mass_storage: false,
            configfs: false,
            functions: Vec::new(),
        }
    }

    /// All modelled gadget features are available for a live ConfigFS root.
    pub fn available() -> Self {
        Self {
            gadget: true,
            usbip: false,
            virtual_media: true,
            mass_storage: true,
            configfs: true,
            functions: vec![
                UsbFunctionKind::Hid,
                UsbFunctionKind::MassStorage,
                UsbFunctionKind::Uac2,
                UsbFunctionKind::Vendor,
            ],
        }
    }

    /// Probe a ConfigFS root without creating or changing anything.
    pub fn at(configfs_root: impl AsRef<Path>) -> Self {
        let root = configfs_root.as_ref();
        let configfs = root.is_dir();
        if !configfs {
            return Self::unavailable();
        }
        Self {
            configfs: true,
            gadget: true,
            virtual_media: true,
            mass_storage: true,
            usbip: false,
            functions: vec![
                UsbFunctionKind::Hid,
                UsbFunctionKind::MassStorage,
                UsbFunctionKind::Uac2,
                UsbFunctionKind::Vendor,
            ],
        }
    }

    /// Mark USB/IP support (for a node that configured a `usbip` helper).
    pub fn with_usbip(mut self, usbip: bool) -> Self {
        self.usbip = usbip;
        self
    }

    /// Project onto the shared protocol capability type.
    pub fn protocol_capability(&self) -> ozma_protocol::UsbCapability {
        ozma_protocol::UsbCapability {
            gadget: self.gadget,
            usbip: self.usbip,
            virtual_media: self.virtual_media,
        }
    }
}

/// Observed state of one configuration-visible function.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct FunctionStatus {
    /// Full ConfigFS function name (`hid.kbd`, `mass_storage.usb0`, ...).
    pub name: String,
    /// Modelled function kind.
    pub kind: UsbFunctionKind,
    /// Whether the function is linked into `configs/c.1`.
    pub linked: bool,
    /// Whether a userspace FunctionFS daemon must run before enumeration.
    pub requires_userspace: bool,
    /// Current media state for a mass-storage function.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub media: Option<MediaState>,
}

/// Observed state of one composite gadget.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct GadgetStatus {
    /// Gadget directory name.
    pub name: String,
    /// Whether the gadget directory currently exists.
    pub present: bool,
    /// Whether a UDC is bound.
    pub bound: bool,
    /// Bound UDC, when present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub udc: Option<String>,
    /// Functions found under `functions/`.
    #[serde(default)]
    pub functions: Vec<FunctionStatus>,
}

impl GadgetStatus {
    /// Find a function by full ConfigFS name or instance name.
    pub fn function(&self, name: &str) -> Option<&FunctionStatus> {
        self.functions.iter().find(|function| {
            function.name == name
                || function
                    .name
                    .split_once('.')
                    .is_some_and(|(_, instance)| instance == name)
        })
    }

    /// Currently attached media across all mass-storage functions.
    pub fn attached_media(&self) -> impl Iterator<Item = (&str, &MediaState)> {
        self.functions.iter().filter_map(|function| {
            function
                .media
                .as_ref()
                .filter(|media| media.is_attached())
                .map(|media| (function.name.as_str(), media))
        })
    }

    /// Empty placeholder used by fake controllers before a create.
    pub fn absent(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            present: false,
            bound: false,
            udc: None,
            functions: Vec::new(),
        }
    }
}
