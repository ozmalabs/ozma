// SPDX-License-Identifier: AGPL-3.0-only
//! Typed failures for USB gadget, virtual-media, and USB/IP operations.

use std::io;
use std::path::PathBuf;

/// A typed USB/media failure.
///
/// Every variant is deliberately specific enough for a node API or the
/// controller to return a stable machine-readable `kind()` without parsing
/// prose. Filesystem failures keep the failing path and operation so a
/// misconfigured ConfigFS root is distinguishable from a missing gadget.
#[derive(Debug, thiserror::Error)]
pub enum UsbError {
    /// The gadget configuration itself is invalid.
    #[error("invalid USB gadget configuration: {0}")]
    InvalidConfig(String),

    /// One configuration field failed validation.
    #[error("invalid {field}: {reason}")]
    InvalidField {
        /// Field name (`name`, `udc`, `functions`, ...).
        field: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// The named gadget directory does not exist.
    #[error("USB gadget {0:?} does not exist")]
    MissingGadget(String),

    /// The named gadget directory already exists.
    #[error("USB gadget {0:?} already exists")]
    GadgetExists(String),

    /// A mass-storage function is not present in the gadget.
    #[error("USB gadget {gadget:?} has no mass-storage function {function:?}")]
    MissingFunction {
        /// Gadget name.
        gadget: String,
        /// Function instance or `mass_storage.<instance>`.
        function: String,
    },

    /// The gadget is already bound to another UDC.
    #[error("USB gadget {gadget:?} is already bound to UDC {udc:?}")]
    AlreadyBound {
        /// Gadget name.
        gadget: String,
        /// Currently bound UDC.
        udc: String,
    },

    /// ConfigFS is not mounted at the configured root.
    #[error("ConfigFS root {0} does not exist; load libcomposite and mount configfs")]
    MissingRoot(PathBuf),

    /// The configured ConfigFS root exists but is not a directory.
    #[error("ConfigFS root {0} is not a directory")]
    RootNotDirectory(PathBuf),

    /// A media image path does not exist.
    #[error("media image {0} does not exist")]
    MediaNotFound(PathBuf),

    /// A media image path exists but is not a regular file.
    #[error("media image {0} is not a regular file")]
    MediaNotFile(PathBuf),

    /// An operation would make a read-only image writable.
    #[error("media is read-only: {0}")]
    ReadOnlyMedia(String),

    /// A media image is not valid for its declared format.
    #[error("invalid {format} media {path}: {reason}")]
    InvalidMedia {
        /// Media format name (`iso9660`, `fat32`, `nbd`).
        format: &'static str,
        /// Path that failed validation.
        path: PathBuf,
        /// Human-readable reason.
        reason: String,
    },

    /// A media format cannot be used by the requested operation.
    #[error("media format {format} is not supported here: {reason}")]
    UnsupportedMediaFormat {
        /// Media format name.
        format: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// A USB/IP bus id, host, or port is malformed.
    #[error("invalid USB/IP {field}: {reason}")]
    InvalidUsbIp {
        /// Field name (`bus_id`, `host`, `port`).
        field: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// A USB/IP operation was attempted while the feature is disabled.
    #[error("USB/IP is disabled in the node configuration")]
    UsbIpDisabled,

    /// A USB/IP host or token was rejected by the allow-list.
    #[error("USB/IP authorization denied for host {host:?}: {reason}")]
    UsbIpUnauthorized {
        /// Remote host that was checked.
        host: String,
        /// Why it was denied (never includes the supplied token).
        reason: String,
    },

    /// USB/IP TLS was requested but this crate cannot terminate it.
    #[error("USB/IP TLS is unavailable: {reason}")]
    UsbIpTlsUnsupported {
        /// Why TLS cannot be provided in-process.
        reason: String,
    },

    /// An operation needs the kernel `vhci_hcd`/`usbip-host` data path.
    #[error("USB/IP {operation} requires the kernel vhci_hcd/usbip-host path: {reason}")]
    UsbIpKernelRequired {
        /// Operation that needs the kernel (`attach`, `detach`, `export`, ...).
        operation: &'static str,
        /// Explicit reason/remediation.
        reason: String,
    },

    /// The in-process USB/IP daemon has not been started.
    #[error("USB/IP daemon is not running")]
    UsbIpDaemonStopped,

    /// The in-process USB/IP daemon is at its bounded connection limit.
    #[error("USB/IP daemon connection limit of {limit} reached")]
    UsbIpConnectionLimit {
        /// Configured limit.
        limit: usize,
    },

    /// A `usbip`/NBD response could not be parsed.
    #[error("invalid {protocol} response: {reason}")]
    Protocol {
        /// Protocol name (`usbip`, `nbd`).
        protocol: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// A ConfigFS directory or attribute operation failed.
    #[error("ConfigFS {operation} failed at {path}: {source}")]
    ConfigFs {
        /// Operation that failed.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// A non-ConfigFS filesystem operation failed.
    #[error("I/O {operation} failed at {path}: {source}")]
    Io {
        /// Operation that failed.
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// The executable needed by a subprocess controller is absent.
    #[error("cannot run {command:?}: executable not found; {remediation}")]
    MissingCommand {
        /// Program name.
        command: String,
        /// Operator remediation.
        remediation: String,
    },

    /// A subprocess could not be spawned.
    #[error("failed to run {command:?}: {source}")]
    Spawn {
        /// Rendered command for diagnostics.
        command: String,
        /// Underlying spawn error.
        #[source]
        source: io::Error,
    },

    /// A subprocess exited non-zero.
    #[error("command {command:?} failed with status {status}: {stderr}")]
    CommandFailed {
        /// Rendered command for diagnostics.
        command: String,
        /// Exit status (`-1` when terminated by a signal).
        status: i32,
        /// Trimmed standard error.
        stderr: String,
    },

    /// A FunctionFS function has no ready userspace daemon.
    #[error("FunctionFS {instance:?} is not ready: {reason}")]
    UnsupportedFunctionFs {
        /// Function instance.
        instance: String,
        /// Explicit readiness reason.
        reason: String,
    },

    /// A FunctionFS mount directory is absent or is not a directory.
    #[error("FunctionFS mount {0} does not exist or is not a directory")]
    MissingFunctionFsMount(PathBuf),

    /// A FunctionFS mount directory has no `ep0` file.
    #[error("FunctionFS mount {0} has no ep0 file")]
    MissingFunctionFsEp0(PathBuf),

    /// A declared FunctionFS endpoint file is absent.
    #[error("FunctionFS endpoint file {0} does not exist")]
    MissingFunctionFsEndpoint(PathBuf),

    /// FunctionFS descriptor bytes are malformed, misaligned, or truncated.
    #[error("invalid FunctionFS descriptors: {reason}")]
    InvalidFunctionFsDescriptor {
        /// Why the descriptor set was rejected.
        reason: String,
    },

    /// A FunctionFS control request is not modelled.
    #[error(
        "unsupported FunctionFS control request bmRequestType=0x{bm_request_type:02x} \
         bRequest=0x{b_request:02x}: {reason}"
    )]
    UnsupportedFunctionFsControl {
        /// `bmRequestType` byte.
        bm_request_type: u8,
        /// `bRequest` byte.
        b_request: u8,
        /// Why the request was rejected.
        reason: String,
    },

    /// A FunctionFS daemon state transition is invalid.
    #[error("invalid FunctionFS daemon state: {reason}")]
    InvalidFunctionFsState {
        /// Why the transition was rejected.
        reason: String,
    },

    /// A FunctionFS daemon did not report readiness before the deadline.
    #[error(
        "FunctionFS {instance:?} did not become ready within {timeout_ms} ms \
         (last observed state: {readiness})"
    )]
    FunctionFsReadinessTimeout {
        /// Function instance.
        instance: String,
        /// Deadline in milliseconds.
        timeout_ms: u64,
        /// Last marker/daemon readiness state.
        readiness: &'static str,
    },

    /// A FunctionFS mount, unmount, or lifecycle helper failed.
    #[error("FunctionFS {operation} failed at {path}: {reason}")]
    FunctionFsLifecycle {
        /// Operation that failed (`mount`, `unmount`, `prepare`, ...).
        operation: &'static str,
        /// Path involved.
        path: PathBuf,
        /// Human-readable reason.
        reason: String,
    },

    /// A write-overlay operation would exceed image or allocation bounds.
    #[error("write overlay bounds exceeded: {reason}")]
    OverlayBounds {
        /// Why the operation was rejected.
        reason: String,
    },

    /// An overlay persistence file is malformed or does not match its base.
    #[error("invalid write overlay {path}: {reason}")]
    OverlayCorrupt {
        /// Persistence file involved.
        path: PathBuf,
        /// Why the file was rejected.
        reason: String,
    },

    /// A bounded FunctionFS buffer would be exceeded.
    #[error("FunctionFS {context} exceeds {limit} bytes (requested {requested})")]
    FunctionFsBuffer {
        /// Buffer purpose (`ep0 write`, `endpoint write`, ...).
        context: &'static str,
        /// Maximum permitted bytes.
        limit: usize,
        /// Requested bytes.
        requested: usize,
    },

    /// FunctionFS `ep0` I/O failed.
    #[error("FunctionFS ep0 {operation} failed at {path}: {source}")]
    FunctionFsEp0Io {
        /// Operation that failed (`open`, `write`, `read`).
        operation: &'static str,
        /// `ep0` path, or a synthetic marker for in-memory writers.
        path: PathBuf,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// A FunctionFS data-endpoint transfer failed.
    #[error("FunctionFS endpoint 0x{address:02x} {operation} failed: {source}")]
    FunctionFsEndpointIo {
        /// Endpoint address.
        address: u8,
        /// Operation that failed (`read`, `write`, `flush`).
        operation: &'static str,
        /// Underlying I/O error.
        #[source]
        source: io::Error,
    },

    /// No ALSA/USB audio bridge is available for a UAC2 function.
    #[error("UAC2 audio bridge for {instance:?} is not implemented")]
    UnsupportedUac2Bridge {
        /// Function instance.
        instance: String,
    },

    /// A UAC2 direction has no isochronous endpoint or no audio path.
    #[error("UAC2 bridge for {instance:?} has no {path} path")]
    Uac2MissingPath {
        /// Function instance.
        instance: String,
        /// `playback`, `capture`, or `endpoint`.
        path: &'static str,
    },

    /// A UAC2 PCM format or buffer does not match the configured format.
    #[error("UAC2 format error: {reason}")]
    Uac2Format {
        /// Why the format or sample buffer was rejected.
        reason: String,
    },

    /// A UAC2 isochronous endpoint operation failed or is invalid.
    #[error("UAC2 endpoint 0x{address:02x} {operation} failed: {reason}")]
    Uac2Endpoint {
        /// Endpoint address.
        address: u8,
        /// Operation that failed (`open`, `read`, `write`, `flush`, ...).
        operation: &'static str,
        /// Human-readable reason.
        reason: String,
    },

    /// A bounded UAC2 buffer would be exceeded.
    #[error("UAC2 {buffer} overflow: limit {limit} bytes, requested {requested}")]
    Uac2Overflow {
        /// Buffer purpose (`ring`, `endpoint read`, ...).
        buffer: &'static str,
        /// Maximum permitted bytes.
        limit: usize,
        /// Requested bytes.
        requested: usize,
    },

    /// A UAC2 bridge operation requires a started bridge.
    #[error("UAC2 bridge for {instance:?} is not running")]
    Uac2NotRunning {
        /// Function instance.
        instance: String,
    },

    /// The operation is not available on this platform.
    #[error("operation is not supported on this platform")]
    UnsupportedPlatform,
}

impl UsbError {
    /// Stable machine-readable kind for API responses and logs.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::InvalidConfig(_) | Self::InvalidField { .. } => "invalid-config",
            Self::MissingGadget(_) => "missing-gadget",
            Self::GadgetExists(_) => "gadget-exists",
            Self::MissingFunction { .. } => "missing-function",
            Self::AlreadyBound { .. } => "already-bound",
            Self::MissingRoot(_) => "missing-configfs-root",
            Self::RootNotDirectory(_) => "configfs-root-not-directory",
            Self::MediaNotFound(_) => "media-not-found",
            Self::MediaNotFile(_) => "media-not-file",
            Self::ReadOnlyMedia(_) => "read-only-media",
            Self::InvalidMedia { .. } => "invalid-media",
            Self::UnsupportedMediaFormat { .. } => "unsupported-media-format",
            Self::InvalidUsbIp { .. } => "invalid-usbip",
            Self::UsbIpDisabled => "usbip-disabled",
            Self::UsbIpUnauthorized { .. } => "usbip-unauthorized",
            Self::UsbIpTlsUnsupported { .. } => "usbip-tls-unsupported",
            Self::UsbIpKernelRequired { .. } => "usbip-kernel-required",
            Self::UsbIpDaemonStopped => "usbip-daemon-stopped",
            Self::UsbIpConnectionLimit { .. } => "usbip-connection-limit",
            Self::Protocol { .. } => "protocol-error",
            Self::ConfigFs { .. } => "configfs-io",
            Self::Io { .. } => "io",
            Self::MissingCommand { .. } => "missing-command",
            Self::Spawn { .. } => "spawn",
            Self::CommandFailed { .. } => "command-failed",
            Self::UnsupportedFunctionFs { .. } => "unsupported-functionfs",
            Self::OverlayBounds { .. } => "overlay-bounds",
            Self::OverlayCorrupt { .. } => "overlay-corrupt",
            Self::MissingFunctionFsMount(_) => "missing-functionfs-mount",
            Self::MissingFunctionFsEp0(_) => "missing-functionfs-ep0",
            Self::MissingFunctionFsEndpoint(_) => "missing-functionfs-endpoint",
            Self::InvalidFunctionFsDescriptor { .. } => "invalid-functionfs-descriptor",
            Self::UnsupportedFunctionFsControl { .. } => "unsupported-functionfs-control",
            Self::InvalidFunctionFsState { .. } => "invalid-functionfs-state",
            Self::FunctionFsReadinessTimeout { .. } => "functionfs-readiness-timeout",
            Self::FunctionFsLifecycle { .. } => "functionfs-lifecycle",
            Self::FunctionFsBuffer { .. } => "functionfs-buffer-too-large",
            Self::FunctionFsEp0Io { .. } => "functionfs-ep0-io",
            Self::FunctionFsEndpointIo { .. } => "functionfs-endpoint-io",
            Self::UnsupportedUac2Bridge { .. } => "unsupported-uac2-bridge",
            Self::Uac2MissingPath { .. } => "uac2-missing-path",
            Self::Uac2Format { .. } => "uac2-format",
            Self::Uac2Endpoint { .. } => "uac2-endpoint",
            Self::Uac2Overflow { .. } => "uac2-overflow",
            Self::Uac2NotRunning { .. } => "uac2-not-running",
            Self::UnsupportedPlatform => "unsupported-platform",
        }
    }
}

pub(crate) fn configfs_error(
    operation: &'static str,
    path: PathBuf,
    source: io::Error,
) -> UsbError {
    UsbError::ConfigFs {
        operation,
        path,
        source,
    }
}

pub(crate) fn io_error(operation: &'static str, path: PathBuf, source: io::Error) -> UsbError {
    UsbError::Io {
        operation,
        path,
        source,
    }
}
