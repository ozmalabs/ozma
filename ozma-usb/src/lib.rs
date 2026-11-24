// SPDX-License-Identifier: AGPL-3.0-only
//! Safe USB gadget, virtual-media, and USB/IP foundations for Ozma nodes.
//!
//! This crate starts the Phase 3 USB/media workstream by porting the model
//! layer of `softnode/virtual_usb.py`, `softnode/usb_mass_storage.py`, and
//! `softnode/virtual_media.py`. `ozma-node` can wire it behind explicit
//! `--usb-gadget` flags:
//!
//! * [`UsbGadgetConfig`] / [`UsbFunction`] describe a composite gadget (HID,
//!   mass storage, UAC2, FunctionFS vendor) and validate it before any
//!   filesystem work.
//! * [`VirtualMedia`] / [`MassStorageImage`] model a backing image, disk vs
//!   CD-ROM presentation, read-only policy, size, and attach/detach state.
//! * [`UsbGadgetController`] is the synchronous, object-safe controller trait.
//! * [`ConfigFsUsbController`] implements it with safe `std::fs` calls against
//!   an injectable root (default `/sys/kernel/config/usb_gadget`).
//! * [`FakeUsbGadgetController`] mirrors the same contract entirely in memory.
//! * [`SubprocessUsbController`] provides the USB/IP system-tool fallback with
//!   a [`CommandRunner`] and argv-only, shell-free execution.
//!
//! No code in this crate uses `unsafe`, mounts a filesystem, opens a block
//! device, writes a disk image, or creates a real USB gadget. The ConfigFS
//! implementation only creates/removes directories, writes attribute files,
//! and creates configuration symlinks. Tests use a temporary root; the one
//! real-ConfigFS test is `#[ignore]` and environment-gated.
//!
//! # Example
//!
//! ```rust,no_run
//! use ozma_usb::{
//!     ConfigFsUsbController, MassStorageImage, UsbFunction, UsbGadgetConfig,
//!     UsbGadgetController,
//! };
//!
//! # fn main() -> Result<(), ozma_usb::UsbError> {
//! let controller = ConfigFsUsbController::with_default_root();
//! let config = UsbGadgetConfig::new("ozma", "dummy_udc.0")
//!     .with_function(UsbFunction::keyboard("kbd"))
//!     .with_function(UsbFunction::mouse("mouse"))
//!     .with_function(UsbFunction::mass_storage(
//!         "usb0",
//!         Some(MassStorageImage::cdrom("/srv/installer.iso")),
//!     ));
//!
//! let status = controller.create_and_bind(&config)?;
//! assert!(status.bound);
//! # Ok(())
//! # }
//! ```
//!
//! The node integration will select between a real controller and the fake
//! based on `/sys/kernel/config/usb_gadget` availability and configuration,
//! then project [`UsbGadgetCapabilities::protocol_capability`] into the shared
//! `ozma-protocol` registration payload.

#![forbid(unsafe_code)]

pub mod config;
pub mod configfs;
pub mod controller;
pub mod error;
pub mod fake;
pub mod media;
pub mod status;
pub mod subprocess;

pub use config::{
    is_safe_identifier, HidProtocol, UsbFunction, UsbFunctionKind, UsbGadgetConfig,
    KEYBOARD_REPORT_DESCRIPTOR, MAX_POWER_MA, MOUSE_REPORT_DESCRIPTOR,
};
pub use configfs::{ConfigFsUsbController, DEFAULT_CONFIGFS_ROOT};
pub use controller::UsbGadgetController;
pub use error::UsbError;
pub use fake::FakeUsbGadgetController;
pub use media::{MassStorageImage, MediaMode, MediaState, VirtualMedia};
pub use status::{FunctionStatus, GadgetStatus, UsbGadgetCapabilities};
pub use subprocess::{
    CommandOutput, CommandRunner, CommandSpec, SubprocessUsbController, SystemCommandRunner,
    UsbIpControl, UsbIpDevice,
};
