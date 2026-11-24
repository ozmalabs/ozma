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
//!   filesystem work. [`Uac2Config`] models descriptor capabilities and
//!   ConfigFS attributes; [`FunctionFsConfig`] models endpoint/string
//!   descriptors and explicit userspace-daemon readiness. The
//!   [`FunctionFsDaemonCore`] additionally encodes the FunctionFS `ep0`
//!   descriptor/string blobs, handles standard control requests, and services
//!   injected endpoints; [`FunctionFsLifecycle`] sequences ConfigFS
//!   function/link creation, an injected mount, descriptor writes, readiness
//!   polling, UDC bind/unbind, and cleanup without mounting by default.
//! * [`VirtualMedia`] / [`MassStorageImage`] model a backing image, disk vs
//!   CD-ROM presentation, read-only policy, declared [`MediaFormat`], size,
//!   and attach/detach state.
//! * [`UsbGadgetController`] is the synchronous, object-safe controller trait.
//! * [`ConfigFsUsbController`] implements it with safe `std::fs` calls against
//!   an injectable root (default `/sys/kernel/config/usb_gadget`).
//! * [`FakeUsbGadgetController`] mirrors the same contract entirely in memory.
//! * [`SubprocessUsbController`] provides the USB/IP system-tool fallback with
//!   a [`CommandRunner`] and argv-only, shell-free execution.
//! * [`UsbIpConfig`] / [`UsbIpState`] and [`SubprocessUsbIpController`] model
//!   USB/IP enablement, host/port, local exports, remote attachments, host and
//!   token authorization, peer key-fingerprint pairing, bounded hotplug events,
//!   and a fail-closed TLS-required policy without a shell.
//! * [`Fat32ImageBuilder`] creates a bounded, read-only FAT32 image in memory
//!   from files/directories; [`parse_fat32_image`] validates one read-only.
//!   [`WriteOverlay`] layers a bounded, copy-on-write block cache with
//!   checksummed persistent commits over an immutable FAT32/raw base.
//! * [`IsoImage`] inspects existing ISO 9660 images read-only, and
//!   [`NbdExport`] / [`NbdRequest`] model NBD exports and framing.
//! * [`NbdServer`] serves a regular-file, in-memory, or overlay-backed image
//!   over a bounded fixed-newstyle TCP/Unix NBD listener with an injectable
//!   transport/clock; writable exports never modify their base image.
//!
//! No code in this crate uses `unsafe`, mounts a filesystem, opens a block
//! device, writes a block device, or creates a real USB gadget. The ConfigFS
//! implementation only creates/removes directories, writes attribute files,
//! and creates configuration symlinks. FAT32 construction is an in-memory byte
//! buffer; ISO inspection reads only a bounded descriptor header; the NBD
//! server opens regular files and loopback sockets only. Tests use a temporary
//! root and synthetic images; FunctionFS defaults to a temp mountpoint and fake
//! mounter; the real-ConfigFS and live-NBD-boot tests are `#[ignore]` and
//! environment-gated.
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
pub mod fat32;
pub mod functionfs;
pub mod functionfs_daemon;
pub mod functionfs_lifecycle;
pub mod iso;
pub mod media;
pub mod nbd;
pub mod nbd_server;
pub mod overlay;
pub mod status;
pub mod subprocess;
pub mod uac2;
pub mod usbip;

pub use config::{
    is_safe_identifier, HidProtocol, UsbFunction, UsbFunctionKind, UsbGadgetConfig,
    KEYBOARD_REPORT_DESCRIPTOR, MAX_POWER_MA, MOUSE_REPORT_DESCRIPTOR,
};
pub use configfs::{ConfigFsUsbController, DEFAULT_CONFIGFS_ROOT};
pub use controller::UsbGadgetController;
pub use error::UsbError;
pub use fake::FakeUsbGadgetController;
pub use fat32::{
    build_fat32_image, fat32_image_from_directory, parse_fat32_boot_sector, parse_fat32_image,
    validate_fat32_boot_sector, Fat32BootInfo, Fat32EntryInfo, Fat32Image, Fat32ImageBuilder,
    Fat32Info, Fat32Layout, Fat32Node, Fat32Options, FAT32_DEFAULT_CLUSTER_SIZE,
};
pub use functionfs::{
    is_functionfs_function_dir, require_functionfs_ready, FunctionFsConfig, FunctionFsDaemon,
    FunctionFsDaemonSpec, FunctionFsDaemonStatus, FunctionFsDescriptorSet, FunctionFsEndpoint,
    FunctionFsOsDescriptor, FunctionFsOsDescriptorKind, FunctionFsOsProperty, FunctionFsReadiness,
    FunctionFsStatus, FunctionFsStrings, FunctionFsTransfer, MissingFunctionFsDaemon,
    ObservedFunctionFsDaemon, MAX_FUNCTIONFS_DESCRIPTOR_BYTES, MAX_FUNCTIONFS_ENDPOINTS,
    MAX_FUNCTIONFS_OS_DESCRIPTORS, MAX_FUNCTIONFS_OS_DESCRIPTOR_BYTES, MAX_FUNCTIONFS_STRING_BYTES,
};
pub use functionfs_daemon::{
    handle_control_request, parse_descriptor_blob, parse_string_blob, FakeFunctionFsDaemon,
    FileEndpointIo, FileEndpointOpener, FileEp0Io, FileFunctionFsDaemon, FunctionFsControlRequest,
    FunctionFsControlResponse, FunctionFsDaemonCore, FunctionFsDaemonStats, FunctionFsDescriptors,
    FunctionFsDirection, FunctionFsEndpointIo, FunctionFsEndpointOpener,
    FunctionFsEndpointTransfer, FunctionFsEp0Io, FunctionFsEvent, FunctionFsEventOutcome,
    FunctionFsEventType, FunctionFsMount, FunctionFsWriteReport, LoopbackEndpoint,
    LoopbackEndpointOpener, LoopbackFunctionFsDaemon, MemoryEp0, ParsedFunctionFsDescriptors,
    FUNCTIONFS_DESCRIPTORS_MAGIC_V2, FUNCTIONFS_EVENT_BYTES, FUNCTIONFS_HAS_FS_DESC,
    FUNCTIONFS_HAS_HS_DESC, FUNCTIONFS_HAS_MS_OS_DESC, FUNCTIONFS_HAS_SS_DESC,
    FUNCTIONFS_STRINGS_MAGIC, MAX_FUNCTIONFS_ENDPOINT_TRANSFER_BYTES,
    MAX_FUNCTIONFS_EP0_WRITE_BYTES,
};
pub use functionfs_lifecycle::{
    CommandFunctionFsMounter, FakeFunctionFsMounter, FunctionFsLifecycle, FunctionFsLifecycleClock,
    FunctionFsLifecycleConfig, FunctionFsLifecyclePhase, FunctionFsLifecycleStatus,
    FunctionFsMounter, ManualFunctionFsLifecycleClock, SystemFunctionFsLifecycleClock,
    DEFAULT_FUNCTIONFS_MOUNT_BASE,
};
pub use iso::{
    parse_iso_volume_descriptors, validate_iso_image, IsoImage, IsoVolumeDescriptor,
    ISO9660_SECTOR_SIZE,
};
pub use media::{MassStorageImage, MediaFormat, MediaMode, MediaState, VirtualMedia};
pub use nbd::{
    validate_nbd_descriptor, NbdCommand, NbdExport, NbdExportDescriptor, NbdHandshake, NbdReply,
    NbdRequest, NbdRequestHeader, NBD_DEFAULT_PORT, NBD_HANDSHAKE_LEN, NBD_MAX_REQUEST_BYTES,
    NBD_REPLY_HEADER_LEN, NBD_REQUEST_HEADER_LEN,
};
pub use nbd_server::{
    serve_session, ManualClock, NbdBackend, NbdClock, NbdExportCatalog, NbdExportSource,
    NbdFileImage, NbdMemoryImage, NbdOverlayBackend, NbdOverlayPolicy, NbdOverlayStatus, NbdServer,
    NbdServerConfig, NbdServerHandle, NbdServerStats, NbdSessionEnd, NbdSessionOutcome,
    NbdTransport, SystemClock,
};
pub use overlay::{
    OverlayConfig, OverlayWriteReport, WriteOverlay, OVERLAY_DEFAULT_BLOCK_SIZE,
    OVERLAY_DEFAULT_MAX_BASE_BYTES, OVERLAY_DEFAULT_MAX_BYTES, OVERLAY_HEADER_BYTES, OVERLAY_MAGIC,
    OVERLAY_MAX_BLOCK_SIZE, OVERLAY_MIN_BLOCK_SIZE, OVERLAY_VERSION,
};
pub use status::{FunctionStatus, GadgetStatus, UsbGadgetCapabilities};
pub use subprocess::{
    CommandOutput, CommandRunner, CommandSpec, SubprocessUsbController, SystemCommandRunner,
    UsbIpControl, UsbIpDevice,
};
pub use uac2::{
    Uac2BridgeStatus, Uac2Config, Uac2DescriptorSet, Uac2DescriptorSummary, Uac2Mode, Uac2Status,
    Uac2StreamStatus, UAC2_CAPTURE_ENDPOINT, UAC2_MAX_CHANNELS, UAC2_MAX_DESCRIPTOR_BYTES,
    UAC2_MAX_SAMPLE_RATE, UAC2_MAX_SAMPLE_RATES, UAC2_MAX_SAMPLE_SIZES, UAC2_MIN_SAMPLE_RATE,
    UAC2_PLAYBACK_ENDPOINT,
};
pub use usbip::{
    SubprocessUsbIpController, UsbIpAttachment, UsbIpAuthorization, UsbIpConfig, UsbIpController,
    UsbIpHotplugEvent, UsbIpHotplugKind, UsbIpHotplugLog, UsbIpKey, UsbIpPeerGrant, UsbIpState,
    UsbIpTlsPolicy, UsbIpTlsStatus, UsbIpToken, DEFAULT_USBIP_PORT, MAX_USBIP_HOTPLUG_EVENTS,
};
