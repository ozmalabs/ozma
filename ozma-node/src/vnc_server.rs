// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! A small, honest RFB 3.8 (VNC) server for controller screen consumers.
//!
//! The Python controller reads a node's `vnc_host`/`vnc_port` for screen reads,
//! OCR, agent vision, and testbench fallback. Before this module the Rust node
//! only *consumed* an external QEMU VNC endpoint with `--vnc-host`/`--vnc-port`
//! and never served VNC.
//!
//! This server is deliberately minimal:
//!
//! * RFB 3.3, 3.7, and 3.8 clients are accepted; the server advertises 3.8.
//! * Security type **None** on loopback by default. An optional password file
//!   enables standard RFB VNC authentication (security type 2, DES
//!   challenge/response). A non-loopback bind is refused unless a password is
//!   configured (preferred) or the operator explicitly set
//!   `--vnc-allow-unauthenticated`.
//! * The transport is still plain TCP: VNC authentication proves knowledge of
//!   a password, but it does **not** encrypt the session. There is no TLS and
//!   no VeNCrypt.
//! * Exactly one capture registry source feeds the framebuffer. Frames are
//!   copied into the last published snapshot; a slow client gets the newest
//!   frame, and intermediate frames are counted as coalesced, never queued
//!   without bound.
//! * The only encoding is Raw. `SetEncodings` is parsed and the unsupported
//!   entries are ignored; the server never claims Hextile, ZRLE, or another
//!   encoding it does not implement. Unsupported pixel formats close the
//!   connection, because RFB has no typed per-message error and silently
//!   continuing would lie about the format the client asked for.
//! * Key and pointer events go to the node's existing [`InputSink`]. Unknown
//!   keysyms are dropped with a diagnostic. Every disconnect calls
//!   `release_all` so a vanished viewer cannot leave a guest key or button held.
//!
//! The controller sees the real bound endpoint only while the listener is
//! running. [`start_if_configured`] does not invent a listener: with no capture
//! source it returns [`VncServerError::NoCaptureSource`], and the caller keeps
//! the registration state at `not-configured`.

use std::fmt;
use std::io;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use rand::rngs::OsRng;
use rand::RngCore;

use bytes::Bytes;
use ozma_capture::CaptureRegistry;
use ozma_input::{InputSink, SharedSink};
use ozma_media::{CaptureSource, Frame, FrameFormat, MediaError, PixelFormat};
use ozma_protocol::input::{InputEvent, PointerButton};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;
use tokio_util::sync::CancellationToken;
use tracing::{debug, info, warn};

/// Default bind address. VNC auth is optional, so the default stays on
/// loopback.
pub const DEFAULT_VNC_BIND: &str = "127.0.0.1";
/// Default port (`0` asks the OS for a free port).
pub const DEFAULT_VNC_SERVER_PORT: u16 = 0;
/// Default maximum concurrent viewers.
pub const DEFAULT_MAX_CLIENTS: usize = 4;
/// Upper bound accepted for [`VncServerConfig::max_clients`].
pub const MAX_CLIENTS_LIMIT: usize = 32;
/// Largest RFB VNC-authentication password accepted. Classic VNC auth uses a
/// single DES key, so a longer password cannot contribute any bits.
pub const MAX_VNC_PASSWORD_BYTES: usize = 8;
/// Largest password file read before parsing. This is a read guard only; the
/// parsed password still has to fit in [`MAX_VNC_PASSWORD_BYTES`].
pub const MAX_VNC_PASSWORD_FILE_BYTES: u64 = 4096;
/// Largest accepted idle timeout (24 hours).
pub const MAX_IDLE_TIMEOUT_SECONDS: u64 = 86_400;
/// RFB security type 1: no authentication.
pub const SECURITY_TYPE_NONE: u8 = 1;
/// RFB security type 2: VNC authentication (DES challenge/response).
pub const SECURITY_TYPE_VNC_AUTH: u8 = 2;
/// Largest framebuffer accepted by the Raw encoder (16 megapixels = 64 MiB
/// at four bytes per pixel). 4K fits; 8K is refused rather than allocating an
/// unbounded update.
pub const MAX_FRAMEBUFFER_PIXELS: u64 = 16 * 1024 * 1024;
/// Largest single Raw frame copied from the capture source, in bytes.
pub const MAX_FRAME_BYTES: usize = (MAX_FRAMEBUFFER_PIXELS as usize) * 4;
/// RFB desktop-name limit in bytes.
pub const MAX_NAME_BYTES: usize = 255;
/// Largest `ClientCutText` payload accepted (and ignored) before the connection
/// is closed instead of trusting an unbounded length field.
pub const MAX_CUT_TEXT_BYTES: u32 = 1024 * 1024;
/// Maximum entries accepted in one `SetEncodings` message.
pub const MAX_ENCODINGS: usize = 1024;
/// Maximum pending framebuffer rectangles merged for one client update.
const MAX_PENDING_RECTS: usize = 8;
/// Command channel bound. A flood of update requests is back-pressured.
const CLIENT_COMMAND_QUEUE: usize = 64;
/// RFB Raw encoding number.
pub const ENCODING_RAW: i32 = 0;
/// How long a viewer may take to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one framebuffer update write may block before the viewer is closed.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Cooperative shutdown window before the listener and capture pump are
/// aborted. This is enough for `CaptureSource::stop` to run.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Configuration for the RFB listener.
#[derive(Clone)]
pub struct VncServerConfig {
    /// Address to bind. Non-loopback is allowed only when
    /// `allow_non_loopback` is true.
    pub bind: IpAddr,
    /// TCP port; `0` lets the OS choose one.
    pub port: u16,
    /// Desktop name sent in `ServerInit`.
    pub name: String,
    /// Explicit opt-in required to bind a non-loopback address.
    pub allow_non_loopback: bool,
    /// Optional RFB security type 2 password. `None` offers security type 1
    /// (None) and is safe only on loopback unless the operator explicitly
    /// overrode the bind policy.
    pub password: Option<VncPassword>,
    /// Explicit opt-in for a non-loopback bind **without** a password. This is
    /// deliberately separate from `allow_non_loopback`, which only says the
    /// caller meant to widen the bind address.
    pub allow_unauthenticated: bool,
    /// Maximum concurrent viewers (1..=[`MAX_CLIENTS_LIMIT`]).
    pub max_clients: usize,
    /// Close a viewer that sends no client message for this long. `None`
    /// (or a zero duration) disables the idle timeout.
    pub idle_timeout: Option<Duration>,
    /// Per-update write timeout.
    pub write_timeout: Duration,
    /// Handshake timeout.
    pub handshake_timeout: Duration,
}

impl fmt::Debug for VncServerConfig {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VncServerConfig")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("name", &self.name)
            .field("allow_non_loopback", &self.allow_non_loopback)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .field("max_clients", &self.max_clients)
            .field("idle_timeout", &self.idle_timeout)
            .field("write_timeout", &self.write_timeout)
            .field("handshake_timeout", &self.handshake_timeout)
            .finish()
    }
}

impl Default for VncServerConfig {
    fn default() -> Self {
        Self {
            bind: DEFAULT_VNC_BIND
                .parse()
                .expect("the default VNC bind address is valid"),
            port: DEFAULT_VNC_SERVER_PORT,
            name: "Ozma node".to_owned(),
            allow_non_loopback: false,
            password: None,
            allow_unauthenticated: false,
            max_clients: DEFAULT_MAX_CLIENTS,
            idle_timeout: None,
            write_timeout: WRITE_TIMEOUT,
            handshake_timeout: HANDSHAKE_TIMEOUT,
        }
    }
}

/// A configured RFB VNC-authentication password.
///
/// Only the DES key derived from the password is retained, and neither `Debug`
/// nor any status surface formats it. Classic RFB VNC authentication uses one
/// 8-byte DES key; shorter passwords are zero-padded and longer password files
/// are rejected at load time rather than silently truncated.
#[derive(Clone)]
pub struct VncPassword {
    des_key: [u8; MAX_VNC_PASSWORD_BYTES],
}

impl fmt::Debug for VncPassword {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("VncPassword(<redacted>)")
    }
}

impl Drop for VncPassword {
    fn drop(&mut self) {
        // Best-effort zeroing. The key still lives in process memory while the
        // listener runs; this only avoids leaving a stale copy after teardown.
        self.des_key.fill(0);
    }
}

impl VncPassword {
    /// Build a password from raw bytes.
    ///
    /// The caller is responsible for newline policy. [`VncPassword::from_file`]
    /// removes one trailing newline and nothing else.
    pub fn from_bytes(password: &[u8]) -> Result<Self, VncPasswordError> {
        if password.is_empty() {
            return Err(VncPasswordError::Empty);
        }
        if password.len() > MAX_VNC_PASSWORD_BYTES {
            return Err(VncPasswordError::TooLong {
                len: password.len(),
                max: MAX_VNC_PASSWORD_BYTES,
            });
        }
        let mut padded = [0u8; MAX_VNC_PASSWORD_BYTES];
        padded[..password.len()].copy_from_slice(password);
        // RFB VNC authentication reverses the bits of each password byte
        // before using it as a DES key.
        let mut des_key = [0u8; MAX_VNC_PASSWORD_BYTES];
        for (key_byte, password_byte) in des_key.iter_mut().zip(padded.iter()) {
            *key_byte = password_byte.reverse_bits();
        }
        Ok(Self { des_key })
    }

    /// Read a password file once at startup.
    ///
    /// Exactly one trailing newline is removed (`\n` or `\r\n`); spaces and
    /// other bytes are preserved. On Unix the file must not grant any group or
    /// other permission and no owner execute bit (0600 or stricter), otherwise
    /// startup fails with [`VncPasswordError::Permissions`].
    pub fn from_file(path: &Path) -> Result<Self, VncPasswordError> {
        let metadata = std::fs::metadata(path).map_err(|source| VncPasswordError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        if metadata.len() > MAX_VNC_PASSWORD_FILE_BYTES {
            return Err(VncPasswordError::TooLong {
                len: metadata.len() as usize,
                max: MAX_VNC_PASSWORD_BYTES,
            });
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = metadata.permissions().mode() & 0o777;
            if mode & !0o600 != 0 {
                return Err(VncPasswordError::Permissions {
                    path: path.to_path_buf(),
                    mode,
                });
            }
        }
        let bytes = std::fs::read(path).map_err(|source| VncPasswordError::Read {
            path: path.to_path_buf(),
            source,
        })?;
        let bytes = strip_one_newline(&bytes);
        Self::from_bytes(bytes).map_err(|error| match error {
            VncPasswordError::Empty => VncPasswordError::EmptyAt {
                path: path.to_path_buf(),
            },
            VncPasswordError::TooLong { len, max } => VncPasswordError::TooLongAt {
                path: path.to_path_buf(),
                len,
                max,
            },
            other => other,
        })
    }

    fn key(&self) -> &[u8; MAX_VNC_PASSWORD_BYTES] {
        &self.des_key
    }
}

/// Remove exactly one trailing newline, preserving all other bytes.
fn strip_one_newline(bytes: &[u8]) -> &[u8] {
    if let Some(stripped) = bytes.strip_suffix(b"\r\n") {
        stripped
    } else if let Some(stripped) = bytes.strip_suffix(b"\n") {
        stripped
    } else {
        bytes
    }
}

/// Typed password-file/configuration errors. The password value is never part
/// of any error or `Debug` rendering.
#[derive(Debug, thiserror::Error)]
pub enum VncPasswordError {
    /// The password or file is empty after trimming one trailing newline.
    #[error("VNC password is empty")]
    Empty,
    /// An in-memory password is empty.
    #[error("VNC password file `{path}` is empty after trimming one trailing newline")]
    EmptyAt { path: PathBuf },
    /// An in-memory password is longer than one DES key.
    #[error("VNC password is {len} bytes; RFB VNC authentication accepts at most {max} bytes")]
    TooLong { len: usize, max: usize },
    /// A password file is longer than one DES key.
    #[error(
        "VNC password file `{path}` is {len} bytes; RFB VNC authentication accepts at most {max} bytes"
    )]
    TooLongAt {
        path: PathBuf,
        len: usize,
        max: usize,
    },
    /// The file could not be read.
    #[error("reading VNC password file `{path}` failed: {source}")]
    Read {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    /// The file is readable/writable by group, other, or has an owner execute
    /// bit.
    #[error(
        "VNC password file `{path}` has mode {mode:o}; it must be 0600 or stricter \
         (owner read/write only)"
    )]
    Permissions { path: PathBuf, mode: u32 },
}

impl VncServerConfig {
    /// Build a configuration from an optional CLI bind string.
    ///
    /// Passing `Some(_)` is the explicit opt-in that permits a widened bind.
    pub fn from_bind(
        bind: Option<&str>,
        port: u16,
        name: impl Into<String>,
    ) -> Result<Self, VncServerError> {
        let raw = bind.unwrap_or(DEFAULT_VNC_BIND).trim();
        let address = raw
            .parse::<IpAddr>()
            .map_err(|error| VncServerError::InvalidBind {
                bind: raw.to_owned(),
                reason: error.to_string(),
            })?;
        Ok(Self {
            bind: address,
            port,
            name: name.into(),
            // Any explicit `--vnc-bind` is the operator's opt-in, including an
            // explicit loopback address. Without it only the loopback default
            // is permitted.
            allow_non_loopback: bind.is_some(),
            ..Self::default()
        })
    }

    /// Validate the listener bounds before binding.
    pub fn validate(&self) -> Result<(), VncServerError> {
        if self.name.trim().is_empty() || self.name.len() > MAX_NAME_BYTES {
            return Err(VncServerError::InvalidName {
                max: MAX_NAME_BYTES,
            });
        }
        if self.max_clients == 0 || self.max_clients > MAX_CLIENTS_LIMIT {
            return Err(VncServerError::InvalidClientLimit {
                requested: self.max_clients,
                max: MAX_CLIENTS_LIMIT,
            });
        }
        if let Some(timeout) = self.idle_timeout {
            if timeout.is_zero() || timeout > Duration::from_secs(MAX_IDLE_TIMEOUT_SECONDS) {
                return Err(VncServerError::InvalidIdleTimeout {
                    seconds: timeout.as_secs_f64(),
                    max: MAX_IDLE_TIMEOUT_SECONDS,
                });
            }
        }
        if !self.bind.is_loopback() {
            if !self.allow_non_loopback {
                return Err(VncServerError::WiderBindRefused {
                    bind: self.bind.to_string(),
                });
            }
            if self.password.is_none() && !self.allow_unauthenticated {
                return Err(VncServerError::UnauthenticatedWiderBindRefused {
                    bind: self.bind.to_string(),
                });
            }
        }
        Ok(())
    }
}

/// Errors that prevent the listener from starting truthfully.
#[derive(Debug, thiserror::Error)]
pub enum VncServerError {
    /// `--vnc-server` was requested but discovery found no source to feed it.
    #[error("no capture source is available to feed the VNC server")]
    NoCaptureSource,
    /// The selected source could not be opened.
    #[error("opening capture source `{id}` failed: {message}")]
    CaptureOpen { id: String, message: String },
    /// Only packed RGB24/BGRA sources are encoded as Raw.
    #[error("capture source format {0} is not supported; the VNC server encodes RGB24 or BGRA")]
    UnsupportedSourceFormat(String),
    /// Geometry is zero, out of RFB range, or above the pixel bound.
    #[error(
        "capture source geometry {width}x{height} is invalid or exceeds the \
         {max_pixels}-pixel VNC bound"
    )]
    Geometry {
        width: u32,
        height: u32,
        max_pixels: u64,
    },
    /// A non-loopback bind without an explicit `--vnc-bind`.
    #[error(
        "refusing non-loopback VNC bind `{bind}`: widening the bind requires an \
         explicit --vnc-bind"
    )]
    WiderBindRefused { bind: String },
    /// A non-loopback bind with no password and no explicit override.
    #[error(
        "refusing unauthenticated non-loopback VNC bind `{bind}`: configure \
         --vnc-password-file, or set --vnc-allow-unauthenticated to accept the risk"
    )]
    UnauthenticatedWiderBindRefused { bind: String },
    /// Idle timeout outside the supported range.
    #[error("VNC idle timeout must be between 1 and {max} seconds, got {seconds}")]
    InvalidIdleTimeout { seconds: f64, max: u64 },
    /// The bind string is not an IP address.
    #[error("invalid VNC bind address `{bind}`: {reason}")]
    InvalidBind { bind: String, reason: String },
    /// Empty/oversized desktop name.
    #[error("VNC desktop name must be non-empty and at most {max} bytes")]
    InvalidName { max: usize },
    /// Client cap outside the supported range.
    #[error("VNC max_clients must be between 1 and {max}, got {requested}")]
    InvalidClientLimit { requested: usize, max: usize },
    /// Binding the TCP port failed.
    #[error("binding VNC listener {address}: {source}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: io::Error,
    },
}

/// A running RFB listener and its counters.
pub struct VncServerHandle {
    bound_addr: SocketAddr,
    stats: Arc<VncStats>,
    cancel: CancellationToken,
    task: Option<tokio::task::JoinHandle<()>>,
    /// Cleared as soon as the listener task exits (including capture failure),
    /// so registration can stop advertising a dead port without a restart.
    listening: Arc<std::sync::atomic::AtomicBool>,
    /// Whether the listener offers RFB security type 2.
    auth_enabled: bool,
}

impl fmt::Debug for VncServerHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VncServerHandle")
            .field("bound_addr", &self.bound_addr)
            .field("listening", &self.is_listening())
            .field("auth_enabled", &self.auth_enabled)
            .field("stats", &self.stats.snapshot())
            .finish()
    }
}

impl Drop for VncServerHandle {
    fn drop(&mut self) {
        // A dropped handle must not keep an unauthenticated listener alive.
        // `shutdown` still awaits the task; Drop only requests cancellation.
        self.cancel.cancel();
    }
}

impl VncServerHandle {
    /// The address the OS actually bound (a real port for `--vnc-server-port 0`).
    pub fn bound_addr(&self) -> SocketAddr {
        self.bound_addr
    }

    /// Whether this listener requires RFB security type 2 (VNC password).
    pub fn auth_enabled(&self) -> bool {
        self.auth_enabled
    }

    /// True until [`VncServerHandle::shutdown`] is called or the listener fails.
    pub fn is_listening(&self) -> bool {
        self.listening.load(std::sync::atomic::Ordering::SeqCst)
            && !self.cancel.is_cancelled()
            && self.task.as_ref().is_some_and(|task| !task.is_finished())
    }

    /// Shared liveness flag, for a registration watcher that does not own the
    /// handle.
    pub fn live_flag(&self) -> Arc<std::sync::atomic::AtomicBool> {
        self.listening.clone()
    }

    /// Controller-facing host/port. A wildcard bind is replaced with the
    /// resolved local address because `0.0.0.0` is not connectable.
    pub fn registration_endpoint(&self, local_ip: &str) -> (String, u16) {
        let host = if self.bound_addr.ip().is_unspecified() {
            local_ip.to_owned()
        } else {
            self.bound_addr.ip().to_string()
        };
        (host, self.bound_addr.port())
    }

    /// Current counters.
    pub fn stats(&self) -> VncServerStats {
        self.stats.snapshot()
    }

    /// Stop accepting clients, abort in-flight viewers, release held input, and
    /// wait for the listener task to finish.
    pub async fn shutdown(mut self) {
        self.cancel.cancel();
        self.listening
            .store(false, std::sync::atomic::Ordering::SeqCst);
        if let Some(task) = self.task.take() {
            let _ = task.await;
        }
    }
}

/// Point-in-time diagnostics for a VNC listener.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VncServerStats {
    /// Currently connected viewers.
    pub clients_connected: usize,
    /// Viewers accepted since start.
    pub clients_accepted: u64,
    /// Connections refused by the client cap.
    pub clients_rejected: u64,
    /// Viewers that finished (normally or by protocol/write failure).
    pub clients_disconnected: u64,
    /// Frames published by the capture pump.
    pub frames_published: u64,
    /// Framebuffer updates written to viewers.
    pub updates_sent: u64,
    /// Frames superseded before a viewer could receive them.
    pub frames_coalesced: u64,
    /// Raw payload bytes written (excluding RFB headers).
    pub bytes_sent: u64,
    /// Keysyms the RFB-to-HID table does not know.
    pub unsupported_keys: u64,
    /// Protocol violations (bad version, security, pixel format, message type,
    /// oversized cut text).
    pub protocol_errors: u64,
    /// Capture source failures.
    pub source_errors: u64,
    /// Viewers closed because a framebuffer write exceeded the timeout.
    pub slow_clients: u64,
}

#[derive(Default)]
struct VncStats {
    clients_connected: AtomicUsize,
    clients_accepted: AtomicU64,
    clients_rejected: AtomicU64,
    clients_disconnected: AtomicU64,
    frames_published: AtomicU64,
    updates_sent: AtomicU64,
    frames_coalesced: AtomicU64,
    bytes_sent: AtomicU64,
    unsupported_keys: AtomicU64,
    protocol_errors: AtomicU64,
    source_errors: AtomicU64,
    slow_clients: AtomicU64,
    generation: AtomicU64,
}

impl VncStats {
    fn snapshot(&self) -> VncServerStats {
        VncServerStats {
            clients_connected: self.clients_connected.load(Ordering::Relaxed),
            clients_accepted: self.clients_accepted.load(Ordering::Relaxed),
            clients_rejected: self.clients_rejected.load(Ordering::Relaxed),
            clients_disconnected: self.clients_disconnected.load(Ordering::Relaxed),
            frames_published: self.frames_published.load(Ordering::Relaxed),
            updates_sent: self.updates_sent.load(Ordering::Relaxed),
            frames_coalesced: self.frames_coalesced.load(Ordering::Relaxed),
            bytes_sent: self.bytes_sent.load(Ordering::Relaxed),
            unsupported_keys: self.unsupported_keys.load(Ordering::Relaxed),
            protocol_errors: self.protocol_errors.load(Ordering::Relaxed),
            source_errors: self.source_errors.load(Ordering::Relaxed),
            slow_clients: self.slow_clients.load(Ordering::Relaxed),
        }
    }

    fn try_connect(&self, max: usize) -> bool {
        let mut current = self.clients_connected.load(Ordering::Acquire);
        loop {
            if current >= max {
                return false;
            }
            match self.clients_connected.compare_exchange_weak(
                current,
                current + 1,
                Ordering::AcqRel,
                Ordering::Acquire,
            ) {
                Ok(_) => return true,
                Err(actual) => current = actual,
            }
        }
    }

    fn disconnect(&self) {
        self.clients_connected.fetch_sub(1, Ordering::AcqRel);
        self.clients_disconnected.fetch_add(1, Ordering::Relaxed);
    }

    fn next_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::AcqRel) + 1
    }
}

/// Optional listener settings supplied by `main` from the CLI.
#[derive(Clone, Debug)]
pub struct VncServerOptions {
    /// Explicit bind address. `Some` is the opt-in that permits a widened bind.
    pub bind: Option<String>,
    /// TCP port; `0` asks the OS for a free port.
    pub port: u16,
    /// Password loaded once at startup. `None` offers security type None.
    pub password: Option<VncPassword>,
    /// Maximum concurrent viewers.
    pub max_clients: usize,
    /// Optional idle timeout.
    pub idle_timeout: Option<Duration>,
    /// Explicit opt-in for an unauthenticated non-loopback bind.
    pub allow_unauthenticated: bool,
}

impl Default for VncServerOptions {
    fn default() -> Self {
        Self {
            bind: None,
            port: DEFAULT_VNC_SERVER_PORT,
            password: None,
            max_clients: DEFAULT_MAX_CLIENTS,
            idle_timeout: None,
            allow_unauthenticated: false,
        }
    }
}

/// Start the RFB server only when `enabled` is true and a capture source exists.
///
/// This is the main-node entry point: `enabled` is `--vnc-server`, and
/// `options.bind` being `Some` is the explicit opt-in that allows a
/// non-loopback address. Returning `Ok(None)` means the flag was absent, so no
/// socket was bound and the caller must keep VNC registration at
/// `not-configured`.
pub async fn start_if_configured(
    enabled: bool,
    options: VncServerOptions,
    registry: &CaptureRegistry,
    sink: SharedSink,
    node_name: &str,
) -> Result<Option<VncServerHandle>, VncServerError> {
    if !enabled {
        return Ok(None);
    }

    // `scan` keeps per-provider failures isolated. A V4L2 discovery error must
    // not hide a working PipeWire/X11/QMP source, and vice versa.
    let mut capabilities = Vec::new();
    for scan in registry.scan() {
        if let Some(error) = scan.error {
            debug!(
                kind = ?scan.kind,
                error = %error.message,
                "VNC capture discovery skipped a failing provider"
            );
        }
        capabilities.extend(scan.capabilities);
    }
    let capability = capabilities
        .into_iter()
        .next()
        .ok_or(VncServerError::NoCaptureSource)?;
    let id = capability.id.clone();
    let name = format!("Ozma {node_name} ({id})");
    let mut config = VncServerConfig::from_bind(options.bind.as_deref(), options.port, name)?;
    config.password = options.password;
    config.max_clients = options.max_clients;
    config.idle_timeout = options.idle_timeout;
    config.allow_unauthenticated = options.allow_unauthenticated;

    let source = registry
        .open(&capability)
        .map_err(|error| VncServerError::CaptureOpen {
            id: id.clone(),
            message: error.message,
        })?;
    start(config, source, sink).await.map(Some)
}

/// Bind the listener and start the capture pump and accept loop.
///
/// The caller supplies an already-open source. On any validation or bind error
/// the source is stopped before the error is returned, so a failed start does
/// not leave a device or child process held.
pub async fn start(
    config: VncServerConfig,
    mut source: Box<dyn CaptureSource>,
    sink: SharedSink,
) -> Result<VncServerHandle, VncServerError> {
    if let Err(error) = config.validate() {
        let _ = source.stop().await;
        return Err(error);
    }

    let source_format = source.format();
    if let Err(error) = validate_source_format(source_format) {
        let _ = source.stop().await;
        return Err(error);
    }

    let address = SocketAddr::new(config.bind, config.port);
    let listener = match TcpListener::bind(address).await {
        Ok(listener) => listener,
        Err(error) => {
            let _ = source.stop().await;
            return Err(VncServerError::Bind {
                address,
                source: error,
            });
        }
    };
    let bound_addr = listener
        .local_addr()
        .map_err(|error| VncServerError::Bind {
            address,
            source: error,
        })?;

    let auth_enabled = config.password.is_some();
    let config = Arc::new(config);
    let stats = Arc::new(VncStats::default());
    let cancel = CancellationToken::new();
    let (frames_tx, frames_rx) = watch::channel::<Option<Arc<FrameSnapshot>>>(None);

    let mut tasks = JoinSet::new();
    tasks.spawn(frame_pump(
        source,
        source_format,
        frames_tx,
        cancel.clone(),
        stats.clone(),
    ));
    tasks.spawn(accept_loop(
        listener,
        config,
        source_format,
        sink,
        frames_rx,
        stats.clone(),
        cancel.clone(),
    ));

    let supervisor_cancel = cancel.clone();
    let listening = Arc::new(std::sync::atomic::AtomicBool::new(true));
    let listening_task = listening.clone();
    let task = tokio::spawn(async move {
        tokio::select! {
            _ = supervisor_cancel.cancelled() => {}
            _ = tasks.join_next() => {}
        }
        supervisor_cancel.cancel();
        // Let the accept loop and frame pump observe cancellation so the
        // capture source can call `stop` cleanly. Abort only if a task ignores
        // the cooperative window.
        let drain = async { while tasks.join_next().await.is_some() {} };
        if tokio::time::timeout(SHUTDOWN_GRACE, drain).await.is_err() {
            tasks.shutdown().await;
        }
        listening_task.store(false, std::sync::atomic::Ordering::SeqCst);
    });

    info!(
        bind = %bound_addr,
        loopback = bound_addr.ip().is_loopback(),
        security = if auth_enabled { "vnc-auth" } else { "none" },
        encrypted = false,
        encoding = "raw",
        "RFB 3.8 VNC server listening (transport is plain TCP; VNC auth is not encryption)"
    );

    Ok(VncServerHandle {
        bound_addr,
        stats,
        cancel,
        task: Some(task),
        listening,
        auth_enabled,
    })
}

fn validate_source_format(format: FrameFormat) -> Result<(), VncServerError> {
    if !matches!(format.pixel_format, PixelFormat::Rgb24 | PixelFormat::Bgra) {
        return Err(VncServerError::UnsupportedSourceFormat(format.to_string()));
    }
    // QMP and D-Bus display sources report their geometry only after the first
    // frame. Keep the listener startable; ServerInit waits for a real frame
    // instead of inventing a size.
    if format.width == 0 || format.height == 0 {
        return Ok(());
    }
    validate_concrete_geometry(format)
}

fn validate_concrete_geometry(format: FrameFormat) -> Result<(), VncServerError> {
    let pixels = u64::from(format.width) * u64::from(format.height);
    if format.width == 0
        || format.height == 0
        || format.width > u32::from(u16::MAX)
        || format.height > u32::from(u16::MAX)
        || pixels > MAX_FRAMEBUFFER_PIXELS
        || format
            .pixel_format
            .nominal_len(format.width, format.height)
            .map_or(true, |len| len > MAX_FRAME_BYTES)
    {
        return Err(VncServerError::Geometry {
            width: format.width,
            height: format.height,
            max_pixels: MAX_FRAMEBUFFER_PIXELS,
        });
    }
    Ok(())
}

/// Last complete frame published by the capture pump.
#[derive(Debug)]
struct FrameSnapshot {
    generation: u64,
    format: FrameFormat,
    data: Bytes,
}

async fn frame_pump(
    mut source: Box<dyn CaptureSource>,
    mut expected: FrameFormat,
    frames: watch::Sender<Option<Arc<FrameSnapshot>>>,
    cancel: CancellationToken,
    stats: Arc<VncStats>,
) {
    loop {
        let result = tokio::select! {
            _ = cancel.cancelled() => break,
            result = source.next_frame() => result,
        };

        match result {
            Ok(frame) => {
                if expected.width == 0 || expected.height == 0 {
                    let actual = frame.format();
                    if actual.pixel_format != expected.pixel_format {
                        stats.source_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(
                            expected = %expected.pixel_format,
                            actual = %actual.pixel_format,
                            "dropping first VNC frame whose pixel format differs from the source"
                        );
                        continue;
                    }
                    if let Err(error) = validate_concrete_geometry(actual) {
                        stats.source_errors.fetch_add(1, Ordering::Relaxed);
                        warn!(error = %error, "first VNC frame has unusable geometry");
                        continue;
                    }
                    info!(
                        width = actual.width,
                        height = actual.height,
                        "VNC capture source published its first frame; ServerInit geometry fixed"
                    );
                    expected = actual;
                }
                if let Some(snapshot) = snapshot_frame(frame, expected, &stats) {
                    stats.frames_published.fetch_add(1, Ordering::Relaxed);
                    frames.send_replace(Some(snapshot));
                }
            }
            Err(MediaError::EndOfStream) => {
                info!("VNC capture source ended; the VNC listener is stopping");
                break;
            }
            Err(error) => {
                stats.source_errors.fetch_add(1, Ordering::Relaxed);
                warn!(error = %error, "VNC capture source failed; the VNC listener is stopping");
                break;
            }
        }
    }
    let _ = source.stop().await;
}

fn snapshot_frame(
    frame: Frame,
    expected: FrameFormat,
    stats: &VncStats,
) -> Option<Arc<FrameSnapshot>> {
    if frame.format() != expected {
        warn!(
            expected = %expected,
            actual = %frame.format(),
            "dropping VNC frame whose format changed after ServerInit"
        );
        stats.source_errors.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let nominal = expected
        .pixel_format
        .nominal_len(expected.width, expected.height)
        .unwrap_or(0);
    if nominal == 0 || frame.len() != nominal || frame.len() > MAX_FRAME_BYTES {
        warn!(
            expected_bytes = nominal,
            actual_bytes = frame.len(),
            "dropping VNC frame with an unexpected size"
        );
        stats.source_errors.fetch_add(1, Ordering::Relaxed);
        return None;
    }
    Some(Arc::new(FrameSnapshot {
        generation: stats.next_generation(),
        format: expected,
        data: frame.into_data(),
    }))
}

async fn accept_loop(
    listener: TcpListener,
    config: Arc<VncServerConfig>,
    geometry: FrameFormat,
    sink: SharedSink,
    frames: watch::Receiver<Option<Arc<FrameSnapshot>>>,
    stats: Arc<VncStats>,
    cancel: CancellationToken,
) {
    let known_geometry = (geometry.width != 0 && geometry.height != 0).then_some(geometry);
    let mut clients = JoinSet::new();
    loop {
        tokio::select! {
            _ = cancel.cancelled() => break,
            Some(_) = clients.join_next(), if !clients.is_empty() => {}
            accepted = listener.accept() => {
                let (stream, peer) = match accepted {
                    Ok(pair) => pair,
                    Err(error) => {
                        warn!(error = %error, "VNC accept failed");
                        continue;
                    }
                };
                if !stats.try_connect(config.max_clients) {
                    stats.clients_rejected.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        peer = %peer,
                        max_clients = config.max_clients,
                        "VNC client refused: concurrent client cap reached"
                    );
                    drop(stream);
                    continue;
                }
                stats.clients_accepted.fetch_add(1, Ordering::Relaxed);
                let client_id = stats.clients_accepted.load(Ordering::Relaxed);
                let write_timeout = config.write_timeout;
                let handshake_timeout = config.handshake_timeout;
                let idle_timeout = config.idle_timeout;
                let password = config.password.clone();
                let name = config.name.clone();
                let frames = frames.clone();
                let input_active = Arc::new(AtomicBool::new(false));
                let guard = ClientGuard {
                    stats: stats.clone(),
                    sink: sink.clone(),
                    input_active: input_active.clone(),
                };
                let client_sink = sink.clone();
                let client_stats = stats.clone();

                clients.spawn(async move {
                    let _guard = guard;
                    handle_client(
                        stream,
                        peer,
                        client_id,
                        name,
                        known_geometry,
                        client_sink,
                        client_stats,
                        input_active,
                        frames,
                        write_timeout,
                        handshake_timeout,
                        idle_timeout,
                        password,
                    )
                    .await;
                });
            }
        }
    }
    // Abort any viewer still attached so shutdown releases input promptly.
    clients.shutdown().await;
}

struct ClientGuard {
    stats: Arc<VncStats>,
    sink: SharedSink,
    input_active: Arc<AtomicBool>,
}

impl Drop for ClientGuard {
    fn drop(&mut self) {
        self.stats.disconnect();
        // A viewer that never sent a key/pointer event cannot have left held
        // input behind. Skipping release_all here keeps a failed handshake from
        // disturbing an operator already using another input path.
        if self.input_active.load(Ordering::Acquire) {
            if let Err(error) = self.sink.release_all() {
                debug!(error = %error, "release_all after VNC disconnect failed");
            }
        }
    }
}

struct ClientContext {
    id: u64,
    width: u32,
    height: u32,
    sink: SharedSink,
    stats: Arc<VncStats>,
    input_active: Arc<AtomicBool>,
    pixel: Mutex<ClientPixelFormat>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClientPixelFormat {
    big_endian: bool,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
}

impl Default for ClientPixelFormat {
    fn default() -> Self {
        Self {
            big_endian: false,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }
}

impl fmt::Display for ClientPixelFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "32bpp true-colour {} r{} g{} b{}",
            if self.big_endian {
                "big-endian"
            } else {
                "little-endian"
            },
            self.red_shift,
            self.green_shift,
            self.blue_shift
        )
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct Rect {
    x: u16,
    y: u16,
    width: u16,
    height: u16,
}

#[derive(Debug)]
enum ClientCommand {
    Update { incremental: bool, rects: Vec<Rect> },
}

impl ClientCommand {
    fn into_pending(self) -> PendingUpdate {
        match self {
            Self::Update { incremental, rects } => PendingUpdate { incremental, rects },
        }
    }
}

#[derive(Debug)]
struct PendingUpdate {
    incremental: bool,
    rects: Vec<Rect>,
}

impl PendingUpdate {
    fn merge(&mut self, other: ClientCommand) {
        let ClientCommand::Update { incremental, rects } = other;
        self.incremental = self.incremental && incremental;
        for rect in rects {
            if self.rects.len() < MAX_PENDING_RECTS {
                self.rects.push(rect);
            } else if let Some(last) = self.rects.last_mut() {
                *last = union_rect(*last, rect);
            }
        }
    }
}

fn union_rect(a: Rect, b: Rect) -> Rect {
    let x = a.x.min(b.x);
    let y = a.y.min(b.y);
    let right = (u32::from(a.x) + u32::from(a.width)).max(u32::from(b.x) + u32::from(b.width));
    let bottom = (u32::from(a.y) + u32::from(a.height)).max(u32::from(b.y) + u32::from(b.height));
    Rect {
        x,
        y,
        width: (right - u32::from(x)).min(u32::from(u16::MAX)) as u16,
        height: (bottom - u32::from(y)).min(u32::from(u16::MAX)) as u16,
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_client(
    mut stream: TcpStream,
    peer: SocketAddr,
    client_id: u64,
    name: String,
    known_geometry: Option<FrameFormat>,
    sink: SharedSink,
    stats: Arc<VncStats>,
    input_active: Arc<AtomicBool>,
    mut frames: watch::Receiver<Option<Arc<FrameSnapshot>>>,
    write_timeout: Duration,
    handshake_timeout: Duration,
    idle_timeout: Option<Duration>,
    password: Option<VncPassword>,
) {
    let handshake = tokio::time::timeout(
        handshake_timeout,
        handshake(
            &mut stream,
            &name,
            known_geometry,
            &mut frames,
            password.as_ref(),
        ),
    )
    .await;
    let (shared, geometry) = match handshake {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => {
            stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
            warn!(client = client_id, peer = %peer, error = %error, "VNC handshake failed");
            return;
        }
        Err(_) => {
            stats.protocol_errors.fetch_add(1, Ordering::Relaxed);
            warn!(client = client_id, peer = %peer, "VNC handshake timed out");
            return;
        }
    };
    debug!(client = client_id, peer = %peer, shared, "VNC viewer connected");

    let context = Arc::new(ClientContext {
        id: client_id,
        width: geometry.width,
        height: geometry.height,
        sink,
        stats,
        input_active,
        pixel: Mutex::new(ClientPixelFormat::default()),
    });
    let (read, write) = stream.into_split();
    let (commands_tx, commands_rx) = mpsc::channel(CLIENT_COMMAND_QUEUE);
    let reader = client_read_loop(read, context.clone(), commands_tx, idle_timeout);
    let writer = client_write_loop(write, commands_rx, frames, context.clone(), write_timeout);

    tokio::select! {
        _ = reader => {
            debug!(client = context.id, "VNC viewer read side closed");
        }
        result = writer => {
            if let Err(error) = result {
                debug!(client = context.id, error = %error, "VNC viewer write side closed");
            }
        }
    }
    debug!(client = context.id, peer = %peer, "VNC viewer disconnected");
}

async fn handshake(
    stream: &mut TcpStream,
    name: &str,
    known_geometry: Option<FrameFormat>,
    frames: &mut watch::Receiver<Option<Arc<FrameSnapshot>>>,
    password: Option<&VncPassword>,
) -> Result<(bool, FrameFormat), ProtocolError> {
    stream
        .write_all(b"RFB 003.008\n")
        .await
        .map_err(ProtocolError::io)?;

    let mut version = [0u8; 12];
    stream
        .read_exact(&mut version)
        .await
        .map_err(ProtocolError::io)?;
    let minor = parse_version(&version)?;

    // Offer exactly one type: password auth when configured, None otherwise.
    // Never offer both, because a client that picks None would bypass the
    // configured password.
    let offered = if password.is_some() {
        SECURITY_TYPE_VNC_AUTH
    } else {
        SECURITY_TYPE_NONE
    };

    match minor {
        3 => {
            // RFB 3.3: the server sends one 32-bit security type and the
            // client does not answer with a selection.
            stream
                .write_all(&u32::from(offered).to_be_bytes())
                .await
                .map_err(ProtocolError::io)?;
            if offered == SECURITY_TYPE_VNC_AUTH {
                authenticate_vnc(stream, minor, password).await?;
            }
        }
        7 | 8 => {
            // RFB 3.7/3.8: list of security types, then the client's choice.
            stream
                .write_all(&[1, offered])
                .await
                .map_err(ProtocolError::io)?;
            let mut selected = [0u8; 1];
            stream
                .read_exact(&mut selected)
                .await
                .map_err(ProtocolError::io)?;
            if selected[0] != offered {
                let reason = if password.is_some() {
                    "Ozma VNC requires the configured password"
                } else {
                    "Ozma VNC accepts only security type None"
                };
                send_security_failure(stream, minor, reason.as_bytes()).await?;
                return Err(ProtocolError::SecurityType(selected[0]));
            }
            if offered == SECURITY_TYPE_VNC_AUTH {
                authenticate_vnc(stream, minor, password).await?;
            } else {
                // SecurityResult success for None.
                stream
                    .write_all(&0u32.to_be_bytes())
                    .await
                    .map_err(ProtocolError::io)?;
            }
        }
        other => return Err(ProtocolError::Version(u16::from(other))),
    }

    let mut shared = [0u8; 1];
    stream
        .read_exact(&mut shared)
        .await
        .map_err(ProtocolError::io)?;

    // Dynamic sources (QMP/D-Bus) know their geometry only after the first
    // frame. The client has already completed version/security/ClientInit, so
    // it waits normally for ServerInit instead of receiving a fabricated size.
    let geometry = match known_geometry {
        Some(geometry) => geometry,
        None => wait_for_first_geometry(frames).await?,
    };

    // ServerInit: geometry, default 32-bit true-colour RGB order, name.
    let name = name.as_bytes();
    let mut init_message = Vec::with_capacity(24 + name.len());
    init_message.extend_from_slice(&(geometry.width as u16).to_be_bytes());
    init_message.extend_from_slice(&(geometry.height as u16).to_be_bytes());
    init_message.extend_from_slice(&[32, 24, 0, 1]); // bpp, depth, big-endian, true-colour
    init_message.extend_from_slice(&255u16.to_be_bytes()); // red-max
    init_message.extend_from_slice(&255u16.to_be_bytes()); // green-max
    init_message.extend_from_slice(&255u16.to_be_bytes()); // blue-max
    init_message.extend_from_slice(&[16, 8, 0]); // red, green, blue shifts
    init_message.extend_from_slice(&[0, 0, 0]); // padding
    init_message.extend_from_slice(&(name.len() as u32).to_be_bytes());
    init_message.extend_from_slice(name);
    stream
        .write_all(&init_message)
        .await
        .map_err(ProtocolError::io)?;

    Ok((shared[0] != 0, geometry))
}

/// Write an RFB `SecurityResult` failure. 3.8 adds a reason string; 3.7 and
/// 3.3 send only the 32-bit status.
async fn send_security_failure(
    stream: &mut TcpStream,
    minor: u8,
    reason: &[u8],
) -> Result<(), ProtocolError> {
    stream
        .write_all(&1u32.to_be_bytes())
        .await
        .map_err(ProtocolError::io)?;
    if minor >= 8 {
        stream
            .write_all(&(reason.len() as u32).to_be_bytes())
            .await
            .map_err(ProtocolError::io)?;
        stream.write_all(reason).await.map_err(ProtocolError::io)?;
    }
    Ok(())
}

/// Perform standard RFB VNC authentication: 16-byte challenge, 16-byte DES
/// response, one `SecurityResult`. A failure closes the connection; there is
/// deliberately no retry loop.
async fn authenticate_vnc(
    stream: &mut TcpStream,
    minor: u8,
    password: Option<&VncPassword>,
) -> Result<(), ProtocolError> {
    let Some(password) = password else {
        // Unreachable when the offered list is built correctly, but failing
        // closed is better than treating a missing key as success.
        send_security_failure(stream, minor, b"VNC password is not configured").await?;
        return Err(ProtocolError::AuthenticationFailed);
    };
    let mut challenge = [0u8; 16];
    OsRng.fill_bytes(&mut challenge);
    stream
        .write_all(&challenge)
        .await
        .map_err(ProtocolError::io)?;

    let mut response = [0u8; 16];
    stream
        .read_exact(&mut response)
        .await
        .map_err(ProtocolError::io)?;
    let expected = vnc_des::response(password.key(), &challenge);
    if constant_time_eq(&expected, &response) {
        stream
            .write_all(&0u32.to_be_bytes())
            .await
            .map_err(ProtocolError::io)?;
        Ok(())
    } else {
        send_security_failure(stream, minor, b"VNC authentication failed").await?;
        Err(ProtocolError::AuthenticationFailed)
    }
}

/// Compare two equal-length byte slices without an early exit on mismatch.
fn constant_time_eq(left: &[u8], right: &[u8]) -> bool {
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

/// Minimal DES-ECB primitives needed by RFB VNC authentication.
///
/// Only two 8-byte blocks are ever encrypted, once per login attempt. This is
/// not a general-purpose cipher API and it is not used for transport security.
mod vnc_des {
    const IP: [u8; 64] = [
        58, 50, 42, 34, 26, 18, 10, 2, 60, 52, 44, 36, 28, 20, 12, 4, 62, 54, 46, 38, 30, 22, 14,
        6, 64, 56, 48, 40, 32, 24, 16, 8, 57, 49, 41, 33, 25, 17, 9, 1, 59, 51, 43, 35, 27, 19, 11,
        3, 61, 53, 45, 37, 29, 21, 13, 5, 63, 55, 47, 39, 31, 23, 15, 7,
    ];
    const FP: [u8; 64] = [
        40, 8, 48, 16, 56, 24, 64, 32, 39, 7, 47, 15, 55, 23, 63, 31, 38, 6, 46, 14, 54, 22, 62,
        30, 37, 5, 45, 13, 53, 21, 61, 29, 36, 4, 44, 12, 52, 20, 60, 28, 35, 3, 43, 11, 51, 19,
        59, 27, 34, 2, 42, 10, 50, 18, 58, 26, 33, 1, 41, 9, 49, 17, 57, 25,
    ];
    const E: [u8; 48] = [
        32, 1, 2, 3, 4, 5, 4, 5, 6, 7, 8, 9, 8, 9, 10, 11, 12, 13, 12, 13, 14, 15, 16, 17, 16, 17,
        18, 19, 20, 21, 20, 21, 22, 23, 24, 25, 24, 25, 26, 27, 28, 29, 28, 29, 30, 31, 32, 1,
    ];
    const P: [u8; 32] = [
        16, 7, 20, 21, 29, 12, 28, 17, 1, 15, 23, 26, 5, 18, 31, 10, 2, 8, 24, 14, 32, 27, 3, 9,
        19, 13, 30, 6, 22, 11, 4, 25,
    ];
    const PC1: [u8; 56] = [
        57, 49, 41, 33, 25, 17, 9, 1, 58, 50, 42, 34, 26, 18, 10, 2, 59, 51, 43, 35, 27, 19, 11, 3,
        60, 52, 44, 36, 63, 55, 47, 39, 31, 23, 15, 7, 62, 54, 46, 38, 30, 22, 14, 6, 61, 53, 45,
        37, 29, 21, 13, 5, 28, 20, 12, 4,
    ];
    const PC2: [u8; 48] = [
        14, 17, 11, 24, 1, 5, 3, 28, 15, 6, 21, 10, 23, 19, 12, 4, 26, 8, 16, 7, 27, 20, 13, 2, 41,
        52, 31, 37, 47, 55, 30, 40, 51, 45, 33, 48, 44, 49, 39, 56, 34, 53, 46, 42, 50, 36, 29, 32,
    ];
    const SHIFTS: [u8; 16] = [1, 1, 2, 2, 2, 2, 2, 2, 1, 2, 2, 2, 2, 2, 2, 1];
    const S: [[[u8; 16]; 4]; 8] = [
        [
            [14, 4, 13, 1, 2, 15, 11, 8, 3, 10, 6, 12, 5, 9, 0, 7],
            [0, 15, 7, 4, 14, 2, 13, 1, 10, 6, 12, 11, 9, 5, 3, 8],
            [4, 1, 14, 8, 13, 6, 2, 11, 15, 12, 9, 7, 3, 10, 5, 0],
            [15, 12, 8, 2, 4, 9, 1, 7, 5, 11, 3, 14, 10, 0, 6, 13],
        ],
        [
            [15, 1, 8, 14, 6, 11, 3, 4, 9, 7, 2, 13, 12, 0, 5, 10],
            [3, 13, 4, 7, 15, 2, 8, 14, 12, 0, 1, 10, 6, 9, 11, 5],
            [0, 14, 7, 11, 10, 4, 13, 1, 5, 8, 12, 6, 9, 3, 2, 15],
            [13, 8, 10, 1, 3, 15, 4, 2, 11, 6, 7, 12, 0, 5, 14, 9],
        ],
        [
            [10, 0, 9, 14, 6, 3, 15, 5, 1, 13, 12, 7, 11, 4, 2, 8],
            [13, 7, 0, 9, 3, 4, 6, 10, 2, 8, 5, 14, 12, 11, 15, 1],
            [13, 6, 4, 9, 8, 15, 3, 0, 11, 1, 2, 12, 5, 10, 14, 7],
            [1, 10, 13, 0, 6, 9, 8, 7, 4, 15, 14, 3, 11, 5, 2, 12],
        ],
        [
            [7, 13, 14, 3, 0, 6, 9, 10, 1, 2, 8, 5, 11, 12, 4, 15],
            [13, 8, 11, 5, 6, 15, 0, 3, 4, 7, 2, 12, 1, 10, 14, 9],
            [10, 6, 9, 0, 12, 11, 7, 13, 15, 1, 3, 14, 5, 2, 8, 4],
            [3, 15, 0, 6, 10, 1, 13, 8, 9, 4, 5, 11, 12, 7, 2, 14],
        ],
        [
            [2, 12, 4, 1, 7, 10, 11, 6, 8, 5, 3, 15, 13, 0, 14, 9],
            [14, 11, 2, 12, 4, 7, 13, 1, 5, 0, 15, 10, 3, 9, 8, 6],
            [4, 2, 1, 11, 10, 13, 7, 8, 15, 9, 12, 5, 6, 3, 0, 14],
            [11, 8, 12, 7, 1, 14, 2, 13, 6, 15, 0, 9, 10, 4, 5, 3],
        ],
        [
            [12, 1, 10, 15, 9, 2, 6, 8, 0, 13, 3, 4, 14, 7, 5, 11],
            [10, 15, 4, 2, 7, 12, 9, 5, 6, 1, 13, 14, 0, 11, 3, 8],
            [9, 14, 15, 5, 2, 8, 12, 3, 7, 0, 4, 10, 1, 13, 11, 6],
            [4, 3, 2, 12, 9, 5, 15, 10, 11, 14, 1, 7, 6, 0, 8, 13],
        ],
        [
            [4, 11, 2, 14, 15, 0, 8, 13, 3, 12, 9, 7, 5, 10, 6, 1],
            [13, 0, 11, 7, 4, 9, 1, 10, 14, 3, 5, 12, 2, 15, 8, 6],
            [1, 4, 11, 13, 12, 3, 7, 14, 10, 15, 6, 8, 0, 5, 9, 2],
            [6, 11, 13, 8, 1, 4, 10, 7, 9, 5, 0, 15, 14, 2, 3, 12],
        ],
        [
            [13, 2, 8, 4, 6, 15, 11, 1, 10, 9, 3, 14, 5, 0, 12, 7],
            [1, 15, 13, 8, 10, 3, 7, 4, 12, 5, 6, 11, 0, 14, 9, 2],
            [7, 11, 4, 1, 9, 12, 14, 2, 0, 6, 10, 13, 15, 3, 5, 8],
            [2, 1, 14, 7, 4, 10, 8, 13, 15, 12, 9, 0, 3, 5, 6, 11],
        ],
    ];

    pub fn response(key: &[u8; 8], challenge: &[u8; 16]) -> [u8; 16] {
        let mut output = [0u8; 16];
        output[..8].copy_from_slice(&encrypt_block(key, &challenge[..8]));
        output[8..].copy_from_slice(&encrypt_block(key, &challenge[8..]));
        output
    }

    pub fn encrypt_block(key: &[u8; 8], block: &[u8]) -> [u8; 8] {
        let mut block_bytes = [0u8; 8];
        block_bytes.copy_from_slice(&block[..8]);
        let subkeys = subkeys(key);
        let permuted = permute(u64::from_be_bytes(block_bytes), &IP, 64);
        let mut left = (permuted >> 32) as u32;
        let mut right = permuted as u32;
        for subkey in subkeys {
            let next = left ^ feistel(right, subkey);
            left = right;
            right = next;
        }
        let preoutput = ((u64::from(right)) << 32) | u64::from(left);
        permute(preoutput, &FP, 64).to_be_bytes()
    }

    fn subkeys(key: &[u8; 8]) -> [u64; 16] {
        let permuted = permute(u64::from_be_bytes(*key), &PC1, 64);
        let mut c = ((permuted >> 28) & 0x0fff_ffff) as u32;
        let mut d = (permuted & 0x0fff_ffff) as u32;
        let mut keys = [0u64; 16];
        for (index, shift) in SHIFTS.iter().enumerate() {
            c = rotate_left_28(c, *shift);
            d = rotate_left_28(d, *shift);
            let combined = (u64::from(c) << 28) | u64::from(d);
            keys[index] = permute(combined, &PC2, 56);
        }
        keys
    }

    fn rotate_left_28(value: u32, shift: u8) -> u32 {
        ((value << shift) | (value >> (28 - shift))) & 0x0fff_ffff
    }

    fn feistel(right: u32, subkey: u64) -> u32 {
        let expanded = permute(u64::from(right), &E, 32) ^ subkey;
        let mut substituted = 0u32;
        for index in 0..8 {
            let six = ((expanded >> (42 - 6 * index)) & 0x3f) as u8;
            let row = ((six >> 4) & 0x2 | (six & 0x1)) as usize;
            let column = ((six >> 1) & 0x0f) as usize;
            substituted = (substituted << 4) | u32::from(S[index][row][column]);
        }
        permute(u64::from(substituted), &P, 32) as u32
    }

    fn permute(input: u64, table: &[u8], input_bits: u32) -> u64 {
        let mut output = 0u64;
        for position in table {
            let bit = (input >> (input_bits - u32::from(*position))) & 1;
            output = (output << 1) | bit;
        }
        output
    }
}

async fn wait_for_first_geometry(
    frames: &mut watch::Receiver<Option<Arc<FrameSnapshot>>>,
) -> Result<FrameFormat, ProtocolError> {
    if let Some(frame) = frames.borrow().clone() {
        return Ok(frame.format);
    }
    loop {
        if frames.changed().await.is_err() {
            return Err(ProtocolError::GeometryUnavailable);
        }
        if let Some(frame) = frames.borrow().clone() {
            return Ok(frame.format);
        }
    }
}

fn parse_version(version: &[u8; 12]) -> Result<u8, ProtocolError> {
    if &version[0..4] != b"RFB "
        || version[7] != b'.'
        || version[11] != b'\n'
        || !version[4..7].iter().all(u8::is_ascii_digit)
        || !version[8..11].iter().all(u8::is_ascii_digit)
    {
        return Err(ProtocolError::MalformedVersion);
    }
    let major = u16::from(version[4] - b'0') * 100
        + u16::from(version[5] - b'0') * 10
        + u16::from(version[6] - b'0');
    let minor = u16::from(version[8] - b'0') * 100
        + u16::from(version[9] - b'0') * 10
        + u16::from(version[10] - b'0');
    if major != 3 || !matches!(minor, 3 | 7 | 8) {
        return Err(ProtocolError::Version(minor));
    }
    Ok(minor as u8)
}

async fn client_read_loop(
    mut read: OwnedReadHalf,
    context: Arc<ClientContext>,
    commands: mpsc::Sender<ClientCommand>,
    idle_timeout: Option<Duration>,
) {
    let mut buttons = 0u8;
    loop {
        let mut message_type = [0u8; 1];
        match idle_timeout {
            Some(limit) => {
                match tokio::time::timeout(limit, read.read_exact(&mut message_type)).await {
                    Ok(Ok(_)) => {}
                    Ok(Err(error)) => {
                        if error.kind() != io::ErrorKind::UnexpectedEof {
                            debug!(client = context.id, error = %error, "VNC read ended");
                        }
                        return;
                    }
                    Err(_) => {
                        // The viewer stopped sending client messages (usually
                        // FramebufferUpdateRequest) for the configured window.
                        debug!(
                            client = context.id,
                            timeout_secs = limit.as_secs(),
                            "VNC client idle timeout"
                        );
                        return;
                    }
                }
            }
            None => {
                if let Err(error) = read.read_exact(&mut message_type).await {
                    if error.kind() != io::ErrorKind::UnexpectedEof {
                        debug!(client = context.id, error = %error, "VNC read ended");
                    }
                    return;
                }
            }
        }

        let result = match message_type[0] {
            0 => read_set_pixel_format(&mut read, &context).await,
            2 => read_set_encodings(&mut read, &context).await,
            3 => read_update_request(&mut read, &context, &commands).await,
            4 => read_key_event(&mut read, &context).await,
            5 => read_pointer_event(&mut read, &context, &mut buttons).await,
            6 => read_client_cut_text(&mut read, &context).await,
            other => Err(ProtocolError::UnknownMessage(other)),
        };

        if let Err(error) = result {
            context
                .stats
                .protocol_errors
                .fetch_add(1, Ordering::Relaxed);
            warn!(client = context.id, error = %error, "VNC client message rejected");
            return;
        }
    }
}

async fn read_set_pixel_format(
    read: &mut OwnedReadHalf,
    context: &ClientContext,
) -> Result<(), ProtocolError> {
    // 3 padding bytes + the 16-byte PIXEL_FORMAT structure.
    let mut body = [0u8; 19];
    read.read_exact(&mut body)
        .await
        .map_err(ProtocolError::io)?;
    let format =
        ClientPixelFormat::parse(&body[3..]).map_err(ProtocolError::UnsupportedPixelFormat)?;
    *lock(&context.pixel) = format;
    debug!(client = context.id, pixel_format = %format, "VNC pixel format applied");
    Ok(())
}

impl ClientPixelFormat {
    fn parse(bytes: &[u8]) -> Result<Self, String> {
        if bytes.len() != 16 {
            return Err(format!("expected 16 bytes, got {}", bytes.len()));
        }
        let bits_per_pixel = bytes[0];
        let depth = bytes[1];
        let big_endian = bytes[2] != 0;
        let true_colour = bytes[3] != 0;
        let red_max = u16::from_be_bytes([bytes[4], bytes[5]]);
        let green_max = u16::from_be_bytes([bytes[6], bytes[7]]);
        let blue_max = u16::from_be_bytes([bytes[8], bytes[9]]);
        let red_shift = bytes[10];
        let green_shift = bytes[11];
        let blue_shift = bytes[12];

        if bits_per_pixel != 32 {
            return Err(format!(
                "only 32 bits-per-pixel is supported, got {bits_per_pixel}"
            ));
        }
        if !(1..=32).contains(&depth) {
            return Err(format!("depth must be 1..=32, got {depth}"));
        }
        if !true_colour {
            return Err("only true-colour pixel formats are supported".to_owned());
        }
        if red_max != 255 || green_max != 255 || blue_max != 255 {
            return Err(format!(
                "only 8 bits per channel is supported (max {red_max}/{green_max}/{blue_max})"
            ));
        }
        if green_shift != 8
            || !((red_shift == 16 && blue_shift == 0) || (red_shift == 0 && blue_shift == 16))
        {
            return Err(format!(
                "only RGB (r16 g8 b0) or BGR (r0 g8 b16) shifts are supported, \
                 got r{red_shift} g{green_shift} b{blue_shift}"
            ));
        }
        Ok(Self {
            big_endian,
            red_shift,
            green_shift,
            blue_shift,
        })
    }
}

async fn read_set_encodings(
    read: &mut OwnedReadHalf,
    context: &ClientContext,
) -> Result<(), ProtocolError> {
    let mut header = [0u8; 3];
    read.read_exact(&mut header)
        .await
        .map_err(ProtocolError::io)?;
    let count = u16::from_be_bytes([header[1], header[2]]) as usize;
    if count > MAX_ENCODINGS {
        return Err(ProtocolError::TooManyEncodings(count));
    }
    let mut encodings = vec![0u8; count * 4];
    read.read_exact(&mut encodings)
        .await
        .map_err(ProtocolError::io)?;
    let mut saw_raw = false;
    for chunk in encodings.chunks_exact(4) {
        let encoding = i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if encoding == ENCODING_RAW {
            saw_raw = true;
        }
    }
    if !saw_raw && count > 0 {
        // Raw is always acceptable in RFB; the client's preference list does not
        // remove it. Record the client's choices without claiming another codec.
        debug!(
            client = context.id,
            count, "VNC client did not list Raw; sending Raw anyway per RFB"
        );
    }
    Ok(())
}

async fn read_update_request(
    read: &mut OwnedReadHalf,
    context: &ClientContext,
    commands: &mpsc::Sender<ClientCommand>,
) -> Result<(), ProtocolError> {
    let mut body = [0u8; 9];
    read.read_exact(&mut body)
        .await
        .map_err(ProtocolError::io)?;
    let incremental = body[0] != 0;
    let rect = Rect {
        x: u16::from_be_bytes([body[1], body[2]]),
        y: u16::from_be_bytes([body[3], body[4]]),
        width: u16::from_be_bytes([body[5], body[6]]),
        height: u16::from_be_bytes([body[7], body[8]]),
    };
    let rects = clip_rects(&[rect], context.width, context.height);
    commands
        .send(ClientCommand::Update { incremental, rects })
        .await
        .map_err(|_| ProtocolError::ClientGone)
}

async fn read_key_event(
    read: &mut OwnedReadHalf,
    context: &ClientContext,
) -> Result<(), ProtocolError> {
    let mut body = [0u8; 7];
    read.read_exact(&mut body)
        .await
        .map_err(ProtocolError::io)?;
    let down = body[0] != 0;
    let keysym = u32::from_be_bytes([body[3], body[4], body[5], body[6]]);
    match keysym_to_hid(keysym) {
        Some(code) => {
            let event = InputEvent::Key {
                code: u32::from(code),
                down,
                modifiers: Default::default(),
            };
            match context.sink.submit(&event) {
                Ok(()) => context.input_active.store(true, Ordering::Release),
                Err(error) => debug!(
                    client = context.id,
                    keysym,
                    code,
                    error = %error,
                    "VNC key event was rejected by the input sink"
                ),
            }
        }
        None => {
            context
                .stats
                .unsupported_keys
                .fetch_add(1, Ordering::Relaxed);
            debug!(client = context.id, keysym, "unknown VNC keysym ignored");
        }
    }
    Ok(())
}

async fn read_pointer_event(
    read: &mut OwnedReadHalf,
    context: &ClientContext,
    buttons: &mut u8,
) -> Result<(), ProtocolError> {
    let mut body = [0u8; 5];
    read.read_exact(&mut body)
        .await
        .map_err(ProtocolError::io)?;
    let mask = body[0];
    let x = u16::from_be_bytes([body[1], body[2]]);
    let y = u16::from_be_bytes([body[3], body[4]]);

    // The shared sink's default mapping is identity HID report space. Convert
    // framebuffer pixels to 0..=32767 here so a viewer coordinate is not
    // interpreted as a tiny pixel coordinate by the USB/uinput sink.
    let report_x = axis_to_report(x, context.width);
    let report_y = axis_to_report(y, context.height);
    submit(
        context,
        InputEvent::PointerAbsolute {
            display_id: None,
            x: report_x,
            y: report_y,
        },
        "pointer",
    );

    let changed = mask ^ *buttons;
    for (bit, button) in [
        (0x01, PointerButton::Left),
        (0x02, PointerButton::Middle),
        (0x04, PointerButton::Right),
        (0x20, PointerButton::Back),
        (0x40, PointerButton::Forward),
    ] {
        if changed & bit != 0 {
            submit(
                context,
                InputEvent::Button {
                    button,
                    down: mask & bit != 0,
                },
                "button",
            );
        }
    }
    // RFB models wheel detents as buttons 4 (up) and 5 (down). HID wheel
    // reports use positive for up, matching the rest of the input pipeline.
    if changed & 0x08 != 0 && mask & 0x08 != 0 {
        submit(context, InputEvent::Wheel { dx: 0, dy: 1 }, "wheel-up");
    }
    if changed & 0x10 != 0 && mask & 0x10 != 0 {
        submit(context, InputEvent::Wheel { dx: 0, dy: -1 }, "wheel-down");
    }
    *buttons = mask;
    Ok(())
}

fn submit(context: &ClientContext, event: InputEvent, kind: &str) {
    match context.sink.submit(&event) {
        Ok(()) => context.input_active.store(true, Ordering::Release),
        Err(error) => debug!(
            client = context.id,
            kind,
            error = %error,
            "VNC pointer event was rejected by the input sink"
        ),
    }
}

fn axis_to_report(value: u16, extent: u32) -> i32 {
    if extent <= 1 {
        return 0;
    }
    let last = i64::from(extent - 1);
    let value = i64::from(value).clamp(0, last);
    ((value * 32_767) / last) as i32
}

async fn read_client_cut_text(
    read: &mut OwnedReadHalf,
    context: &ClientContext,
) -> Result<(), ProtocolError> {
    let mut body = [0u8; 7];
    read.read_exact(&mut body)
        .await
        .map_err(ProtocolError::io)?;
    let length = u32::from_be_bytes([body[3], body[4], body[5], body[6]]);
    if length > MAX_CUT_TEXT_BYTES {
        return Err(ProtocolError::CutTextTooLarge {
            length,
            max: MAX_CUT_TEXT_BYTES,
        });
    }
    let mut ignored = vec![0u8; length as usize];
    read.read_exact(&mut ignored)
        .await
        .map_err(ProtocolError::io)?;
    debug!(
        client = context.id,
        length, "ignored VNC ClientCutText (clipboard is not exposed by this node)"
    );
    Ok(())
}

async fn client_write_loop(
    mut write: OwnedWriteHalf,
    mut commands: mpsc::Receiver<ClientCommand>,
    mut frames: watch::Receiver<Option<Arc<FrameSnapshot>>>,
    context: Arc<ClientContext>,
    write_timeout: Duration,
) -> Result<(), ProtocolError> {
    let mut pending: Option<PendingUpdate> = None;
    let mut last_sent_generation = 0u64;

    loop {
        if let Some(request) = pending.take() {
            let snapshot = { frames.borrow().clone() };
            let should_send = snapshot.as_ref().is_some_and(|frame| {
                !request.incremental || frame.generation > last_sent_generation
            });

            if should_send {
                let frame = snapshot.expect("checked above");
                let pixel = *lock(&context.pixel);
                let (update, payload_bytes) = build_update(&frame, pixel, &request.rects);
                match tokio::time::timeout(write_timeout, write.write_all(&update)).await {
                    Ok(Ok(())) => {
                        if frame.generation > last_sent_generation.saturating_add(1) {
                            context.stats.frames_coalesced.fetch_add(
                                frame.generation - last_sent_generation - 1,
                                Ordering::Relaxed,
                            );
                        }
                        context.stats.updates_sent.fetch_add(1, Ordering::Relaxed);
                        context
                            .stats
                            .bytes_sent
                            .fetch_add(payload_bytes as u64, Ordering::Relaxed);
                        last_sent_generation = frame.generation;
                        continue;
                    }
                    Ok(Err(error)) => return Err(ProtocolError::io(error)),
                    Err(_) => {
                        context.stats.slow_clients.fetch_add(1, Ordering::Relaxed);
                        return Err(ProtocolError::WriteTimeout(write_timeout));
                    }
                }
            }

            pending = Some(request);
        }

        tokio::select! {
            changed = frames.changed() => {
                if changed.is_err() {
                    return Err(ProtocolError::CaptureStopped);
                }
            }
            command = commands.recv() => {
                match command {
                    Some(command) => {
                        let next = command.into_pending();
                        match pending.as_mut() {
                            Some(pending) => pending.merge(ClientCommand::Update {
                                incremental: next.incremental,
                                rects: next.rects,
                            }),
                            None => pending = Some(next),
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

fn build_update(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    rects: &[Rect],
) -> (Vec<u8>, usize) {
    let non_empty: Vec<Rect> = rects
        .iter()
        .copied()
        .filter(|rect| rect.width != 0 && rect.height != 0)
        .collect();
    let mut payload = 0usize;
    for rect in &non_empty {
        payload += usize::from(rect.width) * usize::from(rect.height) * 4;
    }
    let mut update = Vec::with_capacity(4 + non_empty.len() * 12 + payload);
    update.push(0); // FramebufferUpdate
    update.push(0); // padding
    update.extend_from_slice(&(non_empty.len() as u16).to_be_bytes());
    for rect in non_empty {
        update.extend_from_slice(&rect.x.to_be_bytes());
        update.extend_from_slice(&rect.y.to_be_bytes());
        update.extend_from_slice(&rect.width.to_be_bytes());
        update.extend_from_slice(&rect.height.to_be_bytes());
        update.extend_from_slice(&0i32.to_be_bytes()); // Raw encoding
        encode_raw_rect(frame, pixel, rect, &mut update);
    }
    (update, payload)
}

fn encode_raw_rect(frame: &FrameSnapshot, pixel: ClientPixelFormat, rect: Rect, out: &mut Vec<u8>) {
    let bytes_per_pixel = match frame.format.pixel_format {
        PixelFormat::Rgb24 => 3,
        PixelFormat::Bgra => 4,
        _ => return,
    };
    let stride = frame.format.width as usize * bytes_per_pixel;
    for row in 0..usize::from(rect.height) {
        let y = usize::from(rect.y) + row;
        let row_start = y * stride;
        for column in 0..usize::from(rect.width) {
            let x = usize::from(rect.x) + column;
            let index = row_start + x * bytes_per_pixel;
            let (red, green, blue) = match frame.format.pixel_format {
                PixelFormat::Rgb24 => (
                    frame.data[index],
                    frame.data[index + 1],
                    frame.data[index + 2],
                ),
                PixelFormat::Bgra => (
                    frame.data[index + 2],
                    frame.data[index + 1],
                    frame.data[index],
                ),
                _ => (0, 0, 0),
            };
            let value = (u32::from(red) << pixel.red_shift)
                | (u32::from(green) << pixel.green_shift)
                | (u32::from(blue) << pixel.blue_shift);
            if pixel.big_endian {
                out.extend_from_slice(&value.to_be_bytes());
            } else {
                out.extend_from_slice(&value.to_le_bytes());
            }
        }
    }
}

fn clip_rects(rects: &[Rect], width: u32, height: u32) -> Vec<Rect> {
    let width = width.min(u32::from(u16::MAX)) as u32;
    let height = height.min(u32::from(u16::MAX)) as u32;
    rects
        .iter()
        .filter_map(|rect| {
            let x = u32::from(rect.x).min(width);
            let y = u32::from(rect.y).min(height);
            let right = (u32::from(rect.x) + u32::from(rect.width)).min(width);
            let bottom = (u32::from(rect.y) + u32::from(rect.height)).min(height);
            (right > x && bottom > y).then_some(Rect {
                x: x as u16,
                y: y as u16,
                width: (right - x) as u16,
                height: (bottom - y) as u16,
            })
        })
        .collect()
}

/// Keysyms understood by the RFB-to-HID adapter.
///
/// This is intentionally a fixed table, not a layout engine. ASCII maps to the
/// physical key that produces the character on a US layout; named X11 keysyms
/// cover the navigation, modifier, function, and keypad keys. Anything else is
/// reported as unsupported instead of guessed.
fn keysym_to_hid(keysym: u32) -> Option<u8> {
    if (0x20..=0x7e).contains(&keysym) {
        return ascii_to_hid(keysym as u8);
    }
    Some(match keysym {
        0xff08 => 0x2a,                                    // BackSpace
        0xff09 => 0x2b,                                    // Tab
        0xff0d => 0x28,                                    // Return
        0xff1b => 0x29,                                    // Escape
        0xff50 => 0x4a,                                    // Home
        0xff51 => 0x50,                                    // Left
        0xff52 => 0x52,                                    // Up
        0xff53 => 0x4f,                                    // Right
        0xff54 => 0x51,                                    // Down
        0xff55 => 0x4b,                                    // Page_Up / Prior
        0xff56 => 0x4e,                                    // Page_Down / Next
        0xff57 => 0x4d,                                    // End
        0xff58 => 0x4a,                                    // Begin
        0xff63 => 0x49,                                    // Insert
        0xffff => 0x4c,                                    // Delete
        0xff13 => 0x48,                                    // Pause
        0xff14 => 0x47,                                    // Scroll_Lock
        0xff15 => 0x46,                                    // Sys_Req
        0xff61 => 0x46,                                    // Print
        0xff67 => 0x65,                                    // Menu
        0xff7f => 0x53,                                    // Num_Lock
        0xffe1 => 0xe1,                                    // Shift_L
        0xffe2 => 0xe5,                                    // Shift_R
        0xffe3 => 0xe0,                                    // Control_L
        0xffe4 => 0xe4,                                    // Control_R
        0xffe5 => 0x39,                                    // Caps_Lock
        0xffe7 => 0xe3,                                    // Meta_L
        0xffe8 => 0xe7,                                    // Meta_R
        0xffe9 => 0xe2,                                    // Alt_L
        0xffea => 0xe6,                                    // Alt_R
        0xffeb => 0xe3,                                    // Super_L
        0xffec => 0xe7,                                    // Super_R
        0xfe03 => 0xe6,                                    // ISO_Level3_Shift
        0xff20 => 0x65,                                    // Multi_key / Compose
        0xff8d => 0x58,                                    // KP_Enter
        0xffaa => 0x55,                                    // KP_Multiply
        0xffab => 0x57,                                    // KP_Add
        0xffad => 0x56,                                    // KP_Subtract
        0xffae => 0x63,                                    // KP_Decimal
        0xffaf => 0x54,                                    // KP_Divide
        0xffb0 => 0x62,                                    // KP_0
        0xffb1 => 0x59,                                    // KP_1
        0xffb2 => 0x5a,                                    // KP_2
        0xffb3 => 0x5b,                                    // KP_3
        0xffb4 => 0x5c,                                    // KP_4
        0xffb5 => 0x5d,                                    // KP_5
        0xffb6 => 0x5e,                                    // KP_6
        0xffb7 => 0x5f,                                    // KP_7
        0xffb8 => 0x60,                                    // KP_8
        0xffb9 => 0x61,                                    // KP_9
        0xffbe..=0xffc9 => 0x3a + (keysym - 0xffbe) as u8, // F1..F12
        0xffca..=0xffd5 => 0x68 + (keysym - 0xffca) as u8, // F13..F24
        0x1008ff12 => 0x7f,                                // XF86AudioMute
        0x1008ff13 => 0x80,                                // XF86AudioRaiseVolume
        0x1008ff11 => 0x81,                                // XF86AudioLowerVolume
        _ => return None,
    })
}

fn ascii_to_hid(byte: u8) -> Option<u8> {
    Some(match byte {
        b'a'..=b'z' => 0x04 + (byte - b'a'),
        b'A'..=b'Z' => 0x04 + (byte - b'A'),
        b'1'..=b'9' => 0x1e + (byte - b'1'),
        b'0' => 0x27,
        b' ' => 0x2c,
        b'-' => 0x2d,
        b'=' => 0x2e,
        b'[' => 0x2f,
        b']' => 0x30,
        b'\\' => 0x31,
        b';' => 0x33,
        b'\'' => 0x34,
        b'`' => 0x35,
        b',' => 0x36,
        b'.' => 0x37,
        b'/' => 0x38,
        // Shifted ASCII characters map to the physical key that produces them.
        b'!' => 0x1e,
        b'@' => 0x1f,
        b'#' => 0x20,
        b'$' => 0x21,
        b'%' => 0x22,
        b'^' => 0x23,
        b'&' => 0x24,
        b'*' => 0x25,
        b'(' => 0x26,
        b')' => 0x27,
        b'_' => 0x2d,
        b'+' => 0x2e,
        b'{' => 0x2f,
        b'}' => 0x30,
        b'|' => 0x31,
        b':' => 0x33,
        b'"' => 0x34,
        b'~' => 0x35,
        b'<' => 0x36,
        b'>' => 0x37,
        b'?' => 0x38,
        _ => return None,
    })
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[derive(Debug)]
enum ProtocolError {
    Io(io::Error),
    MalformedVersion,
    Version(u16),
    SecurityType(u8),
    AuthenticationFailed,
    UnsupportedPixelFormat(String),
    TooManyEncodings(usize),
    UnknownMessage(u8),
    CutTextTooLarge { length: u32, max: u32 },
    WriteTimeout(Duration),
    ClientGone,
    CaptureStopped,
    GeometryUnavailable,
}

impl ProtocolError {
    fn io(error: io::Error) -> Self {
        Self::Io(error)
    }
}

impl fmt::Display for ProtocolError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Io(error) => write!(formatter, "I/O error: {error}"),
            Self::MalformedVersion => formatter.write_str("malformed RFB version string"),
            Self::Version(minor) => {
                write!(formatter, "unsupported RFB version 3.{minor:03}")
            }
            Self::SecurityType(kind) => {
                write!(formatter, "unsupported security type {kind}")
            }
            Self::AuthenticationFailed => formatter.write_str("VNC authentication failed"),
            Self::UnsupportedPixelFormat(reason) => {
                write!(formatter, "unsupported pixel format: {reason}")
            }
            Self::TooManyEncodings(count) => {
                write!(
                    formatter,
                    "SetEncodings has {count} entries; limit is {MAX_ENCODINGS}"
                )
            }
            Self::UnknownMessage(kind) => {
                write!(formatter, "unknown client message type {kind}")
            }
            Self::CutTextTooLarge { length, max } => {
                write!(
                    formatter,
                    "ClientCutText length {length} exceeds {max} bytes"
                )
            }
            Self::WriteTimeout(duration) => {
                write!(
                    formatter,
                    "framebuffer write exceeded {duration:?}; closing slow viewer"
                )
            }
            Self::ClientGone => formatter.write_str("client read side is gone"),
            Self::CaptureStopped => formatter.write_str("capture source stopped"),
            Self::GeometryUnavailable => {
                formatter.write_str("capture source has not published a frame with usable geometry")
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ozma_input::FakeInputSink;
    use ozma_media::Frame;
    use serde_json::Value;
    use tokio::io::AsyncReadExt;

    struct TestSource {
        format: FrameFormat,
        first_frame: Option<FrameFormat>,
        sequence: u64,
        pause_after: Option<u64>,
        stopped: bool,
        solid: Option<(u8, u8, u8)>,
    }

    impl TestSource {
        fn new(width: u32, height: u32) -> Self {
            Self {
                format: FrameFormat::rgb24(width, height),
                first_frame: None,
                sequence: 0,
                pause_after: None,
                stopped: false,
                solid: None,
            }
        }

        fn with_first_frame(mut self, width: u32, height: u32) -> Self {
            self.first_frame = Some(FrameFormat::rgb24(width, height));
            self
        }

        fn solid(mut self, red: u8, green: u8, blue: u8) -> Self {
            self.solid = Some((red, green, blue));
            self
        }

        fn pause_after(mut self, frames: u64) -> Self {
            self.pause_after = Some(frames);
            self
        }
    }

    #[async_trait]
    impl CaptureSource for TestSource {
        fn id(&self) -> &str {
            "test-vnc-source"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            if self.stopped {
                return Err(MediaError::EndOfStream);
            }
            if self
                .pause_after
                .is_some_and(|frames| self.sequence >= frames)
            {
                std::future::pending::<()>().await;
            }
            tokio::task::yield_now().await;
            let sequence = self.sequence;
            let frame_format = if self.format.width == 0 || self.format.height == 0 {
                let first = self.first_frame.unwrap_or_else(|| FrameFormat::rgb24(4, 2));
                self.format = first;
                first
            } else {
                self.format
            };
            let mut data = vec![
                0u8;
                frame_format
                    .pixel_format
                    .nominal_len(frame_format.width, frame_format.height)
                    .unwrap()
            ];
            let (red, green, blue) =
                self.solid
                    .unwrap_or((sequence as u8, (sequence * 2) as u8, (sequence * 3) as u8));
            match frame_format.pixel_format {
                PixelFormat::Rgb24 => {
                    for pixel in data.chunks_exact_mut(3) {
                        pixel[0] = red;
                        pixel[1] = green;
                        pixel[2] = blue;
                    }
                }
                PixelFormat::Bgra => {
                    for pixel in data.chunks_exact_mut(4) {
                        pixel[0] = blue;
                        pixel[1] = green;
                        pixel[2] = red;
                        pixel[3] = 0xff;
                    }
                }
                _ => unreachable!("test source only uses RGB24/BGRA"),
            }
            self.sequence = self.sequence.saturating_add(1);
            Frame::with_data(frame_format, sequence, sequence, true, data)
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            self.stopped = true;
            Ok(())
        }
    }

    async fn read_exact(stream: &mut TcpStream, buf: &mut [u8]) {
        tokio::time::timeout(Duration::from_secs(2), stream.read_exact(buf))
            .await
            .expect("read timed out")
            .expect("read failed");
    }

    async fn write_all(stream: &mut TcpStream, bytes: &[u8]) {
        stream.write_all(bytes).await.expect("write failed");
    }

    async fn read_server_init(stream: &mut TcpStream) -> (u16, u16, Vec<u8>) {
        let mut init = [0u8; 24];
        read_exact(stream, &mut init).await;
        let width = u16::from_be_bytes([init[0], init[1]]);
        let height = u16::from_be_bytes([init[2], init[3]]);
        let name_len = u32::from_be_bytes([init[20], init[21], init[22], init[23]]) as usize;
        let mut name = vec![0u8; name_len];
        read_exact(stream, &mut name).await;
        (width, height, name)
    }

    async fn handshake_client(stream: &mut TcpStream) -> (u16, u16, Vec<u8>) {
        let mut version = [0u8; 12];
        read_exact(stream, &mut version).await;
        assert_eq!(&version, b"RFB 003.008\n");
        write_all(stream, b"RFB 003.008\n").await;

        let mut security = [0u8; 2];
        read_exact(stream, &mut security).await;
        assert_eq!(security, [1, 1]);
        write_all(stream, &[1]).await;

        let mut result = [0u8; 4];
        read_exact(stream, &mut result).await;
        assert_eq!(result, 0u32.to_be_bytes());

        write_all(stream, &[1]).await;
        read_server_init(stream).await
    }

    /// Independent test-side implementation of the RFB 3.8 VNC-auth response:
    /// pad/truncate to eight bytes, reverse each password byte's bits, and DES-
    /// encrypt both halves of the challenge.
    fn client_vnc_response(password: &[u8], challenge: &[u8; 16]) -> [u8; 16] {
        let mut key = [0u8; 8];
        for (index, key_byte) in key.iter_mut().enumerate() {
            *key_byte = password.get(index).copied().unwrap_or(0).reverse_bits();
        }
        let mut response = [0u8; 16];
        response[..8].copy_from_slice(&vnc_des::encrypt_block(&key, &challenge[..8]));
        response[8..].copy_from_slice(&vnc_des::encrypt_block(&key, &challenge[8..]));
        response
    }

    async fn handshake_client_with_password(
        stream: &mut TcpStream,
        password: &[u8],
    ) -> (u16, u16, Vec<u8>) {
        let mut version = [0u8; 12];
        read_exact(stream, &mut version).await;
        assert_eq!(&version, b"RFB 003.008\n");
        write_all(stream, b"RFB 003.008\n").await;

        let mut security = [0u8; 2];
        read_exact(stream, &mut security).await;
        assert_eq!(
            security,
            [1, SECURITY_TYPE_VNC_AUTH],
            "a password-configured server must offer only VNC authentication"
        );
        write_all(stream, &[SECURITY_TYPE_VNC_AUTH]).await;

        let mut challenge = [0u8; 16];
        read_exact(stream, &mut challenge).await;
        let response = client_vnc_response(password, &challenge);
        write_all(stream, &response).await;

        let mut result = [0u8; 4];
        read_exact(stream, &mut result).await;
        assert_eq!(result, 0u32.to_be_bytes());

        write_all(stream, &[1]).await;
        read_server_init(stream).await
    }

    async fn assert_connection_closed(stream: &mut TcpStream) {
        let mut byte = [0u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(2), stream.read(&mut byte)).await;
        match result {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            other => panic!("expected the connection to close, got {other:?}"),
        }
    }

    async fn request_update(stream: &mut TcpStream, incremental: bool, width: u16, height: u16) {
        let mut message = vec![3, u8::from(incremental)];
        message.extend_from_slice(&0u16.to_be_bytes());
        message.extend_from_slice(&0u16.to_be_bytes());
        message.extend_from_slice(&width.to_be_bytes());
        message.extend_from_slice(&height.to_be_bytes());
        write_all(stream, &message).await;
    }

    async fn read_raw_update(stream: &mut TcpStream) -> (Vec<u8>, Vec<u8>) {
        let mut header = [0u8; 4];
        read_exact(stream, &mut header).await;
        assert_eq!(header[0], 0, "expected FramebufferUpdate");
        let rectangles = u16::from_be_bytes([header[2], header[3]]);
        assert!(rectangles <= 8);
        let mut pixels = Vec::new();
        for _ in 0..rectangles {
            let mut rect = [0u8; 12];
            read_exact(stream, &mut rect).await;
            let width = u16::from_be_bytes([rect[4], rect[5]]);
            let height = u16::from_be_bytes([rect[6], rect[7]]);
            assert_eq!(
                i32::from_be_bytes([rect[8], rect[9], rect[10], rect[11]]),
                ENCODING_RAW
            );
            let mut data = vec![0u8; usize::from(width) * usize::from(height) * 4];
            read_exact(stream, &mut data).await;
            pixels.extend_from_slice(&data);
        }
        (header.to_vec(), pixels)
    }

    async fn start_server(
        source: Box<dyn CaptureSource>,
        sink: SharedSink,
        max_clients: usize,
    ) -> VncServerHandle {
        let config = VncServerConfig {
            name: "Ozma test".to_owned(),
            max_clients,
            ..VncServerConfig::default()
        };
        start(config, source, sink).await.expect("server starts")
    }

    async fn start_server_with_password(
        source: Box<dyn CaptureSource>,
        sink: SharedSink,
        password: &[u8],
    ) -> VncServerHandle {
        let config = VncServerConfig {
            name: "Ozma test".to_owned(),
            max_clients: 1,
            password: Some(VncPassword::from_bytes(password).expect("valid test password")),
            ..VncServerConfig::default()
        };
        start(config, source, sink).await.expect("server starts")
    }

    async fn wait_for<F: Fn() -> bool>(predicate: F) {
        tokio::time::timeout(Duration::from_secs(2), async {
            while !predicate() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("condition was not reached");
    }

    #[tokio::test]
    async fn handshake_negotiates_none_security_and_real_server_init() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(4, 2)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        let (width, height, name) = handshake_client(&mut client).await;
        assert_eq!((width, height), (4, 2));
        assert!(String::from_utf8_lossy(&name).contains("Ozma test"));
        let snapshot = server.stats();
        assert_eq!(snapshot.clients_accepted, 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn password_auth_accepts_the_correct_des_response() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server =
            start_server_with_password(Box::new(TestSource::new(4, 2)), sink, b"hunter2").await;
        assert!(server.auth_enabled());
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        let (width, height, name) = handshake_client_with_password(&mut client, b"hunter2").await;
        assert_eq!((width, height), (4, 2));
        assert!(String::from_utf8_lossy(&name).contains("Ozma test"));

        // A successful auth still produces a normal framebuffer session.
        request_update(&mut client, false, 4, 2).await;
        let (_, pixels) = read_raw_update(&mut client).await;
        assert_eq!(pixels.len(), 4 * 2 * 4);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn password_auth_rejects_a_wrong_response_and_closes_without_retry() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server =
            start_server_with_password(Box::new(TestSource::new(4, 2)), sink, b"hunter2").await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();

        let mut version = [0u8; 12];
        read_exact(&mut client, &mut version).await;
        write_all(&mut client, b"RFB 003.008\n").await;
        let mut security = [0u8; 2];
        read_exact(&mut client, &mut security).await;
        assert_eq!(security, [1, SECURITY_TYPE_VNC_AUTH]);
        write_all(&mut client, &[SECURITY_TYPE_VNC_AUTH]).await;

        let mut challenge = [0u8; 16];
        read_exact(&mut client, &mut challenge).await;
        let mut response = client_vnc_response(b"hunter2", &challenge);
        response[0] ^= 0xff;
        write_all(&mut client, &response).await;

        let mut result = [0u8; 4];
        read_exact(&mut client, &mut result).await;
        assert_eq!(u32::from_be_bytes(result), 1, "SecurityResult must fail");
        // RFB 3.8 includes a reason string, then closes. There is no retry
        // challenge after a failure.
        let mut length = [0u8; 4];
        read_exact(&mut client, &mut length).await;
        let mut reason = vec![0u8; u32::from_be_bytes(length) as usize];
        read_exact(&mut client, &mut reason).await;
        assert!(!reason.is_empty());
        assert_connection_closed(&mut client).await;
        wait_for(|| server.stats().protocol_errors >= 1).await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn dynamic_source_geometry_is_reported_in_server_init() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        // QMP and D-Bus sources start with 0x0 and learn the real geometry
        // from their first frame. ServerInit must wait for that frame rather
        // than advertising a fabricated size.
        let server = start_server(
            Box::new(TestSource::new(0, 0).with_first_frame(3, 2).pause_after(1)),
            sink,
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        let (width, height, name) = handshake_client(&mut client).await;
        assert_eq!((width, height), (3, 2));
        assert!(String::from_utf8_lossy(&name).contains("Ozma test"));
        assert!(server.stats().frames_published >= 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn set_pixel_format_rgb_and_bgr_both_encode_raw_correctly() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(2, 1).solid(10, 20, 30)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        // RGB order, little endian: bytes are B,G,R,0.
        let mut set_format = vec![0u8, 0, 0, 0];
        set_format.extend_from_slice(&[32, 24, 0, 1]);
        set_format.extend_from_slice(&255u16.to_be_bytes());
        set_format.extend_from_slice(&255u16.to_be_bytes());
        set_format.extend_from_slice(&255u16.to_be_bytes());
        set_format.extend_from_slice(&[16, 8, 0, 0, 0, 0]);
        write_all(&mut client, &set_format).await;

        // Raw plus an unsupported encoding: the server ignores Hextile.
        write_all(&mut client, &[2, 0, 0, 2]).await;
        write_all(&mut client, &0i32.to_be_bytes()).await;
        write_all(&mut client, &5i32.to_be_bytes()).await;

        request_update(&mut client, false, 2, 1).await;
        let (_, pixels) = read_raw_update(&mut client).await;
        assert_eq!(pixels.len(), 2 * 4);
        assert_eq!(&pixels[0..4], &[30, 20, 10, 0]);

        // BGR order, little endian: bytes are R,G,B,0. SetPixelFormat may be
        // sent again on the same connection.
        let mut bgr = vec![0u8, 0, 0, 0];
        bgr.extend_from_slice(&[32, 24, 0, 1]);
        bgr.extend_from_slice(&255u16.to_be_bytes());
        bgr.extend_from_slice(&255u16.to_be_bytes());
        bgr.extend_from_slice(&255u16.to_be_bytes());
        bgr.extend_from_slice(&[0, 8, 16, 0, 0, 0]);
        write_all(&mut client, &bgr).await;
        request_update(&mut client, false, 2, 1).await;
        let (_, pixels) = read_raw_update(&mut client).await;
        assert_eq!(pixels.len(), 2 * 4);
        assert_eq!(&pixels[0..4], &[10, 20, 30, 0]);

        server.shutdown().await;
    }

    #[tokio::test]
    async fn full_and_incremental_requests_return_raw_frame_bytes() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(2, 2)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        request_update(&mut client, false, 2, 2).await;
        let (header, pixels) = read_raw_update(&mut client).await;
        assert_eq!(header[2..4], [0, 1]);
        assert_eq!(pixels.len(), 2 * 2 * 4);

        // Let the source advance, then request only new frames. The 25 ms
        // sleep used to race the 30 fps pump under parallel load, so poll a
        // bounded number of times until a genuinely newer frame arrives.
        tokio::time::sleep(Duration::from_millis(25)).await;
        let mut second = pixels.clone();
        for _ in 0..25 {
            request_update(&mut client, true, 2, 2).await;
            let (_, next) = read_raw_update(&mut client).await;
            assert_eq!(next.len(), 2 * 2 * 4);
            second = next;
            if second != pixels {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert_ne!(
            pixels, second,
            "incremental update should publish a newer frame"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn key_pointer_and_cut_text_dispatch_and_disconnect_releases() {
        let sink = Arc::new(FakeInputSink::new());
        let shared: SharedSink = sink.clone();
        let server = start_server(Box::new(TestSource::new(2, 2)), shared, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        // Key a down/up.
        write_all(&mut client, &[4, 1, 0, 0, 0, 0, 0, 0x61]).await;
        write_all(&mut client, &[4, 0, 0, 0, 0, 0, 0, 0x61]).await;
        // Pointer at the bottom-right, then left button down.
        write_all(&mut client, &[5, 0, 0, 1, 0, 1]).await;
        write_all(&mut client, &[5, 1, 0, 1, 0, 1]).await;
        // Cut text is ignored, and must not desynchronise the stream.
        write_all(&mut client, &[6, 0, 0, 0, 0, 0, 0, 3, b'x', b'y', b'z']).await;
        write_all(&mut client, &[4, 1, 0, 0, 0, 0, 0, 0x62]).await;

        wait_for(|| {
            sink.events().iter().any(|event| {
                matches!(
                    event,
                    InputEvent::Key {
                        code: 0x05,
                        down: true,
                        ..
                    }
                )
            })
        })
        .await;

        let events = sink.events();
        assert!(events.contains(&InputEvent::Key {
            code: 0x04,
            down: true,
            modifiers: Default::default(),
        }));
        assert!(events.contains(&InputEvent::Key {
            code: 0x04,
            down: false,
            modifiers: Default::default(),
        }));
        assert!(events.contains(&InputEvent::PointerAbsolute {
            display_id: None,
            x: 32_767,
            y: 32_767,
        }));
        assert!(events.contains(&InputEvent::Button {
            button: PointerButton::Left,
            down: true,
        }));
        assert!(sink.held().has_key(0x05));

        drop(client);
        wait_for(|| {
            sink.held().is_empty() && sink.events().last() == Some(&InputEvent::ReleaseAll)
        })
        .await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_viewer_that_never_sent_input_does_not_release_held_input() {
        let sink = Arc::new(FakeInputSink::new());
        sink.submit(&InputEvent::Key {
            code: 0x04,
            down: true,
            modifiers: Default::default(),
        })
        .unwrap();
        let server = start_server(Box::new(TestSource::new(2, 2)), sink.clone(), 1).await;

        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        let mut version = [0u8; 12];
        read_exact(&mut client, &mut version).await;
        drop(client);
        wait_for(|| server.stats().clients_disconnected >= 1).await;

        assert!(
            sink.held().has_key(0x04),
            "a handshake-only viewer must not release input it never sent"
        );
        sink.release_all().unwrap();
        server.shutdown().await;
    }

    #[tokio::test]
    async fn unsupported_pixel_format_closes_the_viewer_typed() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(2, 2)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        // 16-bit 5-6-5 is a valid RFB format, but not one this Raw encoder
        // claims to produce. Close instead of sending pixels in a format the
        // client did not ask for.
        let mut set_format = vec![0u8, 0, 0, 0];
        set_format.extend_from_slice(&[16, 16, 0, 1]);
        set_format.extend_from_slice(&31u16.to_be_bytes());
        set_format.extend_from_slice(&63u16.to_be_bytes());
        set_format.extend_from_slice(&31u16.to_be_bytes());
        set_format.extend_from_slice(&[11, 5, 0, 0, 0, 0]);
        write_all(&mut client, &set_format).await;

        let mut byte = [0u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(2), client.read(&mut byte)).await;
        match result {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            other => panic!("expected the connection to close, got {other:?}"),
        }
        wait_for(|| server.stats().protocol_errors >= 1).await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn client_cap_refuses_extra_viewers_and_releases_on_disconnect() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(2, 2)), sink, 1).await;

        let mut first = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut first).await;
        wait_for(|| server.stats().clients_connected == 1).await;

        let mut second = TcpStream::connect(server.bound_addr()).await.unwrap();
        let mut byte = [0u8; 1];
        let result = tokio::time::timeout(Duration::from_secs(2), second.read(&mut byte)).await;
        match result {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            other => panic!("expected the capped connection to close, got {other:?}"),
        }
        wait_for(|| server.stats().clients_rejected == 1).await;

        drop(first);
        wait_for(|| server.stats().clients_connected == 0).await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn slow_client_coalesces_intermediate_frames() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(512, 512)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        request_update(&mut client, false, 512, 512).await;
        let _ = read_raw_update(&mut client).await;

        // Do not ask for or read another update while the unpaced source
        // advances. The next request must receive the newest frame, and the
        // superseded generations are counted as dropped.
        tokio::time::sleep(Duration::from_millis(50)).await;
        request_update(&mut client, true, 512, 512).await;
        let _ = read_raw_update(&mut client).await;
        wait_for(|| server.stats().frames_coalesced > 0).await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn slow_viewer_hits_the_write_timeout_and_is_closed() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let config = VncServerConfig {
            name: "Ozma slow test".to_owned(),
            max_clients: 1,
            // Keep the failure fast in CI. A real deployment uses 10 s.
            write_timeout: Duration::from_millis(100),
            ..VncServerConfig::default()
        };
        // 2048x2048 RGB24 is a 16 MiB Raw update. A viewer that stops reading
        // after the request cannot drain it, so the write must time out.
        let server = start(
            config,
            Box::new(TestSource::new(2048, 2048).pause_after(1)),
            sink,
        )
        .await
        .expect("server starts");
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        request_update(&mut client, false, 2048, 2048).await;

        wait_for(|| server.stats().slow_clients >= 1).await;
        wait_for(|| server.stats().clients_connected == 0).await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn idle_timeout_closes_a_viewer_that_stops_sending_requests() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let config = VncServerConfig {
            name: "Ozma idle test".to_owned(),
            max_clients: 1,
            idle_timeout: Some(Duration::from_secs(1)),
            ..VncServerConfig::default()
        };
        let server = start(config, Box::new(TestSource::new(2, 2)), sink)
            .await
            .expect("server starts");
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        wait_for(|| server.stats().clients_connected == 1).await;

        // Send nothing after ClientInit. The read loop must close the session
        // once the idle window expires.
        wait_for(|| server.stats().clients_connected == 0).await;
        assert_connection_closed(&mut client).await;
        server.shutdown().await;
    }

    #[tokio::test]
    async fn non_loopback_bind_is_refused_without_explicit_configuration() {
        let config = VncServerConfig {
            bind: "0.0.0.0".parse().unwrap(),
            allow_non_loopback: false,
            ..VncServerConfig::default()
        };
        let error = start(
            config,
            Box::new(TestSource::new(2, 2)),
            Arc::new(FakeInputSink::new()),
        )
        .await
        .unwrap_err();
        assert!(matches!(error, VncServerError::WiderBindRefused { .. }));
    }

    #[tokio::test]
    async fn non_loopback_without_password_requires_the_explicit_override() {
        let refused = VncServerConfig {
            bind: "0.0.0.0".parse().unwrap(),
            allow_non_loopback: true,
            ..VncServerConfig::default()
        };
        let error = start(
            refused.clone(),
            Box::new(TestSource::new(2, 2)),
            Arc::new(FakeInputSink::new()),
        )
        .await
        .unwrap_err();
        assert!(matches!(
            error,
            VncServerError::UnauthenticatedWiderBindRefused { .. }
        ));

        // The explicit override is accepted...
        let override_config = VncServerConfig {
            allow_unauthenticated: true,
            ..refused.clone()
        };
        override_config
            .validate()
            .expect("the explicit unauthenticated override validates");

        // ...and so is a non-loopback bind with a configured password.
        let password_config = VncServerConfig {
            password: Some(VncPassword::from_bytes(b"secret").unwrap()),
            ..refused
        };
        password_config
            .validate()
            .expect("a password makes a non-loopback bind valid");
    }

    fn password_file_path(tag: &str) -> PathBuf {
        let nonce = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        std::env::temp_dir().join(format!(
            "ozma-vnc-password-{}-{nonce}-{tag}",
            std::process::id()
        ))
    }

    fn write_password_file(tag: &str, bytes: &[u8], mode: Option<u32>) -> PathBuf {
        let path = password_file_path(tag);
        std::fs::write(&path, bytes).expect("write test password file");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if let Some(mode) = mode {
                std::fs::set_permissions(&path, std::fs::Permissions::from_mode(mode))
                    .expect("set test password mode");
            }
        }
        #[cfg(not(unix))]
        let _ = mode;
        path
    }

    #[test]
    fn password_file_policy_trims_one_newline_and_rejects_empty_oversize_and_wide_modes() {
        let valid = write_password_file("valid", b"hunter2\n", Some(0o600));
        let password = VncPassword::from_file(&valid).expect("0600 password file loads");
        assert_eq!(format!("{password:?}"), "VncPassword(<redacted>)");
        assert!(!format!("{password:?}").contains("hunter2"));
        std::fs::remove_file(valid).unwrap();

        let crlf = write_password_file("crlf", b"hunter2\r\n", Some(0o600));
        VncPassword::from_file(&crlf).expect("CRLF is trimmed as one newline");
        std::fs::remove_file(crlf).unwrap();

        let empty = write_password_file("empty", b"", Some(0o600));
        assert!(matches!(
            VncPassword::from_file(&empty),
            Err(VncPasswordError::EmptyAt { .. })
        ));
        std::fs::remove_file(empty).unwrap();

        let newline_only = write_password_file("newline", b"\n", Some(0o600));
        assert!(matches!(
            VncPassword::from_file(&newline_only),
            Err(VncPasswordError::EmptyAt { .. })
        ));
        std::fs::remove_file(newline_only).unwrap();

        // One trailing newline is removed, so nine password bytes still fail.
        let oversized = write_password_file("oversized", b"123456789\n", Some(0o600));
        assert!(matches!(
            VncPassword::from_file(&oversized),
            Err(VncPasswordError::TooLongAt { .. })
        ));
        std::fs::remove_file(oversized).unwrap();

        // A file larger than the read guard is rejected before parsing.
        let huge = write_password_file("huge", &vec![b'x'; 8192], Some(0o600));
        assert!(matches!(
            VncPassword::from_file(&huge),
            Err(VncPasswordError::TooLong { .. })
        ));
        std::fs::remove_file(huge).unwrap();

        #[cfg(unix)]
        {
            let wide = write_password_file("wide", b"hunter2\n", Some(0o644));
            assert!(matches!(
                VncPassword::from_file(&wide),
                Err(VncPasswordError::Permissions { .. })
            ));
            std::fs::remove_file(wide).unwrap();

            // 0400 is stricter than 0600 and is accepted.
            let readonly = write_password_file("readonly", b"hunter2\n", Some(0o400));
            VncPassword::from_file(&readonly).expect("0400 is stricter than 0600");
            std::fs::remove_file(readonly).unwrap();
        }
    }

    #[test]
    fn des_known_answer_and_rfb_key_expansion_are_correct() {
        // FIPS/classic DES known-answer vector.
        let key = [0x13, 0x34, 0x57, 0x79, 0x9b, 0xbc, 0xdf, 0xf1];
        let block = [0x01, 0x23, 0x45, 0x67, 0x89, 0xab, 0xcd, 0xef];
        assert_eq!(
            vnc_des::encrypt_block(&key, &block),
            [0x85, 0xe8, 0x13, 0x54, 0x0f, 0x0a, 0xb4, 0x05]
        );

        // "password" bit-reversed per byte is the classic VNC DES key.
        let password = VncPassword::from_bytes(b"password").unwrap();
        assert_eq!(
            password.key(),
            &[0x0e, 0x86, 0xce, 0xce, 0xee, 0xf6, 0x4e, 0x26]
        );

        assert!(constant_time_eq(b"same", b"same"));
        assert!(!constant_time_eq(b"same", b"diff"));
        assert!(!constant_time_eq(b"short", b"longer"));
    }

    #[tokio::test]
    async fn password_is_redacted_from_debug_and_status_surfaces() {
        let password = VncPassword::from_bytes(b"hunter2").unwrap();
        let config = VncServerConfig {
            password: Some(password.clone()),
            ..VncServerConfig::default()
        };
        let debug = format!("{config:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");

        let options = VncServerOptions {
            password: Some(password),
            ..VncServerOptions::default()
        };
        let debug = format!("{options:?}");
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(debug.contains("<redacted>"), "{debug}");

        let server = start_server_with_password(
            Box::new(TestSource::new(2, 2)),
            Arc::new(FakeInputSink::new()),
            b"hunter2",
        )
        .await;
        let debug = format!("{server:?}");
        assert!(server.auth_enabled());
        assert!(!debug.contains("hunter2"), "{debug}");
        assert!(!format!("{:?}", server.stats()).contains("hunter2"));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn disabled_or_sourceless_config_never_binds_a_fake_listener() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let registry = CaptureRegistry::new();

        let disabled = start_if_configured(
            false,
            VncServerOptions {
                bind: Some("0.0.0.0".to_owned()),
                port: 1,
                ..VncServerOptions::default()
            },
            &registry,
            sink.clone(),
            "test",
        )
        .await
        .expect("disabled is not an error");
        assert!(disabled.is_none());

        let error = start_if_configured(true, VncServerOptions::default(), &registry, sink, "test")
            .await
            .unwrap_err();
        assert!(matches!(error, VncServerError::NoCaptureSource));
    }

    #[tokio::test]
    async fn registration_reports_the_bound_endpoint_only_while_listening() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(2, 2)), sink, 1).await;
        let (host, port) = server.registration_endpoint("192.0.2.55");

        let extras = crate::registration::RegistrationExtras::resolve_with_served_vnc(
            None,
            Some((&host, port)),
            None,
            None,
            None,
        );
        assert_eq!(
            extras.vnc,
            crate::registration::VncRegistration::Configured {
                host: host.clone(),
                port
            }
        );

        let state = crate::controller_compat::ConnectionState::new(
            crate::controller_compat::ConnectionInfo::new(
                "node._ozma._udp.local.",
                "vm",
                7382,
                7384,
                7331,
                crate::controller_compat::SINK_FAKE_RECORDING,
            )
            .with_registration(extras),
            crate::controller_compat::CompatStats::default(),
        );
        let app = crate::controller_compat::connection_router(state);
        let response = tower::ServiceExt::oneshot(
            app,
            axum::http::Request::builder()
                .uri("/api/v1/connection")
                .body(axum::body::Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
        let body = http_body_util::BodyExt::collect(response.into_body())
            .await
            .unwrap()
            .to_bytes();
        let json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(json["registration"]["vnc"]["state"], "configured");
        assert_eq!(json["registration"]["vnc"]["host"], host);
        assert_eq!(json["registration"]["vnc"]["port"], port);

        server.shutdown().await;

        let after = crate::registration::RegistrationExtras::resolve(None, None, None, None);
        assert_eq!(
            after.vnc,
            crate::registration::VncRegistration::NotConfigured
        );
        assert!(after
            .unsupported_controller_fields
            .contains(&"vnc_host".to_owned()));
    }

    #[test]
    fn version_parsing_rejects_malformed_or_unsupported_strings_without_overflow() {
        assert_eq!(parse_version(b"RFB 003.008\n").unwrap(), 8);
        assert_eq!(parse_version(b"RFB 003.007\n").unwrap(), 7);
        assert_eq!(parse_version(b"RFB 003.003\n").unwrap(), 3);
        assert!(parse_version(b"RFB 999.999\n").is_err());
        assert!(parse_version(b"RFB 003.006\n").is_err());
        assert!(parse_version(b"NOT 003.008\n").is_err());
    }

    #[test]
    fn keysym_mapping_covers_ascii_and_named_keys() {
        assert_eq!(keysym_to_hid(0x61), Some(0x04)); // a
        assert_eq!(keysym_to_hid(0x41), Some(0x04)); // A
        assert_eq!(keysym_to_hid(0x31), Some(0x1e)); // 1
        assert_eq!(keysym_to_hid(0x21), Some(0x1e)); // !
        assert_eq!(keysym_to_hid(0xff0d), Some(0x28)); // Return
        assert_eq!(keysym_to_hid(0xff51), Some(0x50)); // Left
        assert_eq!(keysym_to_hid(0xffe1), Some(0xe1)); // Shift_L
        assert_eq!(keysym_to_hid(0xffbe), Some(0x3a)); // F1
        assert_eq!(keysym_to_hid(0x0101), None); // not claimed
    }

    #[test]
    fn pixel_format_validation_is_typed() {
        let mut rgb = [0u8; 16];
        rgb[0] = 32;
        rgb[1] = 24;
        rgb[3] = 1;
        rgb[4..6].copy_from_slice(&255u16.to_be_bytes());
        rgb[6..8].copy_from_slice(&255u16.to_be_bytes());
        rgb[8..10].copy_from_slice(&255u16.to_be_bytes());
        rgb[10] = 16;
        rgb[11] = 8;
        assert_eq!(
            ClientPixelFormat::parse(&rgb),
            Ok(ClientPixelFormat::default())
        );

        let mut bgr = rgb;
        bgr[10] = 0;
        bgr[12] = 16;
        assert_eq!(
            ClientPixelFormat::parse(&bgr),
            Ok(ClientPixelFormat {
                big_endian: false,
                red_shift: 0,
                green_shift: 8,
                blue_shift: 16,
            })
        );

        let mut sixteen = [0u8; 16];
        sixteen[0] = 16;
        sixteen[1] = 16;
        sixteen[3] = 1;
        assert!(ClientPixelFormat::parse(&sixteen).is_err());
    }

    #[test]
    fn geometry_and_frame_bounds_are_enforced() {
        assert!(validate_source_format(FrameFormat::rgb24(4, 2)).is_ok());
        assert!(validate_source_format(FrameFormat::new(PixelFormat::Yuyv, 4, 2)).is_err());
        // A dynamic source may start at 0x0, but the first concrete frame may
        // not be zero-sized or over the bound.
        assert!(validate_source_format(FrameFormat::rgb24(0, 2)).is_ok());
        assert!(validate_concrete_geometry(FrameFormat::rgb24(0, 2)).is_err());
        assert!(validate_source_format(FrameFormat::rgb24(u32::from(u16::MAX) + 1, 1)).is_err());
        assert!(
            validate_concrete_geometry(FrameFormat::rgb24(u32::from(u16::MAX) + 1, 1)).is_err()
        );
    }
}
