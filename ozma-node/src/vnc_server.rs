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
//! * Five pixel encodings are available: Raw (always valid in RFB), ZRLE
//!   (16), Hextile (5), ZLib (6), and Tight (7), selected from the client's
//!   `SetEncodings` preference order. ZRLE, ZLib, and Tight each use their own
//!   per-client zlib stream; Hextile uses the standard 16x16 tiles and its
//!   Raw, BackgroundColour, ForegroundColour, AnySubrects, and
//!   SubrectsColoured sub-encodings. Tight sends basic zlib compression with
//!   the Fill sub-encoding for solid rectangles, the Palette filter for
//!   rectangles with at most 256 colours, and the Gradient predictor for
//!   high-colour rectangles when it beats the copy filter. When the client
//!   advertises a Tight JPEG quality-level pseudo-encoding, high-colour
//!   rectangles may instead use the JpegCompression sub-encoding when the JPEG
//!   payload is smaller than the lossless one. The server never sends an
//!   encoding or sub-encoding the client did not advertise, falls back to Raw
//!   if an encode fails, disables only the failing encoding or JPEG sub-path
//!   for the rest of the connection, and never claims a codec it does not
//!   implement.
//! * A client may request the Cursor pseudo-encoding (-239). Because this
//!   server has no cursor-shape source, it sends a typed 0x0 empty cursor
//!   once per connection; it never fabricates a visible pointer shape.
//! * Unsupported pixel formats close the connection, because RFB has no
//!   typed per-message error and silently continuing would lie about the
//!   format the client asked for.
//! * Key and pointer events go to the node's existing [`InputSink`]. Unknown
//!   keysyms are dropped with a diagnostic. Every disconnect calls
//!   `release_all` so a vanished viewer cannot leave a guest key or button held.
//! * Clipboard bridging is off by default. With `--vnc-clipboard`, a
//!   desktop-profile X11 node writes viewer `ClientCutText` through the same
//!   bounded `xclip`/`xsel` path as `/clipboard`, and sends one `ServerCutText`
//!   snapshot to each viewer on connect. Unavailable, oversized, or non-Latin-1
//!   text is ignored rather than claimed as synced, and a viewer's own cut text
//!   is never echoed back.
//!
//! The controller sees the real bound endpoint only while the listener is
//! running. [`start_if_configured`] does not invent a listener: with no capture
//! source it returns [`VncServerError::NoCaptureSource`], and the caller keeps
//! the registration state at `not-configured`.

use std::collections::HashMap;
use std::fmt;
use std::io;
use std::io::Write as _;
use std::net::{IpAddr, SocketAddr};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use async_trait::async_trait;

use flate2::write::ZlibEncoder;
use flate2::Compression;
use jpeg_encoder::{ColorType as JpegColorType, Encoder as JpegEncoder};
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
/// Default bound for clipboard text bridged between RFB and the node clipboard.
/// This matches the desktop clipboard route's own limit so a viewer cannot
/// push more text than `POST /clipboard` would accept.
pub const DEFAULT_VNC_CLIPBOARD_BYTES: usize = 64 * 1024;
/// Largest configurable clipboard bridge bound. The desktop clipboard path is
/// itself bounded at [`DEFAULT_VNC_CLIPBOARD_BYTES`], so a larger VNC value
/// would only produce a typed write failure.
pub const MAX_VNC_CLIPBOARD_BYTES: usize = DEFAULT_VNC_CLIPBOARD_BYTES;
/// Maximum entries accepted in one `SetEncodings` message.
pub const MAX_ENCODINGS: usize = 1024;
/// Maximum pending framebuffer rectangles merged for one client update.
const MAX_PENDING_RECTS: usize = 8;
/// Command channel bound. A flood of update requests is back-pressured.
const CLIENT_COMMAND_QUEUE: usize = 64;
/// RFB Raw encoding number.
pub const ENCODING_RAW: i32 = 0;
/// RFB Hextile encoding number (RFC 6143 section 7.7.4).
pub const ENCODING_HEXTILE: i32 = 5;
/// RFB ZLib encoding number (RFC 6143 section 7.7.7).
pub const ENCODING_ZLIB: i32 = 6;
/// RFB Tight encoding number (TightVNC extension).
pub const ENCODING_TIGHT: i32 = 7;
/// RFB ZRLE encoding number (RFC 6143 section 7.7.6).
pub const ENCODING_ZRLE: i32 = 16;
/// RFB Cursor pseudo-encoding number (RFC 6143 section 7.8.1).
pub const ENCODING_CURSOR: i32 = -239;
/// RFB ExtendedDesktopSize pseudo-encoding number (the RFB community
/// specification used by TigerVNC and libvncserver).
pub const ENCODING_EXTENDED_DESKTOP_SIZE: i32 = -308;
/// RFB `SetDesktopSize` client message type.
pub const CLIENT_MESSAGE_SET_DESKTOP_SIZE: u8 = 251;
/// Largest number of screens accepted in one `SetDesktopSize` request. The
/// wire count is a `u8`, but a real single-source node has one screen; this
/// bound keeps the parser and any future layout honest.
pub const MAX_DESKTOP_SCREENS: usize = 16;
/// ExtendedDesktopSize reason: the server changed the framebuffer size.
const DESKTOP_SIZE_REASON_SERVER: u16 = 0;
/// ExtendedDesktopSize reason: this rectangle answers a client
/// `SetDesktopSize` request.
const DESKTOP_SIZE_REASON_CLIENT: u16 = 1;

/// Typed outcome carried by an ExtendedDesktopSize rectangle's `result` field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DesktopSizeResult {
    /// The layout was applied. This server never uses it today because the
    /// capture source has no resize API.
    Success = 0,
    /// The server refused the layout (the normal answer here).
    Prohibited = 1,
    /// The requested geometry is valid but exceeds server resources.
    OutOfResources = 2,
    /// Zero-sized, screenless, or otherwise malformed layout.
    InvalidScreenLayout = 3,
}

impl DesktopSizeResult {
    fn code(self) -> u16 {
        self as u16
    }
}

/// Tight JPEG quality-level pseudo-encoding range. `-32` is the lowest
/// (fastest/smallest) quality and `-23` the highest.
pub const ENCODING_JPEG_QUALITY_LOW: i32 = -32;
/// Highest Tight JPEG quality-level pseudo-encoding.
pub const ENCODING_JPEG_QUALITY_HIGH: i32 = -23;
/// Tight compression-level pseudo-encoding range. `-256` is the lowest
/// (fastest) compression level and `-247` the highest.
pub const ENCODING_COMPRESSION_LEVEL_LOW: i32 = -256;
/// Highest Tight compression-level pseudo-encoding.
pub const ENCODING_COMPRESSION_LEVEL_HIGH: i32 = -247;
/// ZRLE tile edge in pixels.
const ZRLE_TILE_SIZE: usize = 64;
/// Hextile tile edge in pixels.
const HEXTILE_TILE_SIZE: usize = 16;
/// Hextile subencoding bit: a raw tile follows.
const HEXTILE_RAW: u8 = 1;
/// Hextile subencoding bit: a background PIXEL follows.
const HEXTILE_BACKGROUND_SPECIFIED: u8 = 2;
/// Hextile subencoding bit: a foreground PIXEL follows.
const HEXTILE_FOREGROUND_SPECIFIED: u8 = 4;
/// Hextile subencoding bit: a subrect count and subrects follow.
const HEXTILE_ANY_SUBRECTS: u8 = 8;
/// Hextile subencoding bit: each subrect carries its own PIXEL.
const HEXTILE_SUBRECTS_COLOURED: u8 = 16;
/// Largest palette ZRLE may send as a packed palette sub-encoding.
const ZRLE_MAX_PACKED_PALETTE: usize = 16;
/// Largest palette ZRLE may send as a palette-RLE sub-encoding.
const ZRLE_MAX_RLE_PALETTE: usize = 127;
/// Bound on one rectangle's compressed ZRLE payload. A 16-megapixel frame
/// is at most 64 MiB as four-byte pixels; deflate can expand stored data by a
/// small constant factor, so leave headroom rather than trusting the length.
const MAX_ZRLE_RECT_BYTES: usize = MAX_FRAME_BYTES + 8 * 1024 * 1024;
/// Bound on one rectangle's compressed ZLib payload. The ZLib encoding carries
/// the same uncompressed rectangle as Raw, so the same headroom over a
/// four-byte-per-pixel frame is enough for deflate's stored-data overhead.
const MAX_ZLIB_RECT_BYTES: usize = MAX_FRAME_BYTES + 8 * 1024 * 1024;
/// Tight rectangles wider than this must be split: TightVNC decoders size
/// their row buffers from this 2048-pixel limit.
const TIGHT_MAX_WIDTH: usize = 2048;
/// Largest pixel area encoded as one Tight wire rectangle. At four bytes per
/// TPIXEL this keeps the uncompressed payload at 4,000,000 bytes, so even
/// incompressible deflate output stays below [`TIGHT_MAX_RECT_BYTES`].
const TIGHT_MAX_PIXELS: usize = 1_000_000;
/// Largest Tight payload representable by the compact 22-bit length field
/// (one, two, or three bytes).
const TIGHT_MAX_RECT_BYTES: usize = 0x003f_ffff;
/// Tight compression-control byte: FillCompression (all pixels one colour).
const TIGHT_FILL: u8 = 0x80;
/// Tight compression-control byte: JpegCompression.
const TIGHT_JPEG: u8 = 0x90;
/// Tight compression-control bit: an explicit filter-id byte follows.
const TIGHT_EXPLICIT_FILTER: u8 = 0x40;
/// Tight filter id: palette (indexed colour) filter.
const TIGHT_FILTER_PALETTE: u8 = 1;
/// Tight filter id: gradient predictor filter.
const TIGHT_FILTER_GRADIENT: u8 = 2;
/// Tight sends filtered data below this size without zlib compression.
const TIGHT_MIN_TO_COMPRESS: usize = 12;
/// Largest palette Tight emits; the palette size byte stores `len - 1`.
const TIGHT_MAX_PALETTE: usize = 256;
/// Default zlib level for Tight when the client does not request one.
const TIGHT_DEFAULT_COMPRESSION: u8 = 6;
/// The gradient filter must beat the copy filter by this percentage of the
/// copy payload's compressed size. Choosing only clear wins keeps near-ties on
/// noisy rectangles on the cheaper copy path.
const TIGHT_GRADIENT_WIN_PERCENT: usize = 95;
/// How long a viewer may take to complete the handshake.
pub const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(5);
/// How long one framebuffer update write may block before the viewer is closed.
pub const WRITE_TIMEOUT: Duration = Duration::from_secs(10);
/// Cooperative shutdown window before the listener and capture pump are
/// aborted. This is enough for `CaptureSource::stop` to run.
pub const SHUTDOWN_GRACE: Duration = Duration::from_secs(3);

/// Clipboard backend used by the opt-in RFB clipboard bridge.
///
/// The server only ever sees text through this trait, so the runtime wiring can
/// supply the desktop-profile X11 clipboard or (in tests) a fake. A backend that
/// has no display or no `xclip`/`xsel` reports `available() == false`; the server
/// then ignores cut text instead of claiming a sync.
#[async_trait]
pub trait ClipboardAccess: Send + Sync {
    /// Whether a usable clipboard backend is currently advertised.
    fn available(&self) -> bool;
    /// Read the current UTF-8 clipboard text.
    async fn read_text(&self) -> Result<String, ClipboardAccessError>;
    /// Write UTF-8 text and return the number of bytes accepted.
    async fn write_text(&self, text: String) -> Result<usize, ClipboardAccessError>;
}

/// Typed clipboard-backend failure. The text itself is never carried here.
#[derive(Clone, Debug, thiserror::Error)]
#[error("{code}: {message}")]
pub struct ClipboardAccessError {
    /// Stable code from the desktop control layer (for example
    /// `desktop.no_display`).
    pub code: String,
    /// Human-readable detail; never clipboard contents.
    pub message: String,
}

impl ClipboardAccessError {
    /// True when the failure means no clipboard backend is usable at all, as
    /// opposed to a transient command failure.
    fn unavailable(&self) -> bool {
        matches!(
            self.code.as_str(),
            "desktop.no_display" | "desktop.tool_missing" | "desktop.not_enabled"
        )
    }
}

/// Adapter from the desktop-profile X11 clipboard to [`ClipboardAccess`].
///
/// The underlying [`crate::desktop_compat::DesktopControls`] already enforces a
/// UTF-8-only, NUL-free, bounded payload and keeps exactly one selection owner.
/// This adapter adds no new process boundary.
#[derive(Clone, Debug)]
pub struct DesktopClipboard {
    controls: crate::desktop_compat::DesktopControls,
}

impl DesktopClipboard {
    /// Wrap the desktop controls attached to this node.
    pub fn new(controls: crate::desktop_compat::DesktopControls) -> Self {
        Self { controls }
    }
}

fn desktop_clipboard_error(error: crate::desktop_compat::DesktopError) -> ClipboardAccessError {
    let api = error.api_error();
    ClipboardAccessError {
        code: api.code,
        message: api.message,
    }
}

#[async_trait]
impl ClipboardAccess for DesktopClipboard {
    fn available(&self) -> bool {
        self.controls.status().clipboard.available
    }

    async fn read_text(&self) -> Result<String, ClipboardAccessError> {
        self.controls
            .clipboard()
            .await
            .map(|read| read.text)
            .map_err(desktop_clipboard_error)
    }

    async fn write_text(&self, text: String) -> Result<usize, ClipboardAccessError> {
        self.controls
            .set_clipboard(text)
            .await
            .map(|write| write.bytes)
            .map_err(desktop_clipboard_error)
    }
}

/// Direction of the last successful clipboard transfer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
#[repr(u8)]
pub enum ClipboardSyncDirection {
    /// No clipboard transfer has succeeded.
    #[default]
    None = 0,
    /// A viewer's `ClientCutText` was written to the node clipboard.
    ViewerToNode = 1,
    /// The node clipboard was sent to a viewer as `ServerCutText`.
    NodeToViewer = 2,
}

impl ClipboardSyncDirection {
    fn from_u8(value: u8) -> Self {
        match value {
            1 => Self::ViewerToNode,
            2 => Self::NodeToViewer,
            _ => Self::None,
        }
    }
}

/// Controller-facing clipboard bridge status. It deliberately exposes only
/// availability, direction, time, and counters, never the text.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct VncClipboardStatus {
    /// The operator enabled the bridge (`--vnc-clipboard`).
    pub enabled: bool,
    /// A backend is currently usable.
    pub available: bool,
    /// Direction of the last successful transfer.
    pub last_direction: ClipboardSyncDirection,
    /// Unix time in milliseconds of the last successful transfer; `0` means
    /// none has succeeded.
    pub last_sync_unix_ms: u64,
    /// Successful viewer-to-node transfers.
    pub from_viewers: u64,
    /// Successful node-to-viewer transfers (initial `ServerCutText`).
    pub to_viewers: u64,
    /// Cut-text messages or clipboard reads rejected by a bound or failure.
    pub rejected: u64,
}

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
    /// Optional clipboard bridge. `None` keeps `ClientCutText` ignored and sends
    /// no `ServerCutText`, which is the default.
    pub clipboard: Option<Arc<dyn ClipboardAccess>>,
    /// Bound for clipboard text in either direction.
    pub clipboard_max_bytes: usize,
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
            .field(
                "clipboard",
                &self.clipboard.as_ref().map(|_| "<configured>"),
            )
            .field("clipboard_max_bytes", &self.clipboard_max_bytes)
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
            clipboard: None,
            clipboard_max_bytes: DEFAULT_VNC_CLIPBOARD_BYTES,
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
        if self.clipboard_max_bytes == 0 || self.clipboard_max_bytes > MAX_VNC_CLIPBOARD_BYTES {
            return Err(VncServerError::InvalidClipboardBound {
                requested: self.clipboard_max_bytes,
                max: MAX_VNC_CLIPBOARD_BYTES,
            });
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
    /// Clipboard bound outside the supported range.
    #[error("VNC clipboard bound must be between 1 and {max} bytes, got {requested}")]
    InvalidClipboardBound { requested: usize, max: usize },
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

    /// Clipboard bridge status. Never includes clipboard text.
    pub fn clipboard_status(&self) -> VncClipboardStatus {
        self.stats.clipboard_status()
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
    /// Accepted capture-source geometry changes published to viewers.
    pub framebuffer_resizes: u64,
    /// ExtendedDesktopSize server resize rectangles written to viewers.
    pub desktop_resize_sent: u64,
    /// `SetDesktopSize` client messages received.
    pub desktop_resize_requests: u64,
    /// `SetDesktopSize` client messages denied because the capture source
    /// cannot be resized by VNC.
    pub desktop_resize_denied: u64,
    /// The clipboard bridge is configured (`--vnc-clipboard`).
    pub clipboard_enabled: bool,
    /// A clipboard backend is currently usable. Always `false` when disabled.
    pub clipboard_available: bool,
    /// Direction of the last successful clipboard transfer.
    pub clipboard_last_direction: ClipboardSyncDirection,
    /// Unix time in milliseconds of the last successful transfer; `0` is never.
    pub clipboard_last_sync_unix_ms: u64,
    /// Successful viewer-to-node transfers.
    pub clipboard_from_viewers: u64,
    /// Successful node-to-viewer transfers.
    pub clipboard_to_viewers: u64,
    /// Cut-text messages or clipboard reads rejected by bound or failure.
    pub clipboard_rejected: u64,
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
    framebuffer_resizes: AtomicU64,
    desktop_resize_sent: AtomicU64,
    desktop_resize_requests: AtomicU64,
    desktop_resize_denied: AtomicU64,
    generation: AtomicU64,
    clipboard_enabled: bool,
    clipboard_available: AtomicBool,
    clipboard_last_direction: AtomicU64,
    clipboard_last_sync_unix_ms: AtomicU64,
    clipboard_from_viewers: AtomicU64,
    clipboard_to_viewers: AtomicU64,
    clipboard_rejected: AtomicU64,
}

impl VncStats {
    fn new(clipboard_enabled: bool, clipboard_available: bool) -> Self {
        Self {
            clipboard_enabled,
            clipboard_available: AtomicBool::new(clipboard_available),
            ..Self::default()
        }
    }

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
            framebuffer_resizes: self.framebuffer_resizes.load(Ordering::Relaxed),
            desktop_resize_sent: self.desktop_resize_sent.load(Ordering::Relaxed),
            desktop_resize_requests: self.desktop_resize_requests.load(Ordering::Relaxed),
            desktop_resize_denied: self.desktop_resize_denied.load(Ordering::Relaxed),
            clipboard_enabled: self.clipboard_enabled,
            clipboard_available: self.clipboard_available.load(Ordering::Relaxed),
            clipboard_last_direction: ClipboardSyncDirection::from_u8(
                self.clipboard_last_direction.load(Ordering::Relaxed) as u8,
            ),
            clipboard_last_sync_unix_ms: self.clipboard_last_sync_unix_ms.load(Ordering::Relaxed),
            clipboard_from_viewers: self.clipboard_from_viewers.load(Ordering::Relaxed),
            clipboard_to_viewers: self.clipboard_to_viewers.load(Ordering::Relaxed),
            clipboard_rejected: self.clipboard_rejected.load(Ordering::Relaxed),
        }
    }

    fn clipboard_status(&self) -> VncClipboardStatus {
        VncClipboardStatus {
            enabled: self.clipboard_enabled,
            available: self.clipboard_available.load(Ordering::Relaxed),
            last_direction: ClipboardSyncDirection::from_u8(
                self.clipboard_last_direction.load(Ordering::Relaxed) as u8,
            ),
            last_sync_unix_ms: self.clipboard_last_sync_unix_ms.load(Ordering::Relaxed),
            from_viewers: self.clipboard_from_viewers.load(Ordering::Relaxed),
            to_viewers: self.clipboard_to_viewers.load(Ordering::Relaxed),
            rejected: self.clipboard_rejected.load(Ordering::Relaxed),
        }
    }

    fn record_clipboard_sync(&self, direction: ClipboardSyncDirection) {
        self.clipboard_last_direction
            .store(direction as u8 as u64, Ordering::Relaxed);
        self.clipboard_last_sync_unix_ms.store(
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |elapsed| elapsed.as_millis() as u64),
            Ordering::Relaxed,
        );
        match direction {
            ClipboardSyncDirection::ViewerToNode => {
                self.clipboard_from_viewers.fetch_add(1, Ordering::Relaxed);
            }
            ClipboardSyncDirection::NodeToViewer => {
                self.clipboard_to_viewers.fetch_add(1, Ordering::Relaxed);
            }
            ClipboardSyncDirection::None => {}
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
#[derive(Clone)]
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
    /// Optional clipboard backend. Only installed when `--vnc-clipboard` was
    /// requested and the node runs the desktop profile with an X11 display.
    pub clipboard: Option<Arc<dyn ClipboardAccess>>,
    /// Bound for clipboard text in either direction.
    pub clipboard_max_bytes: usize,
}

impl fmt::Debug for VncServerOptions {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("VncServerOptions")
            .field("bind", &self.bind)
            .field("port", &self.port)
            .field("password", &self.password.as_ref().map(|_| "<redacted>"))
            .field("max_clients", &self.max_clients)
            .field("idle_timeout", &self.idle_timeout)
            .field("allow_unauthenticated", &self.allow_unauthenticated)
            .field(
                "clipboard",
                &self.clipboard.as_ref().map(|_| "<configured>"),
            )
            .field("clipboard_max_bytes", &self.clipboard_max_bytes)
            .finish()
    }
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
            clipboard: None,
            clipboard_max_bytes: DEFAULT_VNC_CLIPBOARD_BYTES,
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
    config.clipboard = options.clipboard;
    config.clipboard_max_bytes = options.clipboard_max_bytes;

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
    let clipboard_enabled = config.clipboard.is_some();
    let clipboard_available = config
        .clipboard
        .as_ref()
        .is_some_and(|clipboard| clipboard.available());
    let config = Arc::new(config);
    let stats = Arc::new(VncStats::new(clipboard_enabled, clipboard_available));
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
        config.clone(),
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
        encoding = "raw,hextile,zlib,tight,zrle",
        clipboard = if !clipboard_enabled {
            "disabled"
        } else if clipboard_available {
            "available"
        } else {
            "unavailable"
        },
        clipboard_max_bytes = config.clipboard_max_bytes,
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
                let actual = frame.format();
                // The pixel layout never changes for a session. A source that
                // switches between packed RGB24 and BGRA cannot be reinterpreted
                // without a decoder, so keep dropping those frames.
                if actual.pixel_format != expected.pixel_format {
                    stats.source_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        expected = %expected.pixel_format,
                        actual = %actual.pixel_format,
                        "dropping VNC frame whose pixel format differs from the source"
                    );
                    continue;
                }
                // Zero-sized, over-16-megapixel, or out-of-RFB-range geometry is
                // ignored rather than published. This also covers the first
                // dynamic QMP/D-Bus frame.
                if let Err(error) = validate_concrete_geometry(actual) {
                    stats.source_errors.fetch_add(1, Ordering::Relaxed);
                    warn!(
                        error = %error,
                        width = actual.width,
                        height = actual.height,
                        "dropping VNC frame with unusable geometry"
                    );
                    continue;
                }
                if expected.width == 0 || expected.height == 0 {
                    info!(
                        width = actual.width,
                        height = actual.height,
                        "VNC capture source published its first frame; ServerInit geometry fixed"
                    );
                } else if actual.width != expected.width || actual.height != expected.height {
                    // The capture source resized. Publish the new geometry; each
                    // viewer either receives ExtendedDesktopSize (if it
                    // advertised -308) or keeps its ServerInit geometry and sees
                    // only the clipped overlap.
                    info!(
                        old_width = expected.width,
                        old_height = expected.height,
                        width = actual.width,
                        height = actual.height,
                        "VNC capture source resized; publishing new framebuffer geometry"
                    );
                    stats.framebuffer_resizes.fetch_add(1, Ordering::Relaxed);
                }
                expected = actual;
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
                let clipboard = config.clipboard.clone();
                let clipboard_max_bytes = config.clipboard_max_bytes;
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
                        clipboard,
                        clipboard_max_bytes,
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
    /// Geometry last announced to this viewer (ServerInit or the most recent
    /// ExtendedDesktopSize rectangle). A client that did not advertise -308
    /// keeps its ServerInit size forever and sees only the overlapping pixels.
    width: AtomicU32,
    height: AtomicU32,
    sink: SharedSink,
    stats: Arc<VncStats>,
    input_active: Arc<AtomicBool>,
    clipboard: Option<Arc<dyn ClipboardAccess>>,
    clipboard_max_bytes: usize,
    pixel: Mutex<ClientPixelFormat>,
    encoding: Mutex<ClientEncodingState>,
}

impl ClientContext {
    fn geometry(&self) -> (u32, u32) {
        (
            self.width.load(Ordering::Acquire),
            self.height.load(Ordering::Acquire),
        )
    }

    fn set_geometry(&self, width: u32, height: u32) {
        self.width.store(width, Ordering::Release);
        self.height.store(height, Ordering::Release);
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClientPixelFormat {
    depth: u8,
    big_endian: bool,
    red_shift: u8,
    green_shift: u8,
    blue_shift: u8,
}

impl Default for ClientPixelFormat {
    fn default() -> Self {
        Self {
            depth: 24,
            big_endian: false,
            red_shift: 16,
            green_shift: 8,
            blue_shift: 0,
        }
    }
}

/// The rectangle encoding chosen from the client's `SetEncodings` order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum UpdateEncoding {
    Raw,
    Hextile,
    ZLib,
    Tight,
    Zrle,
}

/// Encodings disabled for the rest of one connection after an encode error.
/// Raw is never disabled because it is always a legal fallback.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct DisabledEncodings {
    hextile: bool,
    zlib: bool,
    tight: bool,
    zrle: bool,
}

impl DisabledEncodings {
    fn is_disabled(self, encoding: UpdateEncoding) -> bool {
        match encoding {
            UpdateEncoding::Raw => false,
            UpdateEncoding::Hextile => self.hextile,
            UpdateEncoding::ZLib => self.zlib,
            UpdateEncoding::Tight => self.tight,
            UpdateEncoding::Zrle => self.zrle,
        }
    }

    fn disable(&mut self, encoding: UpdateEncoding) {
        match encoding {
            UpdateEncoding::Raw => {}
            UpdateEncoding::Hextile => self.hextile = true,
            UpdateEncoding::ZLib => self.zlib = true,
            UpdateEncoding::Tight => self.tight = true,
            UpdateEncoding::Zrle => self.zrle = true,
        }
    }
}

/// Per-client encoding preferences: the supported encodings in the order the
/// client listed them, plus whether the Cursor pseudo-encoding was requested
/// and the Tight JPEG-quality/compression-level hints.
/// An empty list (or a list with no supported entry) selects Raw.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct ClientEncodingState {
    order: [UpdateEncoding; 5],
    count: usize,
    cursor: bool,
    /// The client advertised ExtendedDesktopSize (-308), so the server may
    /// send desktop-size rectangles and answer `SetDesktopSize` on the wire.
    extended_desktop_size: bool,
    /// Tight JPEG quality level 0..=9. `None` means the client did not
    /// advertise a JPEG quality pseudo-encoding, so JPEG must never be sent.
    jpeg_quality: Option<u8>,
    /// Tight zlib compression level 0..=9. `None` uses the server default.
    compression_level: Option<u8>,
}

impl Default for ClientEncodingState {
    fn default() -> Self {
        Self {
            order: [UpdateEncoding::Raw; 5],
            count: 0,
            cursor: false,
            extended_desktop_size: false,
            jpeg_quality: None,
            compression_level: None,
        }
    }
}

impl ClientEncodingState {
    fn push(&mut self, encoding: UpdateEncoding) {
        // Duplicates add no preference information and would otherwise grow
        // the order without bound. Keeping the first occurrence preserves the
        // client's relative order.
        if self.order[..self.count].contains(&encoding) {
            return;
        }
        if self.count < self.order.len() {
            self.order[self.count] = encoding;
            self.count += 1;
        }
    }

    /// Apply one `SetEncodings` entry: a real encoding, a supported
    /// pseudo-encoding, or an ignored extension.
    fn apply(&mut self, encoding: i32) {
        match encoding {
            ENCODING_RAW => self.push(UpdateEncoding::Raw),
            ENCODING_HEXTILE => self.push(UpdateEncoding::Hextile),
            ENCODING_ZLIB => self.push(UpdateEncoding::ZLib),
            ENCODING_TIGHT => self.push(UpdateEncoding::Tight),
            ENCODING_ZRLE => self.push(UpdateEncoding::Zrle),
            ENCODING_CURSOR => self.cursor = true,
            ENCODING_EXTENDED_DESKTOP_SIZE => self.extended_desktop_size = true,
            // -32 (low) .. -23 (high). Later entries win, matching the usual
            // RFB "last hint received" interpretation.
            ENCODING_JPEG_QUALITY_LOW..=ENCODING_JPEG_QUALITY_HIGH => {
                self.jpeg_quality = Some((encoding - ENCODING_JPEG_QUALITY_LOW) as u8);
            }
            // -256 (low) .. -247 (high).
            ENCODING_COMPRESSION_LEVEL_LOW..=ENCODING_COMPRESSION_LEVEL_HIGH => {
                self.compression_level = Some((encoding - ENCODING_COMPRESSION_LEVEL_LOW) as u8);
            }
            _ => {}
        }
    }

    /// The first advertised encoding that has not failed, or Raw.
    fn select(self, disabled: DisabledEncodings) -> UpdateEncoding {
        self.order[..self.count]
            .iter()
            .copied()
            .find(|encoding| !disabled.is_disabled(*encoding))
            .unwrap_or(UpdateEncoding::Raw)
    }
}

impl ClientPixelFormat {
    /// A PIXEL is four bytes for every 32-bit true-colour format this server
    /// accepts (Raw and Hextile both use PIXEL, not ZRLE's CPIXEL).
    fn pixel_len(&self) -> usize {
        4
    }

    /// ZRLE's CPIXEL is three bytes when 32-bit true-colour pixels have all
    /// colour bits in either the least- or most-significant three bytes and
    /// depth is at most 24 (RFC 6143 section 7.7.5). Otherwise it is a full
    /// four-byte PIXEL.
    fn cpixel_len(&self) -> usize {
        if self.depth <= 24 {
            3
        } else {
            4
        }
    }

    fn pixel_value(&self, red: u8, green: u8, blue: u8) -> u32 {
        (u32::from(red) << self.red_shift)
            | (u32::from(green) << self.green_shift)
            | (u32::from(blue) << self.blue_shift)
    }

    fn push_pixel(&self, value: u32, out: &mut Vec<u8>) {
        if self.big_endian {
            out.extend_from_slice(&value.to_be_bytes());
        } else {
            out.extend_from_slice(&value.to_le_bytes());
        }
    }

    /// Append a ZRLE CPIXEL. The supported formats keep all colour bits in
    /// the low three bytes, so a 3-byte CPIXEL is the pixel's wire bytes with
    /// the zero byte dropped (at the end for little-endian, at the start for
    /// big-endian).
    fn push_cpixel(&self, value: u32, out: &mut Vec<u8>) {
        if self.cpixel_len() == 4 {
            self.push_pixel(value, out);
            return;
        }
        let bytes = if self.big_endian {
            value.to_be_bytes()
        } else {
            value.to_le_bytes()
        };
        let start = usize::from(self.big_endian);
        out.extend_from_slice(&bytes[start..start + 3]);
    }

    /// Tight's TPIXEL is a full PIXEL unless the client chose 32-bit
    /// true-colour with depth 24 and 8-bit channels; then it is three bytes in
    /// R, G, B order, independent of shift order and endianness.
    fn tight_pixel_len(&self) -> usize {
        if self.depth == 24 {
            3
        } else {
            4
        }
    }

    fn push_tight_pixel(&self, value: u32, out: &mut Vec<u8>) {
        if self.tight_pixel_len() == 3 {
            out.push(((value >> self.red_shift) & 0xff) as u8);
            out.push(((value >> self.green_shift) & 0xff) as u8);
            out.push(((value >> self.blue_shift) & 0xff) as u8);
        } else {
            self.push_pixel(value, out);
        }
    }

    /// Extract the red, green, and blue components from a packed pixel value.
    fn rgb_components(&self, value: u32) -> [u8; 3] {
        [
            ((value >> self.red_shift) & 0xff) as u8,
            ((value >> self.green_shift) & 0xff) as u8,
            ((value >> self.blue_shift) & 0xff) as u8,
        ]
    }
}

impl fmt::Display for ClientPixelFormat {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "32bpp depth-{} true-colour {} r{} g{} b{}",
            self.depth,
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
    Update {
        incremental: bool,
        rects: Vec<Rect>,
    },
    /// A parsed `SetDesktopSize` request plus its typed result. The reader never
    /// applies geometry itself; the writer answers the client so all wire
    /// traffic stays on the single write task.
    SetDesktopSize(DesktopSizeRequest),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct DesktopSizeRequest {
    width: u16,
    height: u16,
    result: DesktopSizeResult,
}

#[derive(Debug)]
struct PendingUpdate {
    incremental: bool,
    rects: Vec<Rect>,
}

impl PendingUpdate {
    fn merge(&mut self, incremental: bool, rects: Vec<Rect>) {
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
    clipboard: Option<Arc<dyn ClipboardAccess>>,
    clipboard_max_bytes: usize,
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
        width: AtomicU32::new(geometry.width),
        height: AtomicU32::new(geometry.height),
        sink,
        stats,
        input_active,
        clipboard,
        clipboard_max_bytes,
        pixel: Mutex::new(ClientPixelFormat::default()),
        encoding: Mutex::new(ClientEncodingState::default()),
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
    // frame. Prefer an already-published frame over the geometry captured at
    // start, so a source that resized before this viewer connected gets an
    // accurate ServerInit instead of an immediate resize. If neither exists,
    // wait for the first frame rather than fabricating a size.
    let published = { frames.borrow().clone() };
    let geometry = match published {
        Some(frame) => frame.format,
        None => match known_geometry {
            Some(geometry) => geometry,
            None => wait_for_first_geometry(frames).await?,
        },
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
            CLIENT_MESSAGE_SET_DESKTOP_SIZE => {
                read_set_desktop_size(&mut read, &context, &commands).await
            }
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
            depth,
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

    // Supported encodings keep the client's stated order; unsupported entries
    // do not disturb it. Tight quality/compression pseudo-encodings are
    // remembered per client. Raw is always acceptable in RFB, so an
    // all-unsupported or empty list falls back to Raw.
    let mut state = ClientEncodingState::default();
    for chunk in encodings.chunks_exact(4) {
        let encoding = i32::from_be_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        state.apply(encoding);
    }
    *lock(&context.encoding) = state;
    debug!(
        client = context.id,
        count,
        order = ?&state.order[..state.count],
        cursor = state.cursor,
        extended_desktop_size = state.extended_desktop_size,
        jpeg_quality = state.jpeg_quality,
        compression_level = state.compression_level,
        "VNC client encoding preferences applied"
    );
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
    let rects = clip_rects(&[rect], context.geometry().0, context.geometry().1);
    commands
        .send(ClientCommand::Update { incremental, rects })
        .await
        .map_err(|_| ProtocolError::ClientGone)
}

/// Parse one RFB `SetDesktopSize` (type 251) message.
///
/// The capture source cannot be resized by VNC, so every valid request is
/// denied with `Prohibited`; malformed bounds get a more specific result. The
/// whole message is always consumed, including every screen record, so a
/// denial never desynchronises later client messages. The wire reply (if the
/// client advertised ExtendedDesktopSize) is emitted by the single write task.
async fn read_set_desktop_size(
    read: &mut OwnedReadHalf,
    context: &ClientContext,
    commands: &mpsc::Sender<ClientCommand>,
) -> Result<(), ProtocolError> {
    // 1 padding byte, width, height, screen count, 1 padding byte.
    let mut header = [0u8; 7];
    read.read_exact(&mut header)
        .await
        .map_err(ProtocolError::io)?;
    let width = u16::from_be_bytes([header[1], header[2]]);
    let height = u16::from_be_bytes([header[3], header[4]]);
    let screens = header[5];
    // Consume every advertised screen record even when the count is over the
    // server's bound: RFB framing depends on it and the count is only a u8.
    let mut screen_bytes = vec![0u8; usize::from(screens) * 16];
    read.read_exact(&mut screen_bytes)
        .await
        .map_err(ProtocolError::io)?;

    let result = validate_set_desktop_size(width, height, u16::from(screens));
    context
        .stats
        .desktop_resize_requests
        .fetch_add(1, Ordering::Relaxed);
    context
        .stats
        .desktop_resize_denied
        .fetch_add(1, Ordering::Relaxed);
    debug!(
        client = context.id,
        requested_width = width,
        requested_height = height,
        screens,
        result = ?result,
        "VNC SetDesktopSize request denied; the capture source has no resize API"
    );
    commands
        .send(ClientCommand::SetDesktopSize(DesktopSizeRequest {
            width,
            height,
            result,
        }))
        .await
        .map_err(|_| ProtocolError::ClientGone)
}

/// Classify a `SetDesktopSize` layout without applying it. A valid layout is
/// still `Prohibited` because this server's only geometry source is the
/// capture feed, which VNC cannot resize.
fn validate_set_desktop_size(width: u16, height: u16, screens: u16) -> DesktopSizeResult {
    if width == 0 || height == 0 || screens == 0 || usize::from(screens) > MAX_DESKTOP_SCREENS {
        return DesktopSizeResult::InvalidScreenLayout;
    }
    if u64::from(width) * u64::from(height) > MAX_FRAMEBUFFER_PIXELS {
        return DesktopSizeResult::OutOfResources;
    }
    DesktopSizeResult::Prohibited
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
    let report_x = axis_to_report(x, context.geometry().0);
    let report_y = axis_to_report(y, context.geometry().1);
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

/// Decode the ISO-8859-1 payload mandated by RFB 6143 into a Rust string.
///
/// Every byte maps directly to the same Unicode code point in `U+0000..=U+00FF`;
/// this is deliberately not UTF-8 decoding, so a viewer's Latin-1 `é` (`0xE9`)
/// becomes `é` rather than a replacement character.
fn decode_latin1(bytes: &[u8]) -> String {
    bytes.iter().map(|&byte| char::from(byte)).collect()
}

/// Remove C0/C1 control characters except tab, CR, and LF.
///
/// The result is written to the node's desktop clipboard, so this stops a viewer
/// from injecting NUL or terminal-style control bytes there.
fn sanitize_clipboard_text(text: &str) -> String {
    text.chars()
        .filter(|character| !character.is_control() || matches!(character, '\t' | '\r' | '\n'))
        .collect()
}

/// Encode node clipboard text as ISO-8859-1.
///
/// `None` means at least one character is outside Latin-1. The caller then sends
/// nothing instead of silently replacing text with `?` and claiming a sync.
fn encode_latin1(text: &str) -> Option<Vec<u8>> {
    text.chars()
        .map(|character| u8::try_from(character as u32).ok())
        .collect()
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
    // Consume the body even when the bridge is disabled or the payload is over
    // the configured bound; RFB has no per-message error, so leaving bytes on
    // the wire would desynchronise every later client message.
    let mut bytes = vec![0u8; length as usize];
    read.read_exact(&mut bytes)
        .await
        .map_err(ProtocolError::io)?;

    let Some(clipboard) = context.clipboard.as_ref() else {
        debug!(
            client = context.id,
            length, "ignored VNC ClientCutText (clipboard bridge is disabled)"
        );
        return Ok(());
    };

    if bytes.len() > context.clipboard_max_bytes {
        context
            .stats
            .clipboard_rejected
            .fetch_add(1, Ordering::Relaxed);
        debug!(
            client = context.id,
            length,
            max = context.clipboard_max_bytes,
            "ignored VNC ClientCutText over the clipboard bound"
        );
        return Ok(());
    }

    let text = sanitize_clipboard_text(&decode_latin1(&bytes));
    if !clipboard.available() {
        context
            .stats
            .clipboard_rejected
            .fetch_add(1, Ordering::Relaxed);
        context
            .stats
            .clipboard_available
            .store(false, Ordering::Relaxed);
        debug!(
            client = context.id,
            length, "VNC ClientCutText ignored: clipboard backend is unavailable"
        );
        return Ok(());
    }

    match clipboard.write_text(text).await {
        Ok(written) => {
            context
                .stats
                .clipboard_available
                .store(true, Ordering::Relaxed);
            context
                .stats
                .record_clipboard_sync(ClipboardSyncDirection::ViewerToNode);
            debug!(
                client = context.id,
                bytes = written,
                "wrote VNC ClientCutText to the node clipboard"
            );
        }
        Err(error) => {
            context
                .stats
                .clipboard_rejected
                .fetch_add(1, Ordering::Relaxed);
            if error.unavailable() {
                context
                    .stats
                    .clipboard_available
                    .store(false, Ordering::Relaxed);
            }
            // Log the stable code only; the text is never logged.
            debug!(
                client = context.id,
                code = %error.code,
                "VNC ClientCutText was not applied to the node clipboard"
            );
        }
    }
    Ok(())
}

/// Send the node's current clipboard to a newly connected viewer as
/// `ServerCutText`, when the bridge is enabled, the backend is available, and
/// the text fits the configured bound and ISO-8859-1.
///
/// A clipboard failure is reported in the counters but never fails the viewer;
/// the framebuffer session continues without a clipboard sync.
async fn send_initial_clipboard(
    write: &mut OwnedWriteHalf,
    context: &ClientContext,
    write_timeout: Duration,
) -> Result<(), ProtocolError> {
    let Some(clipboard) = context.clipboard.as_ref() else {
        return Ok(());
    };
    if !clipboard.available() {
        context
            .stats
            .clipboard_available
            .store(false, Ordering::Relaxed);
        debug!(
            client = context.id,
            "VNC clipboard bridge is unavailable; no ServerCutText sent"
        );
        return Ok(());
    }
    let text = match clipboard.read_text().await {
        Ok(text) => text,
        Err(error) => {
            context
                .stats
                .clipboard_rejected
                .fetch_add(1, Ordering::Relaxed);
            if error.unavailable() {
                context
                    .stats
                    .clipboard_available
                    .store(false, Ordering::Relaxed);
            }
            debug!(
                client = context.id,
                code = %error.code,
                "VNC clipboard read failed; no ServerCutText sent"
            );
            return Ok(());
        }
    };
    let Some(encoded) = encode_latin1(&text) else {
        context
            .stats
            .clipboard_rejected
            .fetch_add(1, Ordering::Relaxed);
        debug!(
            client = context.id,
            "VNC clipboard text is outside ISO-8859-1; no ServerCutText sent"
        );
        return Ok(());
    };
    if encoded.len() > context.clipboard_max_bytes {
        context
            .stats
            .clipboard_rejected
            .fetch_add(1, Ordering::Relaxed);
        debug!(
            client = context.id,
            bytes = encoded.len(),
            max = context.clipboard_max_bytes,
            "VNC clipboard text exceeds the bridge bound; no ServerCutText sent"
        );
        return Ok(());
    }

    let mut message = Vec::with_capacity(8 + encoded.len());
    message.extend_from_slice(&[3, 0, 0, 0]);
    message.extend_from_slice(&(encoded.len() as u32).to_be_bytes());
    message.extend_from_slice(&encoded);
    match tokio::time::timeout(write_timeout, write.write_all(&message)).await {
        Ok(Ok(())) => {
            context
                .stats
                .clipboard_available
                .store(true, Ordering::Relaxed);
            context
                .stats
                .record_clipboard_sync(ClipboardSyncDirection::NodeToViewer);
            debug!(
                client = context.id,
                bytes = encoded.len(),
                "sent VNC ServerCutText from the node clipboard"
            );
            Ok(())
        }
        Ok(Err(error)) => Err(ProtocolError::io(error)),
        Err(_) => Err(ProtocolError::WriteTimeout(write_timeout)),
    }
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
    // One zlib stream per client per compressed encoding, continuing across
    // rectangles and updates. ZRLE, ZLib, and Tight are independent streams.
    // If an encode fails its stream state is untrustworthy, so only that
    // encoding is disabled for the rest of the connection and the next
    // preference is used. Tight also keeps its negotiated zlib level and the
    // JPEG sub-path's failure state, which is disabled independently.
    let mut zrle = ZlibEncoder::new(Vec::new(), Compression::default());
    let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
    let mut tight = TightStream::new(TIGHT_DEFAULT_COMPRESSION);
    let mut disabled = DisabledEncodings::default();
    let mut cursor_sent = false;

    // A newly connected viewer gets the node clipboard once, before the first
    // framebuffer update. A viewer's own ClientCutText is never echoed back.
    send_initial_clipboard(&mut write, &context, write_timeout).await?;

    loop {
        if let Some(request) = pending.take() {
            let snapshot = { frames.borrow().clone() };
            let encoding = *lock(&context.encoding);
            // A changed compression-level hint starts a fresh zlib stream; the
            // next Tight rectangle carries the stream-0 reset bit.
            tight.set_level(
                encoding
                    .compression_level
                    .unwrap_or(TIGHT_DEFAULT_COMPRESSION),
            );
            let preferred = encoding.select(disabled);
            let cursor_pending = encoding.cursor && !cursor_sent;
            let (client_width, client_height) = context.geometry();
            // A frame with a new geometry is announced only to a client that
            // advertised ExtendedDesktopSize. Other clients keep their
            // ServerInit geometry and receive only the clipped overlap, so a
            // capture resize can never desynchronise them.
            let resize = snapshot.as_ref().and_then(|frame| {
                let width = frame.format.width;
                let height = frame.format.height;
                if !encoding.extended_desktop_size
                    || width == 0
                    || height == 0
                    || width > u32::from(u16::MAX)
                    || height > u32::from(u16::MAX)
                    || (width == client_width && height == client_height)
                {
                    None
                } else {
                    Some((width as u16, height as u16))
                }
            });
            let should_send = snapshot.as_ref().is_some_and(|frame| {
                resize.is_some()
                    || !request.incremental
                    || frame.generation > last_sent_generation
                    || cursor_pending
            });

            if should_send {
                let frame = snapshot.expect("checked above");
                let pixel = *lock(&context.pixel);
                let newer_frame = resize.is_some()
                    || !request.incremental
                    || frame.generation > last_sent_generation;
                // A resize leaves the rest of the client framebuffer undefined,
                // so that update carries the whole new framebuffer. An
                // incremental request with no new frame may still carry the
                // one-time cursor shape; unchanged pixels are not re-sent.
                let full_rect;
                let rects: &[Rect] = if let Some((width, height)) = resize {
                    full_rect = Rect {
                        x: 0,
                        y: 0,
                        width,
                        height,
                    };
                    std::slice::from_ref(&full_rect)
                } else if newer_frame {
                    &request.rects
                } else {
                    &[]
                };
                let (clip_width, clip_height) = match resize {
                    Some((width, height)) => (u32::from(width), u32::from(height)),
                    None => (client_width, client_height),
                };
                let zrle_stream = if preferred == UpdateEncoding::Zrle {
                    Some(&mut zrle)
                } else {
                    None
                };
                let zlib_stream = if preferred == UpdateEncoding::ZLib {
                    Some(&mut zlib)
                } else {
                    None
                };
                let tight_stream = if preferred == UpdateEncoding::Tight {
                    Some(&mut tight)
                } else {
                    None
                };
                let built = build_update(
                    &frame,
                    pixel,
                    rects,
                    preferred,
                    zrle_stream,
                    zlib_stream,
                    tight_stream,
                    encoding.jpeg_quality,
                    cursor_pending,
                    resize,
                    (clip_width, clip_height),
                );
                if let Some(failed) = built.failed_encoding {
                    disabled.disable(failed);
                    // Drop the poisoned stream and release its buffer, leaving
                    // any other encoder usable.
                    if failed == UpdateEncoding::Zrle {
                        zrle = ZlibEncoder::new(Vec::new(), Compression::default());
                    } else if failed == UpdateEncoding::ZLib {
                        zlib = ZlibEncoder::new(Vec::new(), Compression::default());
                    } else if failed == UpdateEncoding::Tight {
                        tight = TightStream::new(tight.level);
                    }
                }
                match tokio::time::timeout(write_timeout, write.write_all(&built.bytes)).await {
                    Ok(Ok(())) => {
                        if let Some((width, height)) = resize {
                            context.set_geometry(u32::from(width), u32::from(height));
                            context
                                .stats
                                .desktop_resize_sent
                                .fetch_add(1, Ordering::Relaxed);
                            debug!(
                                client = context.id,
                                width, height, "sent VNC ExtendedDesktopSize resize"
                            );
                        }
                        if newer_frame {
                            if frame.generation > last_sent_generation.saturating_add(1) {
                                context.stats.frames_coalesced.fetch_add(
                                    frame.generation - last_sent_generation - 1,
                                    Ordering::Relaxed,
                                );
                            }
                            last_sent_generation = frame.generation;
                        }
                        if built.cursor_sent {
                            cursor_sent = true;
                        }
                        context.stats.updates_sent.fetch_add(1, Ordering::Relaxed);
                        context
                            .stats
                            .bytes_sent
                            .fetch_add(built.payload_bytes as u64, Ordering::Relaxed);
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
                    Some(ClientCommand::Update { incremental, rects }) => {
                        match pending.as_mut() {
                            Some(pending) => pending.merge(incremental, rects),
                            None => pending = Some(PendingUpdate { incremental, rects }),
                        }
                    }
                    Some(ClientCommand::SetDesktopSize(request)) => {
                        let encoding = *lock(&context.encoding);
                        if !encoding.extended_desktop_size {
                            debug!(
                                client = context.id,
                                requested_width = request.width,
                                requested_height = request.height,
                                "VNC SetDesktopSize denied without a wire reply: the client did not advertise ExtendedDesktopSize"
                            );
                            continue;
                        }
                        let (width, height) = context.geometry();
                        let bytes = build_desktop_size_reply(
                            width as u16,
                            height as u16,
                            request.result,
                        );
                        match tokio::time::timeout(write_timeout, write.write_all(&bytes)).await {
                            Ok(Ok(())) => {
                                debug!(
                                    client = context.id,
                                    requested_width = request.width,
                                    requested_height = request.height,
                                    result = ?request.result,
                                    "sent VNC SetDesktopSize denial"
                                );
                            }
                            Ok(Err(error)) => return Err(ProtocolError::io(error)),
                            Err(_) => {
                                context.stats.slow_clients.fetch_add(1, Ordering::Relaxed);
                                return Err(ProtocolError::WriteTimeout(write_timeout));
                            }
                        }
                    }
                    None => return Ok(()),
                }
            }
        }
    }
}

struct BuiltUpdate {
    bytes: Vec<u8>,
    payload_bytes: usize,
    /// The encoding that failed while building this update, if any. Only that
    /// encoding is disabled for the rest of the connection.
    failed_encoding: Option<UpdateEncoding>,
    cursor_sent: bool,
}

fn push_rect_header(out: &mut Vec<u8>, rect: Rect, encoding: i32) {
    out.extend_from_slice(&rect.x.to_be_bytes());
    out.extend_from_slice(&rect.y.to_be_bytes());
    out.extend_from_slice(&rect.width.to_be_bytes());
    out.extend_from_slice(&rect.height.to_be_bytes());
    out.extend_from_slice(&encoding.to_be_bytes());
}

/// Write one ExtendedDesktopSize (-308) rectangle covering a single screen.
///
/// The pseudo-rectangle reuses the normal rectangle header: `x` carries the
/// reason (0 = server resize, 1 = reply to `SetDesktopSize`), `y` carries the
/// result code, and width/height are the new framebuffer size.
fn push_extended_desktop_size_rect(
    out: &mut Vec<u8>,
    width: u16,
    height: u16,
    reason: u16,
    result: DesktopSizeResult,
) {
    out.extend_from_slice(&reason.to_be_bytes());
    out.extend_from_slice(&result.code().to_be_bytes());
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&ENCODING_EXTENDED_DESKTOP_SIZE.to_be_bytes());
    out.push(1); // one screen
    out.extend_from_slice(&0u32.to_be_bytes()); // screen id
    out.extend_from_slice(&0u16.to_be_bytes()); // screen x
    out.extend_from_slice(&0u16.to_be_bytes()); // screen y
    out.extend_from_slice(&width.to_be_bytes());
    out.extend_from_slice(&height.to_be_bytes());
    out.extend_from_slice(&0u32.to_be_bytes()); // flags
}

/// A FramebufferUpdate containing only a `SetDesktopSize` denial. The client's
/// current geometry is echoed so it does not guess a new size.
fn build_desktop_size_reply(width: u16, height: u16, result: DesktopSizeResult) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(4 + 12 + 17);
    bytes.push(0); // FramebufferUpdate
    bytes.push(0); // padding
    bytes.extend_from_slice(&1u16.to_be_bytes());
    push_extended_desktop_size_rect(
        &mut bytes,
        width,
        height,
        DESKTOP_SIZE_REASON_CLIENT,
        result,
    );
    bytes
}

#[allow(clippy::too_many_arguments)]
fn build_update(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    rects: &[Rect],
    preferred: UpdateEncoding,
    mut zrle: Option<&mut ZlibEncoder<Vec<u8>>>,
    mut zlib: Option<&mut ZlibEncoder<Vec<u8>>>,
    mut tight: Option<&mut TightStream>,
    jpeg_quality: Option<u8>,
    send_cursor: bool,
    desktop_resize: Option<(u16, u16)>,
    client_geometry: (u32, u32),
) -> BuiltUpdate {
    // A client that did not negotiate resize keeps its ServerInit geometry; a
    // resized frame is clipped to the overlap instead of exceeding the size the
    // client believes the framebuffer has.
    let limit_width = frame.format.width.min(client_geometry.0);
    let limit_height = frame.format.height.min(client_geometry.1);
    let non_empty = clip_rects(rects, limit_width, limit_height);
    let mut body = Vec::new();
    let mut payload_bytes = 0usize;
    let mut failed_encoding: Option<UpdateEncoding> = None;
    let mut hextile = Vec::new();
    // Tight may split one requested rectangle into several wire rectangles so
    // no rectangle exceeds the width/area bounds. Count what is actually sent.
    let mut wire_rects = 0usize;

    for rect in &non_empty {
        // Until an encoder fails, use the client's first usable preference.
        // A failure falls back to Raw for this rectangle and every remaining
        // rectangle in the update; the caller disables that one encoding.
        if failed_encoding.is_none() {
            match preferred {
                UpdateEncoding::Raw => {}
                UpdateEncoding::Zrle => {
                    if let Some(stream) = zrle.as_deref_mut() {
                        match encode_zrle_rect(frame, pixel, *rect, stream) {
                            Ok(data) => {
                                push_rect_header(&mut body, *rect, ENCODING_ZRLE);
                                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                                body.extend_from_slice(&data);
                                payload_bytes += data.len();
                                wire_rects += 1;
                                continue;
                            }
                            Err(error) => {
                                debug!(
                                    error = %error,
                                    "ZRLE rectangle encode failed; falling back to Raw"
                                );
                                failed_encoding = Some(UpdateEncoding::Zrle);
                            }
                        }
                    }
                }
                UpdateEncoding::Hextile => {
                    hextile.clear();
                    match encode_hextile_rect(frame, pixel, *rect, &mut hextile) {
                        Ok(()) => {
                            push_rect_header(&mut body, *rect, ENCODING_HEXTILE);
                            payload_bytes += hextile.len();
                            body.extend_from_slice(&hextile);
                            wire_rects += 1;
                            continue;
                        }
                        Err(error) => {
                            debug!(
                                error = %error,
                                "Hextile rectangle encode failed; falling back to Raw"
                            );
                            failed_encoding = Some(UpdateEncoding::Hextile);
                        }
                    }
                }
                UpdateEncoding::ZLib => {
                    if let Some(stream) = zlib.as_deref_mut() {
                        match encode_zlib_rect(frame, pixel, *rect, stream) {
                            Ok(data) => {
                                push_rect_header(&mut body, *rect, ENCODING_ZLIB);
                                body.extend_from_slice(&(data.len() as u32).to_be_bytes());
                                payload_bytes += data.len();
                                body.extend_from_slice(&data);
                                wire_rects += 1;
                                continue;
                            }
                            Err(error) => {
                                debug!(
                                    error = %error,
                                    "ZLib rectangle encode failed; falling back to Raw"
                                );
                                failed_encoding = Some(UpdateEncoding::ZLib);
                            }
                        }
                    }
                }
                UpdateEncoding::Tight => {
                    if let Some(stream) = tight.as_deref_mut() {
                        let mark = body.len();
                        match encode_tight_rect(
                            frame,
                            pixel,
                            *rect,
                            stream,
                            jpeg_quality,
                            &mut body,
                        ) {
                            Ok(count) => {
                                payload_bytes += body.len() - mark;
                                wire_rects += count;
                                continue;
                            }
                            Err(error) => {
                                debug!(
                                    error = %error,
                                    "Tight rectangle encode failed; falling back to Raw"
                                );
                                failed_encoding = Some(UpdateEncoding::Tight);
                            }
                        }
                    }
                }
            }
        }

        push_rect_header(&mut body, *rect, ENCODING_RAW);
        payload_bytes += usize::from(rect.width) * usize::from(rect.height) * 4;
        encode_raw_rect(frame, pixel, *rect, &mut body);
        wire_rects += 1;
    }

    let rect_count = wire_rects + usize::from(send_cursor) + usize::from(desktop_resize.is_some());
    let mut bytes = Vec::with_capacity(4 + rect_count * 12 + body.len());
    bytes.push(0); // FramebufferUpdate
    bytes.push(0); // padding
    bytes.extend_from_slice(&(rect_count as u16).to_be_bytes());
    if let Some((width, height)) = desktop_resize {
        // ExtendedDesktopSize must precede the pixel rectangles of the same
        // update: the client resizes its framebuffer before painting them.
        push_extended_desktop_size_rect(
            &mut bytes,
            width,
            height,
            DESKTOP_SIZE_REASON_SERVER,
            DesktopSizeResult::Success,
        );
    }
    bytes.extend_from_slice(&body);
    if send_cursor {
        // The node has no cursor-shape source. A 0x0 Cursor rectangle is the
        // typed empty cursor: clients hide the local pointer instead of being
        // shown a fabricated shape.
        push_rect_header(
            &mut bytes,
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 0,
            },
            ENCODING_CURSOR,
        );
    }

    BuiltUpdate {
        bytes,
        payload_bytes,
        failed_encoding,
        cursor_sent: send_cursor,
    }
}

fn encode_raw_rect(frame: &FrameSnapshot, pixel: ClientPixelFormat, rect: Rect, out: &mut Vec<u8>) {
    for row in 0..usize::from(rect.height) {
        let y = usize::from(rect.y) + row;
        for column in 0..usize::from(rect.width) {
            let x = usize::from(rect.x) + column;
            // Rectangles are clipped to the validated frame, so a lookup can
            // only fail for an unsupported source format (rejected at start).
            // Emit black rather than a short rectangle if that invariant is
            // ever broken, keeping the byte count the client expects.
            let (red, green, blue) = source_rgb(frame, x, y).unwrap_or((0, 0, 0));
            pixel.push_pixel(pixel.pixel_value(red, green, blue), out);
        }
    }
}

fn source_rgb(frame: &FrameSnapshot, x: usize, y: usize) -> io::Result<(u8, u8, u8)> {
    let bytes_per_pixel = match frame.format.pixel_format {
        PixelFormat::Rgb24 => 3,
        PixelFormat::Bgra => 4,
        other => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("pixel format {other} is not encodable"),
            ))
        }
    };
    let stride = frame.format.width as usize * bytes_per_pixel;
    let index = y
        .checked_mul(stride)
        .and_then(|row| row.checked_add(x.checked_mul(bytes_per_pixel)?))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "pixel offset overflow"))?;
    if index + bytes_per_pixel > frame.data.len() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "pixel is outside the captured frame",
        ));
    }
    Ok(match frame.format.pixel_format {
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
        _ => unreachable!("matched above"),
    })
}

/// Encode one Hextile rectangle (RFC 6143 section 7.7.4).
///
/// The rectangle is split into 16x16 tiles. Each tile is encoded as the
/// smaller of a raw tile and a background/foreground plus maximal same-colour
/// subrects. Background and foreground colours persist across the tiles of one
/// rectangle, but are reset for every rectangle.
fn encode_hextile_rect(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    rect: Rect,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Hextile rectangle is empty",
        ));
    }
    let left = usize::from(rect.x);
    let top = usize::from(rect.y);
    if left + width > frame.format.width as usize || top + height > frame.format.height as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Hextile rectangle exceeds the captured frame",
        ));
    }

    let mut state = HextileState::default();
    let mut scratch = HextileTileScratch::default();
    for tile_y in (0..height).step_by(HEXTILE_TILE_SIZE) {
        let tile_height = HEXTILE_TILE_SIZE.min(height - tile_y);
        for tile_x in (0..width).step_by(HEXTILE_TILE_SIZE) {
            let tile_width = HEXTILE_TILE_SIZE.min(width - tile_x);
            encode_hextile_tile(
                frame,
                pixel,
                left + tile_x,
                top + tile_y,
                tile_width,
                tile_height,
                &mut state,
                &mut scratch,
                out,
            )?;
        }
    }
    Ok(())
}

/// Background and foreground colours persisted across the tiles of one
/// rectangle. RFC 6143 leaves their initial values undefined, so the encoder
/// tracks whether a client could have learned them yet and specifies them
/// explicitly until it has.
#[derive(Default)]
struct HextileState {
    background: u32,
    foreground: u32,
    background_defined: bool,
    foreground_defined: bool,
}

#[derive(Default)]
struct HextileTileScratch {
    values: Vec<u32>,
    counts: HashMap<u32, usize>,
    covered: Vec<bool>,
    subrects: Vec<(u32, u8, u8, u8, u8)>,
    bytes: Vec<u8>,
}

/// Encode one 16x16-or-smaller Hextile tile. The background is the most
/// frequent colour (keeping the previous background on a tie); the remaining
/// pixels become maximal same-colour subrects. Subrects share the foreground
/// colour when possible, otherwise every subrect carries its own PIXEL.
#[allow(clippy::too_many_arguments)]
fn encode_hextile_tile(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    state: &mut HextileState,
    scratch: &mut HextileTileScratch,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    let HextileTileScratch {
        values,
        counts,
        covered,
        subrects,
        bytes,
    } = scratch;
    values.clear();
    counts.clear();
    covered.clear();
    subrects.clear();
    bytes.clear();

    let count = width * height;
    for row in 0..height {
        for column in 0..width {
            let (red, green, blue) = source_rgb(frame, left + column, top + row)?;
            values.push(pixel.pixel_value(red, green, blue));
        }
    }
    for &value in values.iter() {
        *counts.entry(value).or_insert(0) += 1;
    }
    let max_count = counts.values().copied().max().unwrap_or(0);
    let background = if state.background_defined
        && counts.get(&state.background).copied().unwrap_or(0) == max_count
    {
        state.background
    } else {
        values
            .iter()
            .copied()
            .find(|value| counts.get(value).copied() == Some(max_count))
            .expect("a tile always has at least one pixel")
    };

    // A solid tile is either the current background (no payload at all) or a
    // new background PIXEL.
    if max_count == count {
        if !state.background_defined || background != state.background {
            out.push(HEXTILE_BACKGROUND_SPECIFIED);
            pixel.push_pixel(background, out);
            state.background = background;
            state.background_defined = true;
        } else {
            out.push(0);
        }
        return Ok(());
    }

    covered.resize(count, false);
    for y in 0..height {
        for x in 0..width {
            let index = y * width + x;
            if covered[index] || values[index] == background {
                continue;
            }
            let value = values[index];
            // Maximal horizontal run of this colour, then extend that width
            // downward while every cell still matches and is uncovered.
            let mut rect_width = 1;
            while x + rect_width < width {
                let next = index + rect_width;
                if covered[next] || values[next] != value {
                    break;
                }
                rect_width += 1;
            }
            let mut rect_height = 1;
            while y + rect_height < height {
                let row = (y + rect_height) * width;
                let matches = (0..rect_width).all(|dx| {
                    let next = row + x + dx;
                    !covered[next] && values[next] == value
                });
                if !matches {
                    break;
                }
                rect_height += 1;
            }
            for dy in 0..rect_height {
                for dx in 0..rect_width {
                    covered[(y + dy) * width + x + dx] = true;
                }
            }
            subrects.push((value, x as u8, y as u8, rect_width as u8, rect_height as u8));
        }
    }

    // The background is the most frequent colour, so at most 255 pixels are
    // non-background and the subrect count always fits one byte. Keep the
    // guard so a future change to the background heuristic cannot truncate it.
    if subrects.len() > usize::from(u8::MAX) {
        emit_hextile_raw(values, pixel, out);
        return Ok(());
    }

    let first = subrects
        .first()
        .map(|subrect| subrect.0)
        .expect("a non-solid tile has a subrect");
    let coloured = subrects.iter().any(|subrect| subrect.0 != first);
    let background_changed = !state.background_defined || background != state.background;
    let foreground_changed = !coloured && (!state.foreground_defined || first != state.foreground);

    let mut flags = HEXTILE_ANY_SUBRECTS;
    if background_changed {
        flags |= HEXTILE_BACKGROUND_SPECIFIED;
    }
    if foreground_changed {
        flags |= HEXTILE_FOREGROUND_SPECIFIED;
    }
    if coloured {
        flags |= HEXTILE_SUBRECTS_COLOURED;
    }

    bytes.push(flags);
    if background_changed {
        pixel.push_pixel(background, bytes);
    }
    if foreground_changed {
        pixel.push_pixel(first, bytes);
    }
    bytes.push(subrects.len() as u8);
    for &(value, x, y, subrect_width, subrect_height) in subrects.iter() {
        if coloured {
            pixel.push_pixel(value, bytes);
        }
        bytes.push((x << 4) | y);
        bytes.push(((subrect_width - 1) << 4) | (subrect_height - 1));
    }

    if bytes.len() >= 1 + count * pixel.pixel_len() {
        emit_hextile_raw(values, pixel, out);
        return Ok(());
    }

    out.extend_from_slice(bytes);
    state.background = background;
    state.background_defined = true;
    if !coloured {
        state.foreground = first;
        state.foreground_defined = true;
    }
    Ok(())
}

fn emit_hextile_raw(values: &[u32], pixel: ClientPixelFormat, out: &mut Vec<u8>) {
    out.push(HEXTILE_RAW);
    for &value in values {
        pixel.push_pixel(value, out);
    }
}

/// Encode one ZLib rectangle (RFC 6143 section 7.7.7).
///
/// The rectangle payload is exactly the Raw encoding's pixels, compressed
/// through the client's persistent ZLib zlib stream and sync-flushed after each
/// rectangle. ZRLE has a separate stream, so switching between the two never
/// corrupts either one. On any error the caller must disable ZLib for the
/// connection because the stream state may be partially written.
fn encode_zlib_rect(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    rect: Rect,
    encoder: &mut ZlibEncoder<Vec<u8>>,
) -> io::Result<Vec<u8>> {
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZLib rectangle is empty",
        ));
    }
    let left = usize::from(rect.x);
    let top = usize::from(rect.y);
    if left + width > frame.format.width as usize || top + height > frame.format.height as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZLib rectangle exceeds the captured frame",
        ));
    }

    let mut row = Vec::with_capacity(width * pixel.pixel_len());
    for y in 0..height {
        row.clear();
        for x in 0..width {
            let (red, green, blue) = source_rgb(frame, left + x, top + y)?;
            pixel.push_pixel(pixel.pixel_value(red, green, blue), &mut row);
        }
        encoder.write_all(&row)?;
        if encoder.get_ref().len() > MAX_ZLIB_RECT_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "ZLib rectangle exceeded the compressed-size bound",
            ));
        }
    }
    encoder.flush()?;
    let data = std::mem::take(encoder.get_mut());
    if data.len() > MAX_ZLIB_RECT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ZLib rectangle exceeded the compressed-size bound",
        ));
    }
    Ok(data)
}

/// Per-client Tight encoder state.
///
/// The zlib stream continues across rectangles and updates. A changed
/// compression-level hint starts a new zlib stream and marks a pending
/// stream-0 reset, which the next Tight rectangle advertises in its
/// compression-control byte. JPEG is a separate sub-path: if JPEG encoding
/// fails, only JPEG is disabled and lossless Tight continues on the same
/// connection.
struct TightStream {
    encoder: ZlibEncoder<Vec<u8>>,
    level: u8,
    used: bool,
    reset_pending: bool,
    jpeg_disabled: bool,
}

impl TightStream {
    fn new(level: u8) -> Self {
        Self {
            encoder: ZlibEncoder::new(Vec::new(), Compression::new(u32::from(level))),
            level,
            used: false,
            reset_pending: false,
            jpeg_disabled: false,
        }
    }

    fn set_level(&mut self, level: u8) {
        if self.level == level {
            return;
        }
        // A decoder that has already seen this stream must be told to reset
        // stream 0 before it reads the replacement stream.
        if self.used {
            self.reset_pending = true;
        }
        self.encoder = ZlibEncoder::new(Vec::new(), Compression::new(u32::from(level)));
        self.level = level;
        self.used = false;
    }
}

/// Encode one requested rectangle as Tight encoding (encoding 7).
///
/// A requested rectangle can become several wire rectangles: TightVNC decoders
/// cap a rectangle at 2048 pixels wide, and [`TIGHT_MAX_PIXELS`] keeps each
/// rectangle's uncompressed payload small enough that its compressed length
/// always fits Tight's 22-bit compact length field. Headers and payloads are
/// appended to `out`; on any error `out` is restored to its original length so
/// the caller can emit the whole requested rectangle as Raw instead.
fn encode_tight_rect(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    rect: Rect,
    stream: &mut TightStream,
    jpeg_quality: Option<u8>,
    out: &mut Vec<u8>,
) -> io::Result<usize> {
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Tight rectangle is empty",
        ));
    }
    let left = usize::from(rect.x);
    let top = usize::from(rect.y);
    if left + width > frame.format.width as usize || top + height > frame.format.height as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Tight rectangle exceeds the captured frame",
        ));
    }

    let start = out.len();
    let mut scratch = TightScratch::default();
    let mut payload = Vec::new();
    let mut count = 0usize;
    let reset = stream.reset_pending;
    for (index, sub_rect) in tight_sub_rects(rect).into_iter().enumerate() {
        let reset_bit = if index == 0 && reset { 0x01 } else { 0 };
        if let Err(error) = encode_tight_sub_rect(
            frame,
            pixel,
            sub_rect,
            stream,
            jpeg_quality,
            reset_bit,
            &mut scratch,
            &mut payload,
        ) {
            out.truncate(start);
            return Err(error);
        }
        push_rect_header(out, sub_rect, ENCODING_TIGHT);
        out.extend_from_slice(&payload);
        count += 1;
    }
    // The reset bit and the new zlib stream are now committed.
    stream.reset_pending = false;
    stream.used = true;
    Ok(count)
}

/// Split one requested rectangle into Tight wire rectangles. TightVNC decoders
/// cap rectangle width at [`TIGHT_MAX_WIDTH`]; the area cap bounds each
/// rectangle's compressed payload, including incompressible copy-filter data.
fn tight_sub_rects(rect: Rect) -> Vec<Rect> {
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    let mut rects = Vec::new();
    if width == 0 || height == 0 {
        return rects;
    }
    let columns = (width + TIGHT_MAX_WIDTH - 1) / TIGHT_MAX_WIDTH;
    for column in 0..columns {
        let x_offset = column * TIGHT_MAX_WIDTH;
        let sub_width = TIGHT_MAX_WIDTH.min(width - x_offset);
        let rows_per_rect = (TIGHT_MAX_PIXELS / sub_width).max(1);
        for y_offset in (0..height).step_by(rows_per_rect) {
            let sub_height = rows_per_rect.min(height - y_offset);
            rects.push(Rect {
                x: rect.x + x_offset as u16,
                y: rect.y + y_offset as u16,
                width: sub_width as u16,
                height: sub_height as u16,
            });
        }
    }
    rects
}

/// Reusable per-rectangle buffers for Tight encoding. The rectangle area is
/// bounded by [`TIGHT_MAX_PIXELS`], so these never grow without bound.
#[derive(Default)]
struct TightScratch {
    values: Vec<u32>,
    palette: Vec<u32>,
    lookup: HashMap<u32, u8>,
    filtered: Vec<u8>,
    gradient: Vec<u8>,
    gradient_prev: Vec<[u8; 3]>,
    gradient_row: Vec<[u8; 3]>,
    jpeg_rgb: Vec<u8>,
    jpeg: Vec<u8>,
}

/// Encode one Tight wire rectangle (control byte plus optional filter data).
/// The caller has already bounded width and area. `reset_bit` is 1 only for the
/// first rectangle after a zlib stream restart.
#[allow(clippy::too_many_arguments)]
fn encode_tight_sub_rect(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    rect: Rect,
    stream: &mut TightStream,
    jpeg_quality: Option<u8>,
    reset_bit: u8,
    scratch: &mut TightScratch,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    out.clear();
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    let area = width * height;
    if width == 0 || height == 0 || width > TIGHT_MAX_WIDTH || area > TIGHT_MAX_PIXELS {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "Tight rectangle exceeds the encoder bounds",
        ));
    }
    let left = usize::from(rect.x);
    let top = usize::from(rect.y);

    let TightScratch {
        values,
        palette,
        lookup,
        filtered,
        gradient,
        gradient_prev,
        gradient_row,
        jpeg_rgb,
        jpeg,
    } = scratch;
    values.clear();
    values.reserve(area);
    palette.clear();
    lookup.clear();
    filtered.clear();
    gradient.clear();
    jpeg_rgb.clear();
    jpeg.clear();

    for row in 0..height {
        for column in 0..width {
            let (red, green, blue) = source_rgb(frame, left + column, top + row)?;
            values.push(pixel.pixel_value(red, green, blue));
        }
    }

    // FillCompression: a single TPIXEL applies to the whole rectangle.
    let first = values[0];
    if values.iter().all(|value| *value == first) {
        out.push(TIGHT_FILL | reset_bit);
        pixel.push_tight_pixel(first, out);
        return Ok(());
    }

    // Collect at most 256 colours in first-seen order. More than 256 disables
    // the palette filter and marks the rectangle as high-colour, where the
    // gradient filter and JPEG are worth considering.
    let mut too_many_colours = false;
    for &value in values.iter() {
        if lookup.contains_key(&value) {
            continue;
        }
        if palette.len() == TIGHT_MAX_PALETTE {
            too_many_colours = true;
            break;
        }
        lookup.insert(value, palette.len() as u8);
        palette.push(value);
    }

    let tpixel = pixel.tight_pixel_len();
    let copy_size = area * tpixel;
    let palette_size = (!too_many_colours).then(|| {
        let index_size = if palette.len() <= 2 {
            ((width + 7) / 8) * height
        } else {
            area
        };
        index_size + palette.len() * tpixel
    });
    let use_palette = palette_size.is_some_and(|size| size < copy_size);

    if use_palette {
        if palette.len() <= 2 {
            // Two colours use one bit per pixel, MSB first, row aligned.
            filtered.reserve(((width + 7) / 8) * height);
            for row in 0..height {
                let mut byte = 0u8;
                let mut used = 0usize;
                for column in 0..width {
                    let index = lookup[&values[row * width + column]];
                    byte = (byte << 1) | (index & 1);
                    used += 1;
                    if used == 8 {
                        filtered.push(byte);
                        byte = 0;
                        used = 0;
                    }
                }
                if used != 0 {
                    filtered.push(byte << (8 - used));
                }
            }
        } else {
            // Three or more colours use one byte per pixel.
            filtered.reserve(area);
            for &value in values.iter() {
                filtered.push(lookup[&value]);
            }
        }
        out.push(TIGHT_EXPLICIT_FILTER | reset_bit);
        out.push(TIGHT_FILTER_PALETTE);
        out.push((palette.len() - 1) as u8);
        for &value in palette.iter() {
            pixel.push_tight_pixel(value, out);
        }
        write_tight_filtered(stream, filtered, out)?;
        return Ok(());
    }

    // BasicCompression path. Build the copy-filter representation first, then
    // compare it against the gradient predictor and (when the client advertised
    // a JPEG quality level) JPEG. Ties prefer lossless data.
    filtered.reserve(copy_size);
    for &value in values.iter() {
        pixel.push_tight_pixel(value, filtered);
    }
    let mut filter_id = 0u8; // CopyFilter

    if too_many_colours {
        encode_gradient_filter(
            pixel,
            values,
            width,
            height,
            gradient_prev,
            gradient_row,
            gradient,
        );
        let copy_estimate = tight_zlib_estimate(filtered, stream.level)?;
        let gradient_estimate = tight_zlib_estimate(gradient, stream.level)?;
        if gradient_estimate.saturating_mul(100)
            < copy_estimate.saturating_mul(TIGHT_GRADIENT_WIN_PERCENT)
        {
            std::mem::swap(filtered, gradient);
            filter_id = TIGHT_FILTER_GRADIENT;
        }
    }

    if too_many_colours {
        if let Some(level) = jpeg_quality {
            if !stream.jpeg_disabled {
                match encode_tight_jpeg(pixel, values, width, height, level, jpeg_rgb, jpeg) {
                    Ok(()) if jpeg.len() <= TIGHT_MAX_RECT_BYTES => {
                        let jpeg_estimate = jpeg.len() + tight_compact_len_bytes(jpeg.len());
                        let lossless_estimate = tight_zlib_estimate(filtered, stream.level)?;
                        if jpeg_estimate < lossless_estimate {
                            out.push(TIGHT_JPEG | reset_bit);
                            push_tight_length(jpeg.len(), out)?;
                            out.extend_from_slice(jpeg);
                            return Ok(());
                        }
                    }
                    Ok(()) => {
                        debug!(
                            bytes = jpeg.len(),
                            "Tight JPEG payload exceeds the compact-length bound; using lossless Tight"
                        );
                        stream.jpeg_disabled = true;
                    }
                    Err(error) => {
                        debug!(
                            error = %error,
                            "Tight JPEG encode failed; disabling only the JPEG sub-path"
                        );
                        stream.jpeg_disabled = true;
                    }
                }
            }
        }
    }

    if filter_id == 0 {
        // BasicCompression, zlib stream 0, CopyFilter (no filter-id byte).
        out.push(reset_bit);
    } else {
        out.push(TIGHT_EXPLICIT_FILTER | reset_bit);
        out.push(filter_id);
    }
    write_tight_filtered(stream, filtered, out)
}

/// Append filtered Tight pixels, compressing them through the per-client zlib
/// stream unless the payload is below [`TIGHT_MIN_TO_COMPRESS`].
fn write_tight_filtered(
    stream: &mut TightStream,
    filtered: &[u8],
    out: &mut Vec<u8>,
) -> io::Result<()> {
    // Tiny filtered payloads are sent verbatim; TightVNC decoders switch on
    // the uncompressed size and expect no compact length in that case.
    if filtered.len() < TIGHT_MIN_TO_COMPRESS {
        out.extend_from_slice(filtered);
        return Ok(());
    }

    stream.encoder.write_all(filtered)?;
    stream.encoder.flush()?;
    let compressed = std::mem::take(stream.encoder.get_mut());
    if compressed.len() > TIGHT_MAX_RECT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Tight rectangle exceeded the compact-length bound",
        ));
    }
    push_tight_length(compressed.len(), out)?;
    out.extend_from_slice(&compressed);
    Ok(())
}

fn tight_compact_len_bytes(length: usize) -> usize {
    if length < 0x80 {
        1
    } else if length < 0x4000 {
        2
    } else {
        3
    }
}

/// Estimate the wire size of a lossless Tight payload at `level`, including the
/// compact-length prefix. Used only to choose between the copy/gradient/JPEG
/// representations; the chosen representation is then compressed once into the
/// per-client stream.
fn tight_zlib_estimate(data: &[u8], level: u8) -> io::Result<usize> {
    if data.len() < TIGHT_MIN_TO_COMPRESS {
        return Ok(data.len());
    }
    let mut encoder = ZlibEncoder::new(Vec::new(), Compression::new(u32::from(level)));
    encoder.write_all(data)?;
    encoder.flush()?;
    let length = encoder.get_ref().len();
    Ok(length + tight_compact_len_bytes(length))
}

/// Encode the Tight gradient predictor over RGB components.
///
/// Prediction is `V[i-1,j] + V[i,j-1] - V[i-1,j-1]` clamped to 0..=255, with
/// pixels outside the rectangle treated as zero. The emitted byte stream is
/// the per-component difference in TPIXEL layout, which the decoder adds back
/// to the same predictor.
fn encode_gradient_filter(
    pixel: ClientPixelFormat,
    values: &[u32],
    width: usize,
    height: usize,
    previous: &mut Vec<[u8; 3]>,
    row: &mut Vec<[u8; 3]>,
    out: &mut Vec<u8>,
) {
    previous.clear();
    previous.resize(width, [0, 0, 0]);
    row.clear();
    row.resize(width, [0, 0, 0]);
    out.reserve(width * height * pixel.tight_pixel_len());

    for y in 0..height {
        for x in 0..width {
            let actual = pixel.rgb_components(values[y * width + x]);
            let predicted = if x == 0 {
                previous[0]
            } else {
                [
                    gradient_predict(previous[x][0], row[x - 1][0], previous[x - 1][0]),
                    gradient_predict(previous[x][1], row[x - 1][1], previous[x - 1][1]),
                    gradient_predict(previous[x][2], row[x - 1][2], previous[x - 1][2]),
                ]
            };
            let delta = [
                actual[0].wrapping_sub(predicted[0]),
                actual[1].wrapping_sub(predicted[1]),
                actual[2].wrapping_sub(predicted[2]),
            ];
            if pixel.tight_pixel_len() == 3 {
                out.extend_from_slice(&delta);
            } else {
                pixel.push_pixel(pixel.pixel_value(delta[0], delta[1], delta[2]), out);
            }
            row[x] = actual;
        }
        std::mem::swap(previous, row);
    }
}

fn gradient_predict(upper: u8, left: u8, upper_left: u8) -> u8 {
    (i32::from(upper) + i32::from(left) - i32::from(upper_left)).clamp(0, 255) as u8
}

/// Encode an RGB buffer as a JFIF JPEG for Tight's JpegCompression. Quality
/// levels 0..=9 map to a conservative 20..=92 percent range; level 0 is still a
/// valid lossy JPEG because advertising the pseudo-encoding asks for JPEG use.
fn encode_tight_jpeg(
    pixel: ClientPixelFormat,
    values: &[u32],
    width: usize,
    height: usize,
    level: u8,
    rgb: &mut Vec<u8>,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    rgb.clear();
    rgb.reserve(width * height * 3);
    // JPEG always uses red, green, blue regardless of the client's wire order.
    for &value in values {
        rgb.extend_from_slice(&pixel.rgb_components(value));
    }
    let quality = 20u8.saturating_add(level.min(9).saturating_mul(8));
    out.clear();
    JpegEncoder::new(&mut *out, quality)
        .encode(
            rgb,
            u16::try_from(width).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "Tight JPEG width is too large")
            })?,
            u16::try_from(height).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "Tight JPEG height is too large",
                )
            })?,
            JpegColorType::Rgb,
        )
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error.to_string()))
}

/// Append a Tight compact length: one byte for 0..=127, two for 128..=16383,
/// and three for 16384..=4194303 (22 bits).
fn push_tight_length(length: usize, out: &mut Vec<u8>) -> io::Result<()> {
    if length == 0 || length > TIGHT_MAX_RECT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "Tight payload length is not encodable",
        ));
    }
    if length < 0x80 {
        out.push(length as u8);
    } else if length < 0x4000 {
        out.push(0x80 | (length & 0x7f) as u8);
        out.push((length >> 7) as u8);
    } else {
        out.push(0x80 | (length & 0x7f) as u8);
        out.push(0x80 | ((length >> 7) & 0x7f) as u8);
        out.push((length >> 14) as u8);
    }
    Ok(())
}

/// Encode one ZRLE rectangle into the client's persistent zlib stream.
///
/// The returned bytes are exactly this rectangle's compressed segment; the
/// stream itself is flushed to a byte boundary so the client can decode
/// rectangles incrementally. On any error the caller must disable ZRLE for
/// the connection because the stream state may be partially written.
fn encode_zrle_rect(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    rect: Rect,
    encoder: &mut ZlibEncoder<Vec<u8>>,
) -> io::Result<Vec<u8>> {
    let width = usize::from(rect.width);
    let height = usize::from(rect.height);
    if width == 0 || height == 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZRLE rectangle is empty",
        ));
    }
    let left = usize::from(rect.x);
    let top = usize::from(rect.y);
    if left + width > frame.format.width as usize || top + height > frame.format.height as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "ZRLE rectangle exceeds the captured frame",
        ));
    }

    let mut tile = Vec::with_capacity(1 + ZRLE_TILE_SIZE * ZRLE_TILE_SIZE * 4 + 2048);
    let mut scratch = ZrleTileScratch::default();
    for tile_y in (0..height).step_by(ZRLE_TILE_SIZE) {
        let tile_height = ZRLE_TILE_SIZE.min(height - tile_y);
        for tile_x in (0..width).step_by(ZRLE_TILE_SIZE) {
            let tile_width = ZRLE_TILE_SIZE.min(width - tile_x);
            encode_zrle_tile(
                frame,
                pixel,
                left + tile_x,
                top + tile_y,
                tile_width,
                tile_height,
                &mut scratch,
                &mut tile,
            )?;
            encoder.write_all(&tile)?;
            if encoder.get_ref().len() > MAX_ZRLE_RECT_BYTES {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "ZRLE rectangle exceeded the compressed-size bound",
                ));
            }
        }
    }
    encoder.flush()?;
    let data = std::mem::take(encoder.get_mut());
    if data.len() > MAX_ZRLE_RECT_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "ZRLE rectangle exceeded the compressed-size bound",
        ));
    }
    Ok(data)
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ZrleTileKind {
    Raw,
    Solid,
    Palette,
    PaletteRle,
    PlainRle,
}

/// Reusable per-rectangle buffers so a 4K frame does not allocate a new
/// palette, index list, and run list for every 64x64 tile.
#[derive(Default)]
struct ZrleTileScratch {
    values: Vec<u32>,
    palette: Vec<u32>,
    lookup: HashMap<u32, u8>,
    indices: Vec<u8>,
    runs: Vec<(u32, usize)>,
}

/// Encode one ZRLE tile (up to 64x64) and choose the smallest of the Raw,
/// solid, packed-palette, plain-RLE, and palette-RLE sub-encodings.
#[allow(clippy::too_many_arguments)]
fn encode_zrle_tile(
    frame: &FrameSnapshot,
    pixel: ClientPixelFormat,
    left: usize,
    top: usize,
    width: usize,
    height: usize,
    scratch: &mut ZrleTileScratch,
    out: &mut Vec<u8>,
) -> io::Result<()> {
    out.clear();
    let count = width * height;
    let ZrleTileScratch {
        values,
        palette,
        lookup,
        indices,
        runs,
    } = scratch;
    values.clear();
    palette.clear();
    lookup.clear();
    indices.clear();
    runs.clear();
    for row in 0..height {
        for column in 0..width {
            let (red, green, blue) = source_rgb(frame, left + column, top + row)?;
            values.push(pixel.pixel_value(red, green, blue));
        }
    }

    // Collect colours in first-seen order. More than 127 distinct colours
    // disables both palette candidates; plain RLE and Raw remain available.
    let mut too_many_colours = false;
    for &value in values.iter() {
        if let Some(index) = lookup.get(&value) {
            indices.push(*index);
        } else if lookup.len() < ZRLE_MAX_RLE_PALETTE {
            let index = lookup.len() as u8;
            lookup.insert(value, index);
            palette.push(value);
            indices.push(index);
        } else {
            too_many_colours = true;
            break;
        }
    }

    // Runs may continue across row boundaries within a tile.
    for &value in values.iter() {
        match runs.last_mut() {
            Some((last, length)) if *last == value => *length += 1,
            _ => runs.push((value, 1)),
        }
    }

    let cpixel = pixel.cpixel_len();
    let raw_size = 1 + count * cpixel;
    let mut choice = ZrleTileKind::Raw;
    let mut best = raw_size;

    if palette.len() == 1 {
        let solid_size = 1 + cpixel;
        if solid_size < best {
            best = solid_size;
            choice = ZrleTileKind::Solid;
        }
    }
    if !too_many_colours && (2..=ZRLE_MAX_PACKED_PALETTE).contains(&palette.len()) {
        let palette_size =
            1 + palette.len() * cpixel + packed_palette_row_bytes(width, palette.len()) * height;
        if palette_size < best {
            best = palette_size;
            choice = ZrleTileKind::Palette;
        }
    }
    if !too_many_colours && (2..=ZRLE_MAX_RLE_PALETTE).contains(&palette.len()) {
        let palette_rle_size = 1
            + palette.len() * cpixel
            + runs
                .iter()
                .map(|(_, length)| {
                    if *length == 1 {
                        1
                    } else {
                        1 + rle_length_bytes(*length)
                    }
                })
                .sum::<usize>();
        if palette_rle_size < best {
            best = palette_rle_size;
            choice = ZrleTileKind::PaletteRle;
        }
    }
    let plain_rle_size = 1 + runs
        .iter()
        .map(|(_, length)| cpixel + rle_length_bytes(*length))
        .sum::<usize>();
    if plain_rle_size < best {
        choice = ZrleTileKind::PlainRle;
    }

    match choice {
        ZrleTileKind::Raw => {
            out.push(0);
            for value in values.iter() {
                pixel.push_cpixel(*value, out);
            }
        }
        ZrleTileKind::Solid => {
            out.push(1);
            pixel.push_cpixel(palette[0], out);
        }
        ZrleTileKind::Palette => {
            out.push(palette.len() as u8);
            for value in palette.iter() {
                pixel.push_cpixel(*value, out);
            }
            push_packed_palette(&indices, width, height, palette.len(), out);
        }
        ZrleTileKind::PaletteRle => {
            out.push((128 + palette.len()) as u8);
            for value in palette.iter() {
                pixel.push_cpixel(*value, out);
            }
            for (value, length) in runs.iter() {
                let index = lookup[value];
                if *length == 1 {
                    out.push(index);
                } else {
                    out.push(index | 0x80);
                    push_run_length(*length, out);
                }
            }
        }
        ZrleTileKind::PlainRle => {
            out.push(128);
            for (value, length) in runs.iter() {
                pixel.push_cpixel(*value, out);
                push_run_length(*length, out);
            }
        }
    }
    Ok(())
}

fn packed_palette_row_bytes(width: usize, palette_size: usize) -> usize {
    let bits = match palette_size {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    };
    (width * bits + 7) / 8
}

fn push_packed_palette(
    indices: &[u8],
    width: usize,
    height: usize,
    palette_size: usize,
    out: &mut Vec<u8>,
) {
    let bits = match palette_size {
        0..=2 => 1,
        3..=4 => 2,
        _ => 4,
    };
    let mask = (1u8 << bits) - 1;
    for row in 0..height {
        let mut byte = 0u8;
        let mut used = 0usize;
        for column in 0..width {
            byte = (byte << bits) | (indices[row * width + column] & mask);
            used += bits;
            if used == 8 {
                out.push(byte);
                byte = 0;
                used = 0;
            }
        }
        if used != 0 {
            out.push(byte << (8 - used));
        }
    }
}

/// Number of bytes in a ZRLE run-length field: one final byte plus any
/// 255-valued continuation bytes (`length` itself is never zero).
fn rle_length_bytes(length: usize) -> usize {
    1 + (length.saturating_sub(1)) / 255
}

fn push_run_length(length: usize, out: &mut Vec<u8>) {
    let mut remaining = length.saturating_sub(1);
    while remaining >= 255 {
        out.push(255);
        remaining -= 255;
    }
    out.push(remaining as u8);
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

    /// In-memory clipboard backend used by the clipboard bridge tests. It never
    /// touches X11, so tests can assert exact text, call counts, and failures.
    struct FakeClipboard {
        text: Mutex<Option<String>>,
        available: AtomicBool,
        reads: AtomicUsize,
        writes: AtomicUsize,
        fail_read: AtomicBool,
        fail_write: AtomicBool,
        fail_code: &'static str,
    }

    impl FakeClipboard {
        fn new(text: Option<&str>) -> Arc<Self> {
            Arc::new(Self {
                text: Mutex::new(text.map(str::to_owned)),
                available: AtomicBool::new(true),
                reads: AtomicUsize::new(0),
                writes: AtomicUsize::new(0),
                fail_read: AtomicBool::new(false),
                fail_write: AtomicBool::new(false),
                fail_code: "desktop.no_display",
            })
        }

        fn unavailable() -> Arc<Self> {
            let clipboard = Self::new(None);
            clipboard.available.store(false, Ordering::Relaxed);
            clipboard
        }

        fn text(&self) -> Option<String> {
            self.text
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }

        fn writes(&self) -> usize {
            self.writes.load(Ordering::Relaxed)
        }
    }

    #[async_trait]
    impl ClipboardAccess for FakeClipboard {
        fn available(&self) -> bool {
            self.available.load(Ordering::Relaxed)
        }

        async fn read_text(&self) -> Result<String, ClipboardAccessError> {
            self.reads.fetch_add(1, Ordering::Relaxed);
            if self.fail_read.load(Ordering::Relaxed) {
                return Err(ClipboardAccessError {
                    code: self.fail_code.to_owned(),
                    message: "fake read failure".to_owned(),
                });
            }
            Ok(self.text().unwrap_or_default())
        }

        async fn write_text(&self, text: String) -> Result<usize, ClipboardAccessError> {
            self.writes.fetch_add(1, Ordering::Relaxed);
            if self.fail_write.load(Ordering::Relaxed) {
                return Err(ClipboardAccessError {
                    code: self.fail_code.to_owned(),
                    message: "fake write failure".to_owned(),
                });
            }
            if !self.available() {
                return Err(ClipboardAccessError {
                    code: "desktop.no_display".to_owned(),
                    message: "fake clipboard unavailable".to_owned(),
                });
            }
            let bytes = text.len();
            *self
                .text
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(text);
            Ok(bytes)
        }
    }

    /// A fixed RGB24 frame with tile shapes chosen to exercise every ZRLE
    /// sub-encoding: a packed palette, palette RLE, a solid tile, and plain
    /// RLE (128 distinct colours, which disables both palette forms).
    struct PatternSource {
        format: FrameFormat,
        pixels: Arc<Vec<(u8, u8, u8)>>,
        sent: bool,
    }

    impl PatternSource {
        fn new(width: u32, height: u32) -> Self {
            let pixels = Arc::new(pattern_pixels(width as usize, height as usize));
            Self {
                format: FrameFormat::rgb24(width, height),
                pixels,
                sent: false,
            }
        }
    }

    fn pattern_pixels(width: usize, height: usize) -> Vec<(u8, u8, u8)> {
        let colours = [
            (200u8, 10u8, 10u8),
            (10, 200, 10),
            (10, 10, 200),
            (220, 220, 10),
            (40, 40, 40),
        ];
        let mut pixels = Vec::with_capacity(width * height);
        for y in 0..height {
            for x in 0..width {
                let tile = x / 64;
                let local_x = x % 64;
                let colour = match tile {
                    // Four-colour checkerboard: packed palette wins.
                    0 => colours[(local_x + y) % 4],
                    // Four-colour runs of 16: palette RLE wins.
                    1 => colours[(local_x / 16) % 4],
                    // Solid tile.
                    2 => colours[4],
                    // 128 distinct colours in runs of two: plain RLE wins.
                    _ => {
                        let index = (local_x / 2) + (y % 4) * 32;
                        let index = index as u8;
                        (
                            index.wrapping_mul(2),
                            index.wrapping_mul(3),
                            index.wrapping_mul(5),
                        )
                    }
                };
                pixels.push(colour);
            }
        }
        pixels
    }

    #[async_trait]
    impl CaptureSource for PatternSource {
        fn id(&self) -> &str {
            "test-vnc-pattern-source"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            if self.sent {
                std::future::pending::<()>().await;
            }
            tokio::task::yield_now().await;
            self.sent = true;
            let mut data = Vec::with_capacity(self.pixels.len() * 3);
            for (red, green, blue) in self.pixels.iter() {
                data.extend_from_slice(&[*red, *green, *blue]);
            }
            Frame::with_data(self.format, 0, 0, true, data)
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            Ok(())
        }
    }

    /// A fixed RGB24 frame built from a caller-supplied pixel list. It lets a
    /// test control the exact tile shapes an encoder sees.
    struct FixedPixelSource {
        format: FrameFormat,
        pixels: Arc<Vec<(u8, u8, u8)>>,
        sent: bool,
    }

    impl FixedPixelSource {
        fn new(width: u32, height: u32, pixels: Vec<(u8, u8, u8)>) -> Self {
            assert_eq!(pixels.len(), width as usize * height as usize);
            Self {
                format: FrameFormat::rgb24(width, height),
                pixels: Arc::new(pixels),
                sent: false,
            }
        }
    }

    #[async_trait]
    impl CaptureSource for FixedPixelSource {
        fn id(&self) -> &str {
            "test-vnc-fixed-source"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            if self.sent {
                std::future::pending::<()>().await;
            }
            tokio::task::yield_now().await;
            self.sent = true;
            let mut data = Vec::with_capacity(self.pixels.len() * 3);
            for (red, green, blue) in self.pixels.iter() {
                data.extend_from_slice(&[*red, *green, *blue]);
            }
            Frame::with_data(self.format, 0, 0, true, data)
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            Ok(())
        }
    }

    /// A capture source driven directly by the test. `next_frame` blocks until
    /// the test sends a frame, so a resize can happen at an exact point in a
    /// live session instead of racing the pump.
    struct ChannelSource {
        format: FrameFormat,
        frames: mpsc::UnboundedReceiver<Frame>,
    }

    impl ChannelSource {
        fn new(format: FrameFormat, frames: mpsc::UnboundedReceiver<Frame>) -> Self {
            Self { format, frames }
        }
    }

    #[async_trait]
    impl CaptureSource for ChannelSource {
        fn id(&self) -> &str {
            "test-vnc-channel-source"
        }

        fn format(&self) -> FrameFormat {
            self.format
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            match self.frames.recv().await {
                Some(frame) => Ok(frame),
                None => {
                    std::future::pending::<()>().await;
                    unreachable!("pending never resolves")
                }
            }
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            Ok(())
        }
    }

    fn solid_rgb_frame(width: usize, height: usize, colour: (u8, u8, u8)) -> Frame {
        let mut data = vec![0u8; width * height * 3];
        for pixel in data.chunks_exact_mut(3) {
            pixel[0] = colour.0;
            pixel[1] = colour.1;
            pixel[2] = colour.2;
        }
        Frame::with_data(
            FrameFormat::rgb24(width as u32, height as u32),
            0,
            0,
            true,
            data,
        )
        .expect("valid test frame")
    }

    /// Write an RFB `SetDesktopSize` message. Screen records cover the whole
    /// requested framebuffer with zero flags.
    async fn write_set_desktop_size(stream: &mut TcpStream, width: u16, height: u16, screens: u8) {
        let mut message = vec![CLIENT_MESSAGE_SET_DESKTOP_SIZE, 0];
        message.extend_from_slice(&width.to_be_bytes());
        message.extend_from_slice(&height.to_be_bytes());
        message.push(screens);
        message.push(0);
        for id in 0..u32::from(screens) {
            message.extend_from_slice(&id.to_be_bytes());
            message.extend_from_slice(&0u16.to_be_bytes());
            message.extend_from_slice(&0u16.to_be_bytes());
            message.extend_from_slice(&width.to_be_bytes());
            message.extend_from_slice(&height.to_be_bytes());
            message.extend_from_slice(&0u32.to_be_bytes());
        }
        write_all(stream, &message).await;
    }

    fn decode_raw_pixels(
        data: &[u8],
        width: usize,
        height: usize,
        pixel: ClientPixelFormat,
    ) -> Vec<(u8, u8, u8)> {
        let mut position = 0usize;
        let mut pixels = Vec::with_capacity(width * height);
        for _ in 0..width * height {
            pixels.push(decode_rgb(
                decode_pixel(data, &mut position, pixel.big_endian),
                pixel,
            ));
        }
        assert_eq!(position, data.len(), "trailing Raw pixel bytes");
        pixels
    }

    /// Decode one wire rectangle with the matching test decoder. ZRLE, ZLib,
    /// and Tight decoders are persistent because the server keeps one stream
    /// per encoding for the whole connection.
    fn decode_wire_rect(
        rect: &WireRect,
        pixel: ClientPixelFormat,
        zrle: &mut TestZlibDecoder,
        zlib: &mut TestZlibDecoder,
        tight: &mut TestZlibDecoder,
    ) -> Vec<(u8, u8, u8)> {
        let width = usize::from(rect.width);
        let height = usize::from(rect.height);
        match rect.encoding {
            ENCODING_RAW => decode_raw_pixels(&rect.data, width, height, pixel),
            ENCODING_ZRLE => decode_zrle(zrle, &rect.data, width, height, pixel).0,
            ENCODING_HEXTILE => decode_hextile(&rect.data, width, height, pixel).0,
            ENCODING_ZLIB => decode_zlib_rect(zlib, &rect.data, width, height, pixel),
            ENCODING_TIGHT => decode_tight(tight, &rect.data, width, height, pixel).0,
            other => panic!("unexpected encoded rectangle {other}"),
        }
    }

    /// 64x32 pixels chosen to make consecutive Hextile tiles exercise the
    /// solid-reuse, new-background, foreground-subrect, coloured-subrect, and
    /// raw subencodings.
    fn hextile_pixels() -> Vec<(u8, u8, u8)> {
        const A: (u8, u8, u8) = (10, 20, 30);
        const B: (u8, u8, u8) = (200, 30, 40);
        const C: (u8, u8, u8) = (30, 200, 60);
        let mut pixels = Vec::with_capacity(64 * 32);
        for y in 0..32 {
            for x in 0..64 {
                let tile_x = x / 16;
                let tile_y = y / 16;
                let local_x = x % 16;
                let local_y = y % 16;
                let colour = match (tile_x, tile_y) {
                    (0, 0) | (1, 0) => A,
                    (2, 0) => {
                        if (local_x + local_y) % 2 == 0 {
                            A
                        } else {
                            B
                        }
                    }
                    (3, 0) => {
                        if local_x < 8 {
                            if local_y < 8 {
                                B
                            } else {
                                C
                            }
                        } else {
                            A
                        }
                    }
                    (0, 1) => {
                        let index = (local_y * 16 + local_x) as u8;
                        (index, index.wrapping_mul(2), index.wrapping_mul(3))
                    }
                    (1, 1) => {
                        if (local_x + local_y) % 2 == 0 {
                            A
                        } else {
                            B
                        }
                    }
                    (2, 1) | (3, 1) => B,
                    _ => unreachable!("64x32 is exactly a 4x2 tile grid"),
                };
                pixels.push(colour);
            }
        }
        pixels
    }

    /// More than 256 colours with high per-pixel entropy, so Tight keeps the
    /// copy filter (the gradient predictor cannot beat uncorrelated noise) and
    /// its payload goes through the per-client zlib stream.
    fn tight_copy_pixels(width: usize, height: usize) -> Vec<(u8, u8, u8)> {
        let mut state = 0x1234_5678u32;
        (0..width * height)
            .map(|_| {
                state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
                ((state >> 24) as u8, (state >> 16) as u8, (state >> 8) as u8)
            })
            .collect()
    }

    /// A smooth two-dimensional colour ramp with more than 256 colours, where
    /// the gradient predictor beats the copy filter after zlib.
    fn tight_gradient_pixels(width: usize, height: usize) -> Vec<(u8, u8, u8)> {
        (0..width * height)
            .map(|index| {
                let x = index % width;
                let y = index / width;
                (
                    ((x * 2 + y) % 256) as u8,
                    ((x + y * 3) % 256) as u8,
                    ((x / 2 + y * 2) % 256) as u8,
                )
            })
            .collect()
    }

    /// A rectangle with a fixed small palette, so Tight can use the palette
    /// filter. `colours == 2` exercises one-bit packing; more colours exercise
    /// one byte per pixel.
    fn tight_palette_pixels(
        width: usize,
        height: usize,
        colours: &[(u8, u8, u8)],
    ) -> Vec<(u8, u8, u8)> {
        (0..width * height)
            .map(|index| {
                let x = index % width;
                let y = index / width;
                colours[(x / 8 + y / 8 * 3) % colours.len()]
            })
            .collect()
    }

    fn frame_from_pixels(width: usize, height: usize, pixels: &[(u8, u8, u8)]) -> FrameSnapshot {
        let mut data = Vec::with_capacity(width * height * 3);
        for &(red, green, blue) in pixels {
            data.extend_from_slice(&[red, green, blue]);
        }
        FrameSnapshot {
            generation: 1,
            format: FrameFormat::rgb24(width as u32, height as u32),
            data: Bytes::from(data),
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

    fn client_pixel_format(
        depth: u8,
        big_endian: bool,
        red_shift: u8,
        green_shift: u8,
        blue_shift: u8,
    ) -> ClientPixelFormat {
        ClientPixelFormat {
            depth,
            big_endian,
            red_shift,
            green_shift,
            blue_shift,
        }
    }

    async fn set_pixel_format(stream: &mut TcpStream, pixel: ClientPixelFormat) {
        let mut message = vec![0u8, 0, 0, 0];
        message.extend_from_slice(&[32, pixel.depth, u8::from(pixel.big_endian), 1]);
        for _ in 0..3 {
            message.extend_from_slice(&255u16.to_be_bytes());
        }
        message.extend_from_slice(&[
            pixel.red_shift,
            pixel.green_shift,
            pixel.blue_shift,
            0,
            0,
            0,
        ]);
        write_all(stream, &message).await;
    }

    async fn set_encodings(stream: &mut TcpStream, encodings: &[i32]) {
        let mut message = vec![2u8, 0];
        message.extend_from_slice(&(encodings.len() as u16).to_be_bytes());
        for encoding in encodings {
            message.extend_from_slice(&encoding.to_be_bytes());
        }
        write_all(stream, &message).await;
    }

    #[derive(Debug)]
    struct WireRect {
        x: u16,
        y: u16,
        width: u16,
        height: u16,
        encoding: i32,
        data: Vec<u8>,
    }

    async fn read_update(stream: &mut TcpStream) -> Vec<WireRect> {
        read_update_with_pixel(stream, ClientPixelFormat::default()).await
    }

    async fn read_update_with_pixel(
        stream: &mut TcpStream,
        pixel: ClientPixelFormat,
    ) -> Vec<WireRect> {
        let mut header = [0u8; 4];
        read_exact(stream, &mut header).await;
        assert_eq!(header[0], 0, "expected FramebufferUpdate");
        let rectangles = u16::from_be_bytes([header[2], header[3]]);
        assert!(rectangles <= MAX_PENDING_RECTS as u16 + 2);
        let mut rects = Vec::new();
        for _ in 0..rectangles {
            let mut rect = [0u8; 12];
            read_exact(stream, &mut rect).await;
            let width = u16::from_be_bytes([rect[4], rect[5]]);
            let height = u16::from_be_bytes([rect[6], rect[7]]);
            let encoding = i32::from_be_bytes([rect[8], rect[9], rect[10], rect[11]]);
            let data = match encoding {
                ENCODING_RAW => {
                    let mut data = vec![0u8; usize::from(width) * usize::from(height) * 4];
                    read_exact(stream, &mut data).await;
                    data
                }
                ENCODING_ZRLE => {
                    let mut length = [0u8; 4];
                    read_exact(stream, &mut length).await;
                    let length = u32::from_be_bytes(length) as usize;
                    assert!(length <= MAX_ZRLE_RECT_BYTES);
                    let mut data = vec![0u8; length];
                    read_exact(stream, &mut data).await;
                    data
                }
                ENCODING_ZLIB => {
                    let mut length = [0u8; 4];
                    read_exact(stream, &mut length).await;
                    let length = u32::from_be_bytes(length) as usize;
                    assert!(length <= MAX_ZLIB_RECT_BYTES);
                    let mut data = vec![0u8; length];
                    read_exact(stream, &mut data).await;
                    data
                }
                ENCODING_TIGHT => read_tight_payload(stream, width, height, pixel).await,
                ENCODING_HEXTILE => read_hextile_payload(stream, width, height).await,
                ENCODING_EXTENDED_DESKTOP_SIZE => {
                    // One byte screen count, then 16 bytes per screen.
                    let mut screens = [0u8; 1];
                    read_exact(stream, &mut screens).await;
                    let mut data = vec![screens[0]];
                    let mut bodies = vec![0u8; usize::from(screens[0]) * 16];
                    read_exact(stream, &mut bodies).await;
                    data.extend_from_slice(&bodies);
                    data
                }
                ENCODING_CURSOR => Vec::new(),
                other => panic!("unexpected encoding {other}"),
            };
            rects.push(WireRect {
                x: u16::from_be_bytes([rect[0], rect[1]]),
                y: u16::from_be_bytes([rect[2], rect[3]]),
                width,
                height,
                encoding,
                data,
            });
        }
        rects
    }

    async fn read_hextile_payload(stream: &mut TcpStream, width: u16, height: u16) -> Vec<u8> {
        let tile_width = usize::from(width);
        let tile_height = usize::from(height);
        let mut data = Vec::new();
        for tile_y in (0..tile_height).step_by(HEXTILE_TILE_SIZE) {
            let current_height = HEXTILE_TILE_SIZE.min(tile_height - tile_y);
            for tile_x in (0..tile_width).step_by(HEXTILE_TILE_SIZE) {
                let current_width = HEXTILE_TILE_SIZE.min(tile_width - tile_x);
                let mut byte = [0u8; 1];
                read_exact(stream, &mut byte).await;
                let flags = byte[0];
                data.push(flags);
                let mut pixel = [0u8; 4];
                if flags & HEXTILE_RAW != 0 {
                    let mut raw = vec![0u8; current_width * current_height * 4];
                    read_exact(stream, &mut raw).await;
                    data.extend_from_slice(&raw);
                    continue;
                }
                if flags & HEXTILE_BACKGROUND_SPECIFIED != 0 {
                    read_exact(stream, &mut pixel).await;
                    data.extend_from_slice(&pixel);
                }
                if flags & HEXTILE_FOREGROUND_SPECIFIED != 0 {
                    read_exact(stream, &mut pixel).await;
                    data.extend_from_slice(&pixel);
                }
                if flags & HEXTILE_ANY_SUBRECTS != 0 {
                    read_exact(stream, &mut byte).await;
                    data.push(byte[0]);
                    let coloured = flags & HEXTILE_SUBRECTS_COLOURED != 0;
                    for _ in 0..byte[0] {
                        if coloured {
                            read_exact(stream, &mut pixel).await;
                            data.extend_from_slice(&pixel);
                        }
                        let mut xywh = [0u8; 2];
                        read_exact(stream, &mut xywh).await;
                        data.extend_from_slice(&xywh);
                    }
                }
            }
        }
        data
    }

    /// Read exactly one Tight rectangle's bytes from the wire. The payload has
    /// no outer length; its size is implied by the control byte, filter,
    /// palette size, and compact length, so the reader mirrors the decoder.
    async fn read_tight_payload(
        stream: &mut TcpStream,
        width: u16,
        height: u16,
        pixel: ClientPixelFormat,
    ) -> Vec<u8> {
        let width = usize::from(width);
        let height = usize::from(height);
        let tpixel = pixel.tight_pixel_len();
        let mut data = Vec::new();
        let mut byte = [0u8; 1];

        read_exact(stream, &mut byte).await;
        let control = byte[0];
        data.push(control);
        let high = (control & 0xf0) >> 4;

        if high == 8 {
            let mut fill = vec![0u8; tpixel];
            read_exact(stream, &mut fill).await;
            data.extend_from_slice(&fill);
            return data;
        }
        if high == 9 {
            let length = read_tight_compact_length(stream, &mut data).await;
            assert!(length <= TIGHT_MAX_RECT_BYTES);
            let mut jpeg = vec![0u8; length];
            read_exact(stream, &mut jpeg).await;
            data.extend_from_slice(&jpeg);
            return data;
        }
        assert!(
            high == 0 || high == 4,
            "unexpected Tight compression-control {control:#04x}"
        );

        let mut palette_size = 0usize;
        if control & TIGHT_EXPLICIT_FILTER != 0 {
            read_exact(stream, &mut byte).await;
            let filter = byte[0];
            data.push(filter);
            match filter {
                TIGHT_FILTER_PALETTE => {
                    read_exact(stream, &mut byte).await;
                    palette_size = usize::from(byte[0]) + 1;
                    data.push(byte[0]);
                    let mut palette = vec![0u8; palette_size * tpixel];
                    read_exact(stream, &mut palette).await;
                    data.extend_from_slice(&palette);
                }
                TIGHT_FILTER_GRADIENT => {}
                0 => {}
                other => panic!("unexpected Tight filter {other}"),
            }
        }

        let row_size = if palette_size != 0 {
            if palette_size <= 2 {
                (width + 7) / 8
            } else {
                width
            }
        } else {
            width * tpixel
        };
        let data_size = height * row_size;
        if data_size < TIGHT_MIN_TO_COMPRESS {
            let mut raw = vec![0u8; data_size];
            read_exact(stream, &mut raw).await;
            data.extend_from_slice(&raw);
        } else {
            let length = read_tight_compact_length(stream, &mut data).await;
            assert!(length <= TIGHT_MAX_RECT_BYTES);
            let mut compressed = vec![0u8; length];
            read_exact(stream, &mut compressed).await;
            data.extend_from_slice(&compressed);
        }
        data
    }

    /// Read one Tight compact length from the wire, appending its encoded bytes
    /// to `data` so a test decoder can consume the same byte slice.
    async fn read_tight_compact_length(stream: &mut TcpStream, data: &mut Vec<u8>) -> usize {
        let mut byte = [0u8; 1];
        let mut length_bytes = Vec::new();
        loop {
            read_exact(stream, &mut byte).await;
            length_bytes.push(byte[0]);
            if byte[0] & 0x80 == 0 {
                break;
            }
            assert!(length_bytes.len() < 3, "Tight compact length is too long");
        }
        data.extend_from_slice(&length_bytes);
        match length_bytes.len() {
            1 => usize::from(length_bytes[0] & 0x7f),
            2 => usize::from(length_bytes[0] & 0x7f) | (usize::from(length_bytes[1] & 0x7f) << 7),
            _ => {
                usize::from(length_bytes[0] & 0x7f)
                    | (usize::from(length_bytes[1] & 0x7f) << 7)
                    | (usize::from(length_bytes[2]) << 14)
            }
        }
    }

    /// Test-side decoder for a client's persistent zlib stream. The same
    /// wrapper is usable for ZRLE, ZLib, and Tight; they must be separate
    /// decoder instances just as the server keeps separate encoder streams.
    struct TestZlibDecoder {
        decompress: flate2::Decompress,
    }

    impl TestZlibDecoder {
        fn new() -> Self {
            Self {
                decompress: flate2::Decompress::new(true),
            }
        }

        /// The server sets a control-byte reset bit when it starts a new zlib
        /// stream (for example after a compression-level change).
        fn reset(&mut self) {
            self.decompress = flate2::Decompress::new(true);
        }

        fn inflate(&mut self, compressed: &[u8]) -> Vec<u8> {
            let mut output = Vec::new();
            let mut remaining = compressed;
            for _ in 0..1024 {
                output.reserve(64 * 1024);
                let before_in = self.decompress.total_in();
                let before_out = self.decompress.total_out();
                let status = self
                    .decompress
                    .decompress_vec(remaining, &mut output, flate2::FlushDecompress::Sync)
                    .expect("ZRLE zlib stream is valid");
                let consumed = (self.decompress.total_in() - before_in) as usize;
                let produced = self.decompress.total_out() - before_out;
                remaining = &remaining[consumed..];
                match status {
                    flate2::Status::Ok => {
                        assert!(remaining.is_empty(), "zlib input was not fully consumed");
                        return output;
                    }
                    flate2::Status::StreamEnd => return output,
                    flate2::Status::BufError => {
                        assert!(
                            consumed != 0 || produced != 0,
                            "zlib decoder made no progress"
                        );
                    }
                }
            }
            panic!("ZRLE zlib stream did not finish");
        }
    }

    fn decode_zrle(
        decoder: &mut TestZlibDecoder,
        compressed: &[u8],
        width: usize,
        height: usize,
        pixel: ClientPixelFormat,
    ) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
        let bytes = decoder.inflate(compressed);
        let cpixel_len = pixel.cpixel_len();
        let mut position = 0usize;
        let mut pixels = vec![(0u8, 0u8, 0u8); width * height];
        let mut subencodings = Vec::new();

        for tile_y in (0..height).step_by(ZRLE_TILE_SIZE) {
            let tile_height = ZRLE_TILE_SIZE.min(height - tile_y);
            for tile_x in (0..width).step_by(ZRLE_TILE_SIZE) {
                let tile_width = ZRLE_TILE_SIZE.min(width - tile_x);
                let tile_pixels = tile_width * tile_height;
                let subencoding = bytes[position];
                position += 1;
                subencodings.push(subencoding);
                match subencoding {
                    0 => {
                        for row in 0..tile_height {
                            for column in 0..tile_width {
                                let value = decode_cpixel(
                                    &bytes,
                                    &mut position,
                                    cpixel_len,
                                    pixel.big_endian,
                                );
                                pixels[(tile_y + row) * width + tile_x + column] =
                                    decode_rgb(value, pixel);
                            }
                        }
                    }
                    1 => {
                        let value =
                            decode_cpixel(&bytes, &mut position, cpixel_len, pixel.big_endian);
                        let colour = decode_rgb(value, pixel);
                        for row in 0..tile_height {
                            for column in 0..tile_width {
                                pixels[(tile_y + row) * width + tile_x + column] = colour;
                            }
                        }
                    }
                    2..=16 => {
                        let palette_size = subencoding as usize;
                        let mut palette = Vec::with_capacity(palette_size);
                        for _ in 0..palette_size {
                            palette.push(decode_rgb(
                                decode_cpixel(&bytes, &mut position, cpixel_len, pixel.big_endian),
                                pixel,
                            ));
                        }
                        let bits = if palette_size <= 2 {
                            1
                        } else if palette_size <= 4 {
                            2
                        } else {
                            4
                        };
                        let row_bytes = (tile_width * bits + 7) / 8;
                        for row in 0..tile_height {
                            let row_start = position;
                            position += row_bytes;
                            for column in 0..tile_width {
                                let bit = column * bits;
                                let shift = 8 - bits - (bit % 8);
                                let index =
                                    (bytes[row_start + bit / 8] >> shift) & ((1 << bits) - 1);
                                pixels[(tile_y + row) * width + tile_x + column] =
                                    palette[index as usize];
                            }
                        }
                    }
                    128 => {
                        let mut filled = 0usize;
                        while filled < tile_pixels {
                            let colour = decode_rgb(
                                decode_cpixel(&bytes, &mut position, cpixel_len, pixel.big_endian),
                                pixel,
                            );
                            let run = decode_run_length(&bytes, &mut position);
                            assert!(filled + run <= tile_pixels, "RLE run crossed the tile");
                            for _ in 0..run {
                                let index = filled;
                                pixels[(tile_y + index / tile_width) * width
                                    + tile_x
                                    + index % tile_width] = colour;
                                filled += 1;
                            }
                        }
                    }
                    130..=255 => {
                        let palette_size = subencoding as usize - 128;
                        let mut palette = Vec::with_capacity(palette_size);
                        for _ in 0..palette_size {
                            palette.push(decode_rgb(
                                decode_cpixel(&bytes, &mut position, cpixel_len, pixel.big_endian),
                                pixel,
                            ));
                        }
                        let mut filled = 0usize;
                        while filled < tile_pixels {
                            let index = bytes[position];
                            position += 1;
                            let (palette_index, run) = if index & 0x80 != 0 {
                                (
                                    (index & 0x7f) as usize,
                                    decode_run_length(&bytes, &mut position),
                                )
                            } else {
                                (index as usize, 1)
                            };
                            assert!(filled + run <= tile_pixels, "RLE run crossed the tile");
                            for _ in 0..run {
                                let index = filled;
                                pixels[(tile_y + index / tile_width) * width
                                    + tile_x
                                    + index % tile_width] = palette[palette_index];
                                filled += 1;
                            }
                        }
                    }
                    other => panic!("invalid ZRLE sub-encoding {other}"),
                }
            }
        }
        assert_eq!(position, bytes.len(), "trailing ZRLE tile bytes");
        (pixels, subencodings)
    }

    fn decode_zlib_rect(
        decoder: &mut TestZlibDecoder,
        compressed: &[u8],
        width: usize,
        height: usize,
        pixel: ClientPixelFormat,
    ) -> Vec<(u8, u8, u8)> {
        let bytes = decoder.inflate(compressed);
        assert_eq!(
            bytes.len(),
            width * height * pixel.pixel_len(),
            "ZLib rectangle payload must be uncompressed Raw pixels"
        );
        let mut position = 0usize;
        let mut pixels = Vec::with_capacity(width * height);
        for _ in 0..width * height {
            pixels.push(decode_rgb(
                decode_pixel(&bytes, &mut position, pixel.big_endian),
                pixel,
            ));
        }
        assert_eq!(position, bytes.len(), "trailing ZLib pixel bytes");
        pixels
    }

    fn decode_tight_pixel(bytes: &[u8], position: &mut usize, pixel: ClientPixelFormat) -> u32 {
        if pixel.tight_pixel_len() == 3 {
            let red = bytes[*position];
            let green = bytes[*position + 1];
            let blue = bytes[*position + 2];
            *position += 3;
            pixel.pixel_value(red, green, blue)
        } else {
            decode_pixel(bytes, position, pixel.big_endian)
        }
    }

    fn decode_tight_compact_length(bytes: &[u8], position: &mut usize) -> usize {
        let first = bytes[*position];
        *position += 1;
        let mut length = usize::from(first & 0x7f);
        if first & 0x80 != 0 {
            let second = bytes[*position];
            *position += 1;
            length |= usize::from(second & 0x7f) << 7;
            if second & 0x80 != 0 {
                let third = bytes[*position];
                *position += 1;
                length |= usize::from(third) << 14;
            }
        }
        length
    }

    /// Test-side decoder for one Tight rectangle: FillCompression,
    /// JpegCompression, and BasicCompression with the copy, palette, or gradient
    /// filter. `decoder` is the client's persistent Tight zlib stream and must
    /// be shared across successive rectangles.
    fn decode_tight(
        decoder: &mut TestZlibDecoder,
        data: &[u8],
        width: usize,
        height: usize,
        pixel: ClientPixelFormat,
    ) -> (Vec<(u8, u8, u8)>, u8) {
        let tpixel = pixel.tight_pixel_len();
        let mut position = 0usize;
        let control = data[position];
        position += 1;
        if control & 0x01 != 0 {
            decoder.reset();
        }
        let kind = control & 0xf0;
        let mut pixels = vec![(0u8, 0u8, 0u8); width * height];

        if kind == TIGHT_FILL {
            let value = decode_tight_pixel(data, &mut position, pixel);
            pixels.fill(decode_rgb(value, pixel));
            assert_eq!(position, data.len(), "trailing Tight fill bytes");
            return (pixels, control);
        }
        if kind == TIGHT_JPEG {
            let length = decode_tight_compact_length(data, &mut position);
            let jpeg = &data[position..position + length];
            position += length;
            assert_eq!(position, data.len(), "trailing Tight JPEG bytes");
            return (decode_jpeg(jpeg, width, height), control);
        }

        assert!(
            kind == 0 || kind == TIGHT_EXPLICIT_FILTER,
            "expected basic Tight compression, got {control:#04x}"
        );
        let mut palette = Vec::new();
        let mut gradient = false;
        if control & TIGHT_EXPLICIT_FILTER != 0 {
            let filter = data[position];
            position += 1;
            match filter {
                TIGHT_FILTER_PALETTE => {
                    let palette_size = usize::from(data[position]) + 1;
                    position += 1;
                    palette.reserve(palette_size);
                    for _ in 0..palette_size {
                        palette.push(decode_tight_pixel(data, &mut position, pixel));
                    }
                }
                TIGHT_FILTER_GRADIENT => gradient = true,
                0 => {}
                other => panic!("unexpected Tight filter {other}"),
            }
        }

        let row_size = if palette.is_empty() {
            width * tpixel
        } else if palette.len() <= 2 {
            (width + 7) / 8
        } else {
            width
        };
        let data_size = height * row_size;
        let filtered = if data_size < TIGHT_MIN_TO_COMPRESS {
            let raw = data[position..position + data_size].to_vec();
            position += data_size;
            raw
        } else {
            let length = decode_tight_compact_length(data, &mut position);
            assert!(length <= TIGHT_MAX_RECT_BYTES);
            let compressed = &data[position..position + length];
            position += length;
            decoder.inflate(compressed)
        };
        assert_eq!(position, data.len(), "trailing Tight rectangle bytes");
        assert_eq!(filtered.len(), data_size, "wrong Tight filtered data size");

        if palette.is_empty() && gradient {
            let mut previous = vec![[0u8; 3]; width];
            let mut row = vec![[0u8; 3]; width];
            let mut source = 0usize;
            for y in 0..height {
                for x in 0..width {
                    let value = decode_tight_pixel(&filtered, &mut source, pixel);
                    let (red, green, blue) = decode_rgb(value, pixel);
                    let predicted = if x == 0 {
                        previous[0]
                    } else {
                        [
                            gradient_predict(previous[x][0], row[x - 1][0], previous[x - 1][0]),
                            gradient_predict(previous[x][1], row[x - 1][1], previous[x - 1][1]),
                            gradient_predict(previous[x][2], row[x - 1][2], previous[x - 1][2]),
                        ]
                    };
                    let actual = [
                        red.wrapping_add(predicted[0]),
                        green.wrapping_add(predicted[1]),
                        blue.wrapping_add(predicted[2]),
                    ];
                    row[x] = actual;
                    pixels[y * width + x] = (actual[0], actual[1], actual[2]);
                }
                std::mem::swap(&mut previous, &mut row);
            }
            assert_eq!(source, filtered.len(), "trailing Tight gradient bytes");
        } else if palette.is_empty() {
            let mut source = 0usize;
            for slot in pixels.iter_mut() {
                *slot = decode_rgb(decode_tight_pixel(&filtered, &mut source, pixel), pixel);
            }
        } else if palette.len() <= 2 {
            for row in 0..height {
                for column in 0..width {
                    let byte = filtered[row * row_size + column / 8];
                    let index = (byte >> (7 - (column % 8))) & 1;
                    pixels[row * width + column] = decode_rgb(palette[usize::from(index)], pixel);
                }
            }
        } else {
            for row in 0..height {
                for column in 0..width {
                    let index = usize::from(filtered[row * width + column]);
                    assert!(index < palette.len(), "palette index out of range");
                    pixels[row * width + column] = decode_rgb(palette[index], pixel);
                }
            }
        }
        (pixels, control)
    }

    fn decode_jpeg(data: &[u8], width: usize, height: usize) -> Vec<(u8, u8, u8)> {
        let mut decoder = jpeg_decoder::Decoder::new(std::io::Cursor::new(data));
        let decoded = decoder.decode().expect("Tight JPEG payload must decode");
        let info = decoder.info().expect("Tight JPEG must carry image info");
        assert_eq!(usize::from(info.width), width, "wrong Tight JPEG width");
        assert_eq!(usize::from(info.height), height, "wrong Tight JPEG height");
        assert_eq!(
            decoded.len(),
            width * height * 3,
            "wrong Tight JPEG data size"
        );
        decoded
            .chunks_exact(3)
            .map(|rgb| (rgb[0], rgb[1], rgb[2]))
            .collect()
    }

    fn decode_hextile(
        data: &[u8],
        width: usize,
        height: usize,
        pixel: ClientPixelFormat,
    ) -> (Vec<(u8, u8, u8)>, Vec<u8>) {
        let mut position = 0usize;
        let mut pixels = vec![(0u8, 0u8, 0u8); width * height];
        let mut subencodings = Vec::new();
        let mut background = 0u32;
        let mut foreground = 0u32;

        for tile_y in (0..height).step_by(HEXTILE_TILE_SIZE) {
            let tile_height = HEXTILE_TILE_SIZE.min(height - tile_y);
            for tile_x in (0..width).step_by(HEXTILE_TILE_SIZE) {
                let tile_width = HEXTILE_TILE_SIZE.min(width - tile_x);
                let subencoding = data[position];
                position += 1;
                subencodings.push(subencoding);
                if subencoding & HEXTILE_RAW != 0 {
                    assert_eq!(
                        subencoding & !HEXTILE_RAW,
                        0,
                        "a raw Hextile subencoding must not set other bits"
                    );
                    for row in 0..tile_height {
                        for column in 0..tile_width {
                            let value = decode_pixel(data, &mut position, pixel.big_endian);
                            pixels[(tile_y + row) * width + tile_x + column] =
                                decode_rgb(value, pixel);
                        }
                    }
                    continue;
                }
                if subencoding & HEXTILE_BACKGROUND_SPECIFIED != 0 {
                    background = decode_pixel(data, &mut position, pixel.big_endian);
                }
                if subencoding & HEXTILE_FOREGROUND_SPECIFIED != 0 {
                    foreground = decode_pixel(data, &mut position, pixel.big_endian);
                }
                let background_rgb = decode_rgb(background, pixel);
                for row in 0..tile_height {
                    for column in 0..tile_width {
                        pixels[(tile_y + row) * width + tile_x + column] = background_rgb;
                    }
                }
                if subencoding & HEXTILE_ANY_SUBRECTS != 0 {
                    let count = data[position];
                    position += 1;
                    let coloured = subencoding & HEXTILE_SUBRECTS_COLOURED != 0;
                    for _ in 0..count {
                        let colour = if coloured {
                            decode_rgb(decode_pixel(data, &mut position, pixel.big_endian), pixel)
                        } else {
                            decode_rgb(foreground, pixel)
                        };
                        let x_and_y = data[position];
                        let width_and_height = data[position + 1];
                        position += 2;
                        let x = usize::from(x_and_y >> 4);
                        let y = usize::from(x_and_y & 0x0f);
                        let subrect_width = usize::from(width_and_height >> 4) + 1;
                        let subrect_height = usize::from(width_and_height & 0x0f) + 1;
                        assert!(
                            x + subrect_width <= tile_width && y + subrect_height <= tile_height,
                            "Hextile subrect crosses its tile"
                        );
                        for row in 0..subrect_height {
                            for column in 0..subrect_width {
                                pixels[(tile_y + y + row) * width + tile_x + x + column] = colour;
                            }
                        }
                    }
                }
            }
        }
        assert_eq!(position, data.len(), "trailing Hextile tile bytes");
        (pixels, subencodings)
    }

    fn decode_pixel(bytes: &[u8], position: &mut usize, big_endian: bool) -> u32 {
        let end = *position + 4;
        let array: [u8; 4] = bytes[*position..end].try_into().unwrap();
        *position = end;
        if big_endian {
            u32::from_be_bytes(array)
        } else {
            u32::from_le_bytes(array)
        }
    }

    fn decode_cpixel(
        bytes: &[u8],
        position: &mut usize,
        cpixel_len: usize,
        big_endian: bool,
    ) -> u32 {
        let end = *position + cpixel_len;
        let slice = &bytes[*position..end];
        *position = end;
        if cpixel_len == 4 {
            let array: [u8; 4] = slice.try_into().unwrap();
            if big_endian {
                u32::from_be_bytes(array)
            } else {
                u32::from_le_bytes(array)
            }
        } else {
            let array: [u8; 3] = slice.try_into().unwrap();
            if big_endian {
                u32::from_be_bytes([0, array[0], array[1], array[2]])
            } else {
                u32::from_le_bytes([array[0], array[1], array[2], 0])
            }
        }
    }

    fn decode_rgb(value: u32, pixel: ClientPixelFormat) -> (u8, u8, u8) {
        (
            ((value >> pixel.red_shift) & 0xff) as u8,
            ((value >> pixel.green_shift) & 0xff) as u8,
            ((value >> pixel.blue_shift) & 0xff) as u8,
        )
    }

    fn decode_run_length(bytes: &[u8], position: &mut usize) -> usize {
        let mut sum = 0usize;
        loop {
            let byte = bytes[*position];
            *position += 1;
            sum += byte as usize;
            if byte != 255 {
                return sum + 1;
            }
        }
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

    async fn start_server_with_clipboard(
        source: Box<dyn CaptureSource>,
        sink: SharedSink,
        clipboard: Arc<dyn ClipboardAccess>,
        max_bytes: usize,
    ) -> VncServerHandle {
        let config = VncServerConfig {
            name: "Ozma test".to_owned(),
            max_clients: 1,
            clipboard: Some(clipboard),
            clipboard_max_bytes: max_bytes,
            ..VncServerConfig::default()
        };
        start(config, source, sink).await.expect("server starts")
    }

    async fn read_server_cut_text(stream: &mut TcpStream) -> Vec<u8> {
        let mut header = [0u8; 4];
        read_exact(stream, &mut header).await;
        assert_eq!(header[0], 3, "expected ServerCutText");
        let mut length = [0u8; 4];
        read_exact(stream, &mut length).await;
        let mut text = vec![0u8; u32::from_be_bytes(length) as usize];
        read_exact(stream, &mut text).await;
        text
    }

    async fn write_client_cut_text(stream: &mut TcpStream, bytes: &[u8]) {
        let mut message = vec![6u8, 0, 0, 0];
        message.extend_from_slice(&(bytes.len() as u32).to_be_bytes());
        message.extend_from_slice(bytes);
        write_all(stream, &message).await;
    }

    async fn assert_no_server_cut_text(stream: &mut TcpStream) {
        let mut byte = [0u8; 1];
        match tokio::time::timeout(Duration::from_millis(200), stream.read(&mut byte)).await {
            Err(_) => {}
            Ok(other) => panic!("expected no ServerCutText, got {other:?}"),
        }
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

        // Raw listed before Hextile: the client's order keeps Raw selected.
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
    async fn client_cut_text_decodes_latin1_sanitizes_controls_and_writes_clipboard() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let clipboard = FakeClipboard::new(None);
        let server = start_server_with_clipboard(
            Box::new(TestSource::new(2, 2)),
            sink,
            clipboard.clone(),
            64,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        // The empty current clipboard is still a valid sync.
        assert!(read_server_cut_text(&mut client).await.is_empty());

        // RFB 6143 uses ISO-8859-1: 0xE9 is `é`. NUL and 0x01 are control bytes
        // and must not reach the desktop clipboard; tab/CR/LF remain.
        write_client_cut_text(
            &mut client,
            &[b'A', 0xE9, 0x00, 0x01, b'\t', b'X', b'\r', b'\n'],
        )
        .await;

        wait_for(|| clipboard.text().as_deref() == Some("Aé\tX\r\n")).await;
        let status = server.clipboard_status();
        assert!(status.enabled);
        assert!(status.available);
        assert_eq!(status.last_direction, ClipboardSyncDirection::ViewerToNode);
        assert_eq!(status.from_viewers, 1);
        assert!(status.last_sync_unix_ms > 0);
        assert!(!format!("{status:?}").contains("Aé"));
        server.shutdown().await;
    }

    #[tokio::test]
    async fn client_cut_text_over_the_configured_bound_is_ignored_without_desync() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let clipboard = FakeClipboard::new(None);
        let server = start_server_with_clipboard(
            Box::new(TestSource::new(2, 2)),
            sink,
            clipboard.clone(),
            4,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        assert!(read_server_cut_text(&mut client).await.is_empty());

        // Five bytes exceed the four-byte bound; the body is still consumed so
        // the following four-byte message arrives intact.
        write_client_cut_text(&mut client, b"abcde").await;
        write_client_cut_text(&mut client, b"okay").await;

        wait_for(|| clipboard.text().as_deref() == Some("okay")).await;
        assert_eq!(clipboard.writes(), 1, "the oversized text was not written");
        let status = server.clipboard_status();
        assert_eq!(status.from_viewers, 1);
        assert_eq!(status.rejected, 1);
        assert_eq!(server.stats().clients_connected, 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn unavailable_clipboard_ignores_cut_text_and_reports_no_sync() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let clipboard = FakeClipboard::unavailable();
        let server = start_server_with_clipboard(
            Box::new(TestSource::new(2, 2)),
            sink,
            clipboard.clone(),
            64,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        // No ServerCutText when the backend is unavailable.
        assert_no_server_cut_text(&mut client).await;

        write_client_cut_text(&mut client, b"hello").await;
        tokio::time::sleep(Duration::from_millis(50)).await;

        assert_eq!(clipboard.writes(), 0);
        assert_eq!(clipboard.text(), None);
        let status = server.clipboard_status();
        assert!(status.enabled);
        assert!(!status.available);
        assert_eq!(status.last_direction, ClipboardSyncDirection::None);
        assert_eq!(status.from_viewers, 0);
        assert_eq!(status.to_viewers, 0);
        assert!(status.rejected >= 1);
        assert_eq!(server.stats().clients_connected, 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn server_cut_text_is_sent_on_connect_as_latin1() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let clipboard = FakeClipboard::new(Some("hello é"));
        let server = start_server_with_clipboard(
            Box::new(TestSource::new(2, 2)),
            sink,
            clipboard.clone(),
            64,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        let text = read_server_cut_text(&mut client).await;
        assert_eq!(text, b"hello \xE9", "ServerCutText must be ISO-8859-1");
        let status = server.clipboard_status();
        assert!(status.available);
        assert_eq!(status.last_direction, ClipboardSyncDirection::NodeToViewer);
        assert_eq!(status.to_viewers, 1);
        assert!(status.last_sync_unix_ms > 0);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn server_cut_text_is_skipped_when_the_text_is_not_latin1() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let clipboard = FakeClipboard::new(Some("hello 😀"));
        let server = start_server_with_clipboard(
            Box::new(TestSource::new(2, 2)),
            sink,
            clipboard.clone(),
            64,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        // The text cannot be represented in ISO-8859-1, so nothing is sent and
        // no sync is claimed.
        assert_no_server_cut_text(&mut client).await;
        let status = server.clipboard_status();
        assert_eq!(status.to_viewers, 0);
        assert_eq!(status.last_direction, ClipboardSyncDirection::None);
        assert!(status.rejected >= 1);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn a_viewers_cut_text_is_not_echoed_back_to_the_same_viewer() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let clipboard = FakeClipboard::new(Some("seed"));
        let server = start_server_with_clipboard(
            Box::new(TestSource::new(2, 2)),
            sink,
            clipboard.clone(),
            64,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        assert_eq!(read_server_cut_text(&mut client).await, b"seed");

        write_client_cut_text(&mut client, b"new").await;
        wait_for(|| clipboard.text().as_deref() == Some("new")).await;
        assert_no_server_cut_text(&mut client).await;

        let status = server.clipboard_status();
        assert_eq!(status.from_viewers, 1);
        assert_eq!(status.to_viewers, 1, "only the connect-time sync is sent");
        server.shutdown().await;
    }

    #[tokio::test]
    async fn client_cut_text_above_the_protocol_cap_closes_the_viewer() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let clipboard = FakeClipboard::new(None);
        let server =
            start_server_with_clipboard(Box::new(TestSource::new(2, 2)), sink, clipboard, 64).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        assert!(read_server_cut_text(&mut client).await.is_empty());

        let mut header = vec![6u8, 0, 0, 0];
        header.extend_from_slice(&(MAX_CUT_TEXT_BYTES + 1).to_be_bytes());
        write_all(&mut client, &header).await;
        assert_connection_closed(&mut client).await;
        wait_for(|| server.stats().protocol_errors >= 1).await;
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
                depth: 24,
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

    #[tokio::test]
    async fn zrle_decodes_the_known_framebuffer_in_rgb_and_bgr_formats() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(PatternSource::new(256, 64)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let expected = pattern_pixels(256, 64);

        // One connection, one persistent zlib stream, three pixel formats.
        // Decoding each rectangle through the same decoder also proves the
        // server did not silently reset the stream between updates.
        let mut decoder = TestZlibDecoder::new();
        for (depth, big_endian, red_shift, green_shift, blue_shift) in [
            (24, false, 16, 8, 0), // RGB order, little-endian, 3-byte CPIXEL
            (24, false, 0, 8, 16), // BGR order, little-endian, 3-byte CPIXEL
            (24, true, 0, 8, 16),  // BGR order, big-endian, 3-byte CPIXEL
            (32, false, 16, 8, 0), // depth 32 forces a 4-byte CPIXEL
        ] {
            let format = client_pixel_format(depth, big_endian, red_shift, green_shift, blue_shift);
            set_pixel_format(&mut client, format).await;
            set_encodings(&mut client, &[ENCODING_ZRLE]).await;
            request_update(&mut client, false, 256, 64).await;
            let rects = read_update(&mut client).await;
            assert_eq!(rects.len(), 1);
            assert_eq!(rects[0].encoding, ENCODING_ZRLE);
            assert_eq!((rects[0].x, rects[0].y), (0, 0));
            assert_eq!((rects[0].width, rects[0].height), (256, 64));

            let (pixels, subencodings) = decode_zrle(&mut decoder, &rects[0].data, 256, 64, format);
            assert_eq!(pixels, expected);
            assert_eq!(subencodings.len(), 4);
            assert!(
                subencodings.contains(&1),
                "solid tile should use sub-encoding 1: {subencodings:?}"
            );
            assert!(
                subencodings.iter().any(|sub| (2..=16).contains(sub)),
                "packed-palette tile should use sub-encoding 2..=16: {subencodings:?}"
            );
            assert!(
                subencodings.iter().any(|sub| *sub == 128),
                "128-colour tile should use plain RLE: {subencodings:?}"
            );
            assert!(
                subencodings.iter().any(|sub| *sub >= 130),
                "run-heavy palette tile should use palette RLE: {subencodings:?}"
            );
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn encoding_preference_honours_all_supported_codecs_in_client_order() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(4, 2).solid(10, 20, 30)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        let cases: &[(&[i32], i32)] = &[
            // A single advertised encoding is used as-is.
            (&[ENCODING_ZRLE], ENCODING_ZRLE),
            (&[ENCODING_RAW], ENCODING_RAW),
            (&[ENCODING_HEXTILE], ENCODING_HEXTILE),
            (&[ENCODING_ZLIB], ENCODING_ZLIB),
            (&[ENCODING_TIGHT], ENCODING_TIGHT),
            // The client's order decides between every supported pair.
            (&[ENCODING_RAW, ENCODING_ZRLE], ENCODING_RAW),
            (&[ENCODING_ZRLE, ENCODING_RAW], ENCODING_ZRLE),
            (&[ENCODING_HEXTILE, ENCODING_ZLIB], ENCODING_HEXTILE),
            (&[ENCODING_ZLIB, ENCODING_HEXTILE], ENCODING_ZLIB),
            (&[ENCODING_HEXTILE, ENCODING_ZRLE], ENCODING_HEXTILE),
            (&[ENCODING_ZLIB, ENCODING_ZRLE], ENCODING_ZLIB),
            (&[ENCODING_TIGHT, ENCODING_RAW], ENCODING_TIGHT),
            (&[ENCODING_RAW, ENCODING_TIGHT], ENCODING_RAW),
            (&[ENCODING_TIGHT, ENCODING_ZRLE], ENCODING_TIGHT),
            (&[ENCODING_ZRLE, ENCODING_TIGHT], ENCODING_ZRLE),
            (&[ENCODING_ZLIB, ENCODING_TIGHT], ENCODING_ZLIB),
            (&[ENCODING_TIGHT, ENCODING_ZLIB], ENCODING_TIGHT),
            (
                &[ENCODING_ZRLE, ENCODING_HEXTILE, ENCODING_ZLIB, ENCODING_RAW],
                ENCODING_ZRLE,
            ),
            (
                &[
                    ENCODING_TIGHT,
                    ENCODING_ZRLE,
                    ENCODING_HEXTILE,
                    ENCODING_ZLIB,
                    ENCODING_RAW,
                ],
                ENCODING_TIGHT,
            ),
            // An unsupported encoding is ignored without disturbing the
            // supported entries around it.
            (&[987654, ENCODING_ZLIB], ENCODING_ZLIB),
            (&[ENCODING_HEXTILE, 987654, ENCODING_RAW], ENCODING_HEXTILE),
            // An all-unsupported or empty list falls back to Raw.
            (&[987654], ENCODING_RAW),
            (&[], ENCODING_RAW),
        ];
        for (encodings, expected) in cases {
            set_encodings(&mut client, encodings).await;
            request_update(&mut client, false, 4, 2).await;
            let rects = read_update(&mut client).await;
            assert_eq!(
                rects[0].encoding, *expected,
                "encodings {encodings:?} selected the wrong codec"
            );
        }

        server.shutdown().await;
    }

    #[tokio::test]
    async fn cursor_pseudo_encoding_sends_a_typed_empty_cursor_only_when_requested() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(4, 2)), sink, 1).await;

        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        set_encodings(&mut client, &[ENCODING_CURSOR, ENCODING_RAW]).await;
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update(&mut client).await;
        assert_eq!(rects.len(), 2, "Raw pixels plus one Cursor rectangle");
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        let cursor = &rects[1];
        assert_eq!(cursor.encoding, ENCODING_CURSOR);
        assert_eq!(
            (cursor.x, cursor.y, cursor.width, cursor.height),
            (0, 0, 0, 0),
            "an unknown cursor must be the typed empty cursor, not a fabricated shape"
        );
        assert!(cursor.data.is_empty());

        // The empty cursor is a one-time shape notification.
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update(&mut client).await;
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        drop(client);
        wait_for(|| server.stats().clients_connected == 0).await;

        // A client that did not request Cursor never receives that rectangle.
        let mut plain = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut plain).await;
        set_encodings(&mut plain, &[ENCODING_RAW]).await;
        request_update(&mut plain, false, 4, 2).await;
        let rects = read_update(&mut plain).await;
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        server.shutdown().await;
    }

    fn first_rect_encoding(bytes: &[u8]) -> i32 {
        i32::from_be_bytes([bytes[12], bytes[13], bytes[14], bytes[15]])
    }

    fn assert_all_hextile_variants(subencodings: &[u8]) {
        assert!(
            subencodings.contains(&HEXTILE_RAW),
            "a raw tile should be present: {subencodings:?}"
        );
        assert!(
            subencodings.iter().any(|sub| *sub == 0),
            "a solid tile reusing the background should be present: {subencodings:?}"
        );
        assert!(
            subencodings
                .iter()
                .any(|sub| sub & HEXTILE_BACKGROUND_SPECIFIED != 0 && sub & HEXTILE_RAW == 0),
            "a new background should be specified: {subencodings:?}"
        );
        assert!(
            subencodings.iter().any(|sub| {
                sub & HEXTILE_ANY_SUBRECTS != 0
                    && sub & HEXTILE_FOREGROUND_SPECIFIED != 0
                    && sub & HEXTILE_SUBRECTS_COLOURED == 0
            }),
            "a non-coloured foreground subrect tile should be present: {subencodings:?}"
        );
        assert!(
            subencodings
                .iter()
                .any(|sub| sub & HEXTILE_SUBRECTS_COLOURED != 0),
            "a coloured subrect tile should be present: {subencodings:?}"
        );
    }

    #[test]
    fn hextile_all_subencodings_decode_in_rgb_and_bgr() {
        let (width, height) = (64, 32);
        let expected = hextile_pixels();
        let frame = frame_from_pixels(width, height, &expected);

        for pixel in [
            client_pixel_format(24, false, 16, 8, 0),
            client_pixel_format(24, true, 0, 8, 16),
        ] {
            let mut encoded = Vec::new();
            encode_hextile_rect(
                &frame,
                pixel,
                Rect {
                    x: 0,
                    y: 0,
                    width: width as u16,
                    height: height as u16,
                },
                &mut encoded,
            )
            .expect("Hextile encode");
            let (decoded, subencodings) = decode_hextile(&encoded, width, height, pixel);
            assert_eq!(decoded, expected);
            assert_all_hextile_variants(&subencodings);
        }
    }

    #[test]
    fn hextile_handles_edge_tiles_and_rectangle_offsets() {
        let (frame_width, frame_height) = (40usize, 40usize);
        let pixels: Vec<(u8, u8, u8)> = (0..frame_width * frame_height)
            .map(|index| {
                (
                    (index % 251) as u8,
                    ((index * 3) % 253) as u8,
                    ((index * 7) % 255) as u8,
                )
            })
            .collect();
        let frame = frame_from_pixels(frame_width, frame_height, &pixels);
        let rect = Rect {
            x: 3,
            y: 5,
            width: 20,
            height: 17,
        };
        let pixel = client_pixel_format(24, false, 0, 8, 16);

        let mut encoded = Vec::new();
        encode_hextile_rect(&frame, pixel, rect, &mut encoded).expect("Hextile encode");
        let (decoded, _) = decode_hextile(&encoded, 20, 17, pixel);

        let mut expected = Vec::with_capacity(20 * 17);
        for y in 5..5 + 17 {
            for x in 3..3 + 20 {
                expected.push(pixels[y * frame_width + x]);
            }
        }
        assert_eq!(decoded, expected);
    }

    #[test]
    fn hextile_and_zlib_bounds_are_rejected_before_writing() {
        let pixels = vec![(1u8, 2u8, 3u8); 4 * 2];
        let frame = frame_from_pixels(4, 2, &pixels);
        let pixel = ClientPixelFormat::default();

        for rect in [
            Rect {
                x: 0,
                y: 0,
                width: 0,
                height: 2,
            },
            Rect {
                x: 2,
                y: 0,
                width: 4,
                height: 2,
            },
        ] {
            let mut hextile = Vec::new();
            let error = encode_hextile_rect(&frame, pixel, rect, &mut hextile)
                .expect_err("invalid Hextile rectangle must fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(hextile.is_empty(), "invalid Hextile geometry wrote bytes");

            let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
            let error = encode_zlib_rect(&frame, pixel, rect, &mut zlib)
                .expect_err("invalid ZLib rectangle must fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(
                zlib.get_ref().is_empty(),
                "invalid ZLib geometry touched the stream"
            );

            let mut tight = TightStream::new(TIGHT_DEFAULT_COMPRESSION);
            let mut tight_out = Vec::new();
            let error = encode_tight_rect(&frame, pixel, rect, &mut tight, None, &mut tight_out)
                .expect_err("invalid Tight rectangle must fail");
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
            assert!(tight_out.is_empty(), "invalid Tight geometry wrote bytes");
            assert!(
                tight.encoder.get_ref().is_empty(),
                "invalid Tight geometry touched the stream"
            );
        }
    }

    #[test]
    fn tight_sub_rects_respect_the_width_and_area_bounds() {
        for rect in [
            Rect {
                x: 0,
                y: 0,
                width: 1,
                height: 1,
            },
            Rect {
                x: 7,
                y: 9,
                width: TIGHT_MAX_WIDTH as u16,
                height: 5,
            },
            Rect {
                x: 0,
                y: 0,
                width: (TIGHT_MAX_WIDTH + 1) as u16,
                height: 5,
            },
            Rect {
                x: 1,
                y: 2,
                width: 4096,
                height: 2300,
            },
            Rect {
                x: 0,
                y: 0,
                width: u16::MAX,
                height: 3,
            },
        ] {
            let pieces = tight_sub_rects(rect);
            assert!(!pieces.is_empty(), "a non-empty rectangle must split");
            let mut covered = 0usize;
            for piece in &pieces {
                assert!(piece.width as usize <= TIGHT_MAX_WIDTH);
                assert!(
                    piece.width as usize * piece.height as usize <= TIGHT_MAX_PIXELS,
                    "piece {:?} exceeds the Tight area bound",
                    piece
                );
                assert!(piece.width > 0 && piece.height > 0);
                assert!(piece.x >= rect.x && piece.y >= rect.y);
                assert!(
                    u32::from(piece.x) + u32::from(piece.width)
                        <= u32::from(rect.x) + u32::from(rect.width)
                );
                assert!(
                    u32::from(piece.y) + u32::from(piece.height)
                        <= u32::from(rect.y) + u32::from(rect.height)
                );
                covered += piece.width as usize * piece.height as usize;
            }
            assert_eq!(covered, rect.width as usize * rect.height as usize);
        }
    }

    #[test]
    fn tight_wire_rectangle_splitting_stays_within_bounds() {
        // A solid 4100x600 frame is wider than the 2048-pixel Tight limit, so
        // the encoder must split it. Each wire rectangle is a FillCompression
        // payload of one TPIXEL, which this test parses independently.
        let width = 4100usize;
        let height = 600usize;
        let pixels = vec![(12u8, 34u8, 56u8); width * height];
        let frame = frame_from_pixels(width, height, &pixels);
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        let mut encoder = TightStream::new(TIGHT_DEFAULT_COMPRESSION);
        let mut out = Vec::new();
        let count = encode_tight_rect(
            &frame,
            pixel,
            Rect {
                x: 0,
                y: 0,
                width: width as u16,
                height: height as u16,
            },
            &mut encoder,
            None,
            &mut out,
        )
        .expect("Tight encode");
        assert!(count > 1, "a 4100-pixel-wide rectangle must be split");

        let mut position = 0usize;
        let mut covered = 0usize;
        for _ in 0..count {
            let header = &out[position..position + 12];
            position += 12;
            let rect_width = u16::from_be_bytes([header[4], header[5]]) as usize;
            let rect_height = u16::from_be_bytes([header[6], header[7]]) as usize;
            assert_eq!(
                i32::from_be_bytes([header[8], header[9], header[10], header[11]]),
                ENCODING_TIGHT
            );
            assert!(rect_width <= TIGHT_MAX_WIDTH);
            assert!(rect_width * rect_height <= TIGHT_MAX_PIXELS);
            assert_eq!(out[position], TIGHT_FILL);
            position += 1 + pixel.tight_pixel_len();
            covered += rect_width * rect_height;
        }
        assert_eq!(position, out.len(), "trailing Tight bytes");
        assert_eq!(covered, width * height);
    }

    #[test]
    fn tight_copy_palette_and_fill_decode_in_rgb_and_bgr() {
        let colours = [
            (10u8, 20u8, 30u8),
            (200, 30, 40),
            (30, 200, 60),
            (220, 220, 10),
            (40, 40, 40),
            (200, 10, 200),
            (10, 200, 200),
            (120, 120, 240),
        ];
        let cases: Vec<(&str, Vec<(u8, u8, u8)>, u8)> = vec![
            ("copy", tight_copy_pixels(64, 32), 0),
            (
                "palette2",
                tight_palette_pixels(64, 32, &colours[..2]),
                TIGHT_EXPLICIT_FILTER,
            ),
            (
                "palette8",
                tight_palette_pixels(64, 32, &colours),
                TIGHT_EXPLICIT_FILTER,
            ),
            ("fill", vec![(9u8, 8u8, 7u8); 64 * 32], TIGHT_FILL),
        ];

        for (depth, big_endian, red_shift, green_shift, blue_shift) in [
            (24, false, 16, 8, 0), // RGB little-endian, 3-byte TPIXEL
            (24, false, 0, 8, 16), // BGR little-endian, 3-byte TPIXEL
            (24, true, 0, 8, 16),  // BGR big-endian, 3-byte TPIXEL
            (32, true, 16, 8, 0),  // depth 32 forces a 4-byte PIXEL
        ] {
            let pixel = client_pixel_format(depth, big_endian, red_shift, green_shift, blue_shift);
            for (name, pixels, expected_control) in &cases {
                let frame = frame_from_pixels(64, 32, pixels);
                let rect = Rect {
                    x: 0,
                    y: 0,
                    width: 64,
                    height: 32,
                };
                let mut encoder = TightStream::new(TIGHT_DEFAULT_COMPRESSION);
                let mut decoder = TestZlibDecoder::new();
                for round in 0..3 {
                    let mut encoded = Vec::new();
                    let count =
                        encode_tight_rect(&frame, pixel, rect, &mut encoder, None, &mut encoded)
                            .expect("Tight encode");
                    assert_eq!(count, 1);
                    // encode_tight_rect writes a 12-byte rectangle header too;
                    // the decoder consumes only the encoding payload.
                    let (decoded, control) =
                        decode_tight(&mut decoder, &encoded[12..], 64, 32, pixel);
                    assert_eq!(decoded, *pixels, "{name} round {round} decoded wrong");
                    assert_eq!(
                        control, *expected_control,
                        "{name} used an unexpected Tight compression/filter"
                    );
                }
            }
        }
    }

    /// Encode one Tight wire rectangle and return its payload (without the
    /// 12-byte rectangle header).
    fn encode_tight_test_payload(
        frame: &FrameSnapshot,
        pixel: ClientPixelFormat,
        rect: Rect,
        level: u8,
        jpeg_quality: Option<u8>,
    ) -> Vec<u8> {
        let mut stream = TightStream::new(level);
        let mut encoded = Vec::new();
        let count = encode_tight_rect(frame, pixel, rect, &mut stream, jpeg_quality, &mut encoded)
            .expect("Tight encode");
        assert_eq!(count, 1);
        assert_eq!(
            i32::from_be_bytes([encoded[8], encoded[9], encoded[10], encoded[11]]),
            ENCODING_TIGHT
        );
        encoded[12..].to_vec()
    }

    #[test]
    fn tight_quality_and_compression_pseudo_encodings_are_recorded() {
        let mut state = ClientEncodingState::default();
        state.apply(ENCODING_TIGHT);
        state.apply(ENCODING_JPEG_QUALITY_LOW);
        state.apply(ENCODING_COMPRESSION_LEVEL_LOW);
        assert_eq!(state.jpeg_quality, Some(0));
        assert_eq!(state.compression_level, Some(0));

        state.apply(ENCODING_JPEG_QUALITY_HIGH);
        state.apply(ENCODING_COMPRESSION_LEVEL_HIGH);
        assert_eq!(state.jpeg_quality, Some(9));
        assert_eq!(state.compression_level, Some(9));

        // An intermediate level and a later duplicate both update the hint.
        state.apply(-25); // JPEG level 7
        state.apply(-250); // compression level 6
        assert_eq!(state.jpeg_quality, Some(7));
        assert_eq!(state.compression_level, Some(6));

        // Unrelated pseudo-encodings do not disturb the hints.
        state.apply(ENCODING_CURSOR);
        state.apply(-512);
        assert!(state.cursor);
        assert_eq!(state.jpeg_quality, Some(7));
        assert_eq!(state.compression_level, Some(6));
        assert_eq!(
            state.select(DisabledEncodings::default()),
            UpdateEncoding::Tight
        );
    }

    #[test]
    fn tight_gradient_filter_wins_on_a_smooth_ramp_and_decodes() {
        let (width, height) = (64usize, 64usize);
        let expected = tight_gradient_pixels(width, height);
        let frame = frame_from_pixels(width, height, &expected);
        for pixel in [
            client_pixel_format(24, false, 16, 8, 0),
            client_pixel_format(24, false, 0, 8, 16),
            client_pixel_format(32, true, 16, 8, 0),
        ] {
            let payload = encode_tight_test_payload(
                &frame,
                pixel,
                Rect {
                    x: 0,
                    y: 0,
                    width: width as u16,
                    height: height as u16,
                },
                TIGHT_DEFAULT_COMPRESSION,
                None,
            );
            assert_eq!(payload[0] & 0xf0, TIGHT_EXPLICIT_FILTER);
            assert_eq!(payload[1], TIGHT_FILTER_GRADIENT);

            let mut decoder = TestZlibDecoder::new();
            let (decoded, control) = decode_tight(&mut decoder, &payload, width, height, pixel);
            assert_eq!(decoded, expected);
            assert_eq!(control & 0xf0, TIGHT_EXPLICIT_FILTER);
        }
    }

    #[test]
    fn tight_copy_filter_stays_selected_for_noise() {
        let (width, height) = (64usize, 64usize);
        let expected = tight_copy_pixels(width, height);
        let frame = frame_from_pixels(width, height, &expected);
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        let payload = encode_tight_test_payload(
            &frame,
            pixel,
            Rect {
                x: 0,
                y: 0,
                width: width as u16,
                height: height as u16,
            },
            TIGHT_DEFAULT_COMPRESSION,
            None,
        );
        assert_eq!(
            payload[0] & 0xf0,
            0,
            "uncorrelated noise must keep the copy filter"
        );

        let mut decoder = TestZlibDecoder::new();
        let (decoded, _) = decode_tight(&mut decoder, &payload, width, height, pixel);
        assert_eq!(decoded, expected);
    }

    #[test]
    fn tight_jpeg_subencoding_decodes_at_the_requested_quality() {
        let (width, height) = (64usize, 64usize);
        let pixels = tight_copy_pixels(width, height);
        let frame = frame_from_pixels(width, height, &pixels);
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        let rect = Rect {
            x: 0,
            y: 0,
            width: width as u16,
            height: height as u16,
        };

        let mut lengths = Vec::new();
        for level in [0u8, 9u8] {
            let payload = encode_tight_test_payload(
                &frame,
                pixel,
                rect,
                TIGHT_DEFAULT_COMPRESSION,
                Some(level),
            );
            assert_eq!(payload[0] & 0xf0, TIGHT_JPEG);
            let mut position = 1usize;
            let length = decode_tight_compact_length(&payload, &mut position);
            assert!(length <= TIGHT_MAX_RECT_BYTES);
            assert_eq!(
                position + length,
                payload.len(),
                "Tight JPEG compact length must cover the payload exactly"
            );
            let decoded = decode_jpeg(&payload[position..position + length], width, height);
            assert_eq!(decoded.len(), width * height);
            lengths.push(length);
        }
        assert!(
            lengths[1] > lengths[0],
            "JPEG quality level 9 ({}) must be larger than level 0 ({})",
            lengths[1],
            lengths[0]
        );

        // Without an advertised quality level the same high-colour rectangle
        // must stay lossless and must never carry a JPEG control byte.
        let payload =
            encode_tight_test_payload(&frame, pixel, rect, TIGHT_DEFAULT_COMPRESSION, None);
        assert_ne!(payload[0] & 0xf0, TIGHT_JPEG);
        let mut decoder = TestZlibDecoder::new();
        let (decoded, _) = decode_tight(&mut decoder, &payload, width, height, pixel);
        assert_eq!(decoded, pixels);
    }

    #[test]
    fn tight_compression_level_controls_the_zlib_payload_size() {
        let (width, height) = (64usize, 64usize);
        let expected = tight_gradient_pixels(width, height);
        let frame = frame_from_pixels(width, height, &expected);
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        let rect = Rect {
            x: 0,
            y: 0,
            width: width as u16,
            height: height as u16,
        };

        let low = encode_tight_test_payload(&frame, pixel, rect, 0, None);
        let high = encode_tight_test_payload(&frame, pixel, rect, 9, None);
        assert!(
            high.len() < low.len(),
            "level 9 ({}) must be smaller than level 0 ({})",
            high.len(),
            low.len()
        );

        for (level, payload) in [(0u8, low), (9u8, high)] {
            let mut decoder = TestZlibDecoder::new();
            let (decoded, _) = decode_tight(&mut decoder, &payload, width, height, pixel);
            assert_eq!(decoded, expected, "level {level} decoded wrong");
        }
    }

    #[test]
    fn tight_compression_level_change_resets_the_zlib_stream() {
        let (width, height) = (32usize, 32usize);
        let pixels = tight_gradient_pixels(width, height);
        let frame = frame_from_pixels(width, height, &pixels);
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        let rect = Rect {
            x: 0,
            y: 0,
            width: width as u16,
            height: height as u16,
        };

        let mut stream = TightStream::new(6);
        let mut decoder = TestZlibDecoder::new();
        for (level, expect_reset) in [(6u8, false), (9u8, true), (9u8, false)] {
            stream.set_level(level);
            let mut encoded = Vec::new();
            encode_tight_rect(&frame, pixel, rect, &mut stream, None, &mut encoded)
                .expect("Tight encode");
            let payload = &encoded[12..];
            assert_eq!(
                payload[0] & 0x01 != 0,
                expect_reset,
                "level {level} reset bit"
            );
            let (decoded, _) = decode_tight(&mut decoder, payload, width, height, pixel);
            assert_eq!(decoded, pixels, "level {level} decoded wrong after reset");
        }
    }

    #[test]
    fn tight_jpeg_subpath_failure_keeps_lossless_tight() {
        let (width, height) = (64usize, 64usize);
        let expected = tight_copy_pixels(width, height);
        let frame = frame_from_pixels(width, height, &expected);
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        let rect = Rect {
            x: 0,
            y: 0,
            width: width as u16,
            height: height as u16,
        };

        let mut stream = TightStream::new(TIGHT_DEFAULT_COMPRESSION);
        stream.jpeg_disabled = true;
        let mut encoded = Vec::new();
        encode_tight_rect(&frame, pixel, rect, &mut stream, Some(9), &mut encoded)
            .expect("lossless Tight must survive a disabled JPEG sub-path");
        assert_ne!(encoded[12] & 0xf0, TIGHT_JPEG);
        assert!(
            stream.jpeg_disabled,
            "JPEG stays disabled for the connection"
        );

        let mut decoder = TestZlibDecoder::new();
        let (decoded, _) = decode_tight(&mut decoder, &encoded[12..], width, height, pixel);
        assert_eq!(decoded, expected);
    }

    #[test]
    fn zlib_stream_encodes_multiple_rectangles_in_rgb_and_bgr() {
        let (width, height) = (37usize, 21usize);
        let pixels: Vec<(u8, u8, u8)> = (0..width * height)
            .map(|index| {
                (
                    (index % 255) as u8,
                    ((index * 5) % 255) as u8,
                    ((index * 11) % 255) as u8,
                )
            })
            .collect();
        let frame = frame_from_pixels(width, height, &pixels);
        let rect = Rect {
            x: 0,
            y: 0,
            width: width as u16,
            height: height as u16,
        };

        for pixel in [
            client_pixel_format(24, false, 16, 8, 0),
            client_pixel_format(24, false, 0, 8, 16),
            client_pixel_format(32, true, 16, 8, 0),
        ] {
            // One encoder and one decoder for three successive rectangles.
            let mut encoder = ZlibEncoder::new(Vec::new(), Compression::default());
            let mut decoder = TestZlibDecoder::new();
            for _ in 0..3 {
                let data =
                    encode_zlib_rect(&frame, pixel, rect, &mut encoder).expect("ZLib encode");
                let decoded = decode_zlib_rect(&mut decoder, &data, width, height, pixel);
                assert_eq!(decoded, pixels);
            }
        }
    }

    #[test]
    fn compressed_encoding_failure_falls_back_to_raw_and_disables_only_that_encoding() {
        // YUYV is rejected at startup in production, but it gives this unit
        // test a frame whose pixels the encoders cannot read, forcing the
        // per-rectangle error path without a synthetic stream failure.
        let frame = FrameSnapshot {
            generation: 1,
            format: FrameFormat::new(PixelFormat::Yuyv, 4, 2),
            data: Bytes::from(vec![0u8; 16]),
        };
        let pixel = ClientPixelFormat::default();
        let rects = [Rect {
            x: 0,
            y: 0,
            width: 4,
            height: 2,
        }];

        let mut zrle = ZlibEncoder::new(Vec::new(), Compression::default());
        let built = build_update(
            &frame,
            pixel,
            &rects,
            UpdateEncoding::Zrle,
            Some(&mut zrle),
            None,
            None,
            None,
            false,
            None,
            (4, 2),
        );
        assert_eq!(built.failed_encoding, Some(UpdateEncoding::Zrle));
        assert_eq!(first_rect_encoding(&built.bytes), ENCODING_RAW);
        assert_eq!(built.payload_bytes, 4 * 2 * 4);

        let built = build_update(
            &frame,
            pixel,
            &rects,
            UpdateEncoding::Hextile,
            None,
            None,
            None,
            None,
            false,
            None,
            (4, 2),
        );
        assert_eq!(built.failed_encoding, Some(UpdateEncoding::Hextile));
        assert_eq!(first_rect_encoding(&built.bytes), ENCODING_RAW);

        let mut zlib = ZlibEncoder::new(Vec::new(), Compression::default());
        let built = build_update(
            &frame,
            pixel,
            &rects,
            UpdateEncoding::ZLib,
            None,
            Some(&mut zlib),
            None,
            None,
            false,
            None,
            (4, 2),
        );
        assert_eq!(built.failed_encoding, Some(UpdateEncoding::ZLib));
        assert_eq!(first_rect_encoding(&built.bytes), ENCODING_RAW);

        let mut tight = TightStream::new(TIGHT_DEFAULT_COMPRESSION);
        let built = build_update(
            &frame,
            pixel,
            &rects,
            UpdateEncoding::Tight,
            None,
            None,
            Some(&mut tight),
            None,
            false,
            None,
            (4, 2),
        );
        assert_eq!(built.failed_encoding, Some(UpdateEncoding::Tight));
        assert_eq!(first_rect_encoding(&built.bytes), ENCODING_RAW);

        // A selection with no usable stream (the encoder was already disabled)
        // produces Raw without claiming a new failure.
        let built = build_update(
            &frame,
            pixel,
            &rects,
            UpdateEncoding::Zrle,
            None,
            None,
            None,
            None,
            false,
            None,
            (4, 2),
        );
        assert_eq!(built.failed_encoding, None);
        assert_eq!(first_rect_encoding(&built.bytes), ENCODING_RAW);
    }

    #[test]
    fn encoding_selection_skips_only_disabled_encodings() {
        let mut state = ClientEncodingState::default();
        assert_eq!(
            state.select(DisabledEncodings::default()),
            UpdateEncoding::Raw
        );

        state.push(UpdateEncoding::Hextile);
        state.push(UpdateEncoding::ZLib);
        state.push(UpdateEncoding::Tight);
        state.push(UpdateEncoding::Zrle);
        // Duplicates and extra entries do not change or overflow the order.
        state.push(UpdateEncoding::Hextile);
        assert_eq!(state.count, 4);

        let mut disabled = DisabledEncodings::default();
        assert_eq!(state.select(disabled), UpdateEncoding::Hextile);
        disabled.disable(UpdateEncoding::Hextile);
        assert_eq!(state.select(disabled), UpdateEncoding::ZLib);
        assert!(!disabled.is_disabled(UpdateEncoding::Tight));
        disabled.disable(UpdateEncoding::ZLib);
        assert_eq!(state.select(disabled), UpdateEncoding::Tight);
        assert!(!disabled.is_disabled(UpdateEncoding::Zrle));
        disabled.disable(UpdateEncoding::Tight);
        assert_eq!(state.select(disabled), UpdateEncoding::Zrle);
        disabled.disable(UpdateEncoding::Zrle);
        assert_eq!(state.select(disabled), UpdateEncoding::Raw);
    }

    #[tokio::test]
    async fn hextile_frames_decode_over_the_wire_in_rgb_and_bgr() {
        let expected = hextile_pixels();
        let server = start_server(
            Box::new(FixedPixelSource::new(64, 32, expected.clone())),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        for pixel in [
            client_pixel_format(24, false, 16, 8, 0),
            client_pixel_format(24, false, 0, 8, 16),
            client_pixel_format(24, true, 0, 8, 16),
        ] {
            set_pixel_format(&mut client, pixel).await;
            set_encodings(&mut client, &[ENCODING_HEXTILE]).await;
            request_update(&mut client, false, 64, 32).await;
            let rects = read_update(&mut client).await;
            assert_eq!(rects.len(), 1);
            assert_eq!(rects[0].encoding, ENCODING_HEXTILE);
            let (decoded, subencodings) = decode_hextile(&rects[0].data, 64, 32, pixel);
            assert_eq!(decoded, expected);
            assert_all_hextile_variants(&subencodings);
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn zlib_and_zrle_use_independent_persistent_streams() {
        let expected = pattern_pixels(64, 32);
        let server = start_server(
            Box::new(PatternSource::new(64, 32)),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let mut zlib_decoder = TestZlibDecoder::new();
        let mut zrle_decoder = TestZlibDecoder::new();

        for pixel in [
            client_pixel_format(24, false, 16, 8, 0),
            client_pixel_format(24, false, 0, 8, 16),
            client_pixel_format(24, true, 0, 8, 16),
        ] {
            set_pixel_format(&mut client, pixel).await;

            // Two ZLib rectangles on the same persistent zlib stream.
            set_encodings(&mut client, &[ENCODING_ZLIB]).await;
            for _ in 0..2 {
                request_update(&mut client, false, 64, 32).await;
                let rects = read_update(&mut client).await;
                assert_eq!(rects.len(), 1);
                assert_eq!(rects[0].encoding, ENCODING_ZLIB);
                let decoded = decode_zlib_rect(&mut zlib_decoder, &rects[0].data, 64, 32, pixel);
                assert_eq!(decoded, expected);
            }

            // ZRLE has its own stream and must decode independently.
            set_encodings(&mut client, &[ENCODING_ZRLE]).await;
            request_update(&mut client, false, 64, 32).await;
            let rects = read_update(&mut client).await;
            assert_eq!(rects[0].encoding, ENCODING_ZRLE);
            let (decoded, _) = decode_zrle(&mut zrle_decoder, &rects[0].data, 64, 32, pixel);
            assert_eq!(decoded, expected);

            // Returning to ZLib continues the original stream rather than
            // restarting it.
            set_encodings(&mut client, &[ENCODING_ZLIB]).await;
            request_update(&mut client, false, 64, 32).await;
            let rects = read_update(&mut client).await;
            assert_eq!(rects[0].encoding, ENCODING_ZLIB);
            let decoded = decode_zlib_rect(&mut zlib_decoder, &rects[0].data, 64, 32, pixel);
            assert_eq!(decoded, expected);
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn tight_frames_decode_over_the_wire_in_rgb_and_bgr() {
        let colours = [
            (10u8, 20u8, 30u8),
            (200, 30, 40),
            (30, 200, 60),
            (220, 220, 10),
        ];
        let sources = [
            ("copy", tight_copy_pixels(64, 32), 0),
            (
                "palette",
                tight_palette_pixels(64, 32, &colours),
                TIGHT_EXPLICIT_FILTER,
            ),
        ];

        for (name, expected, expected_control) in sources {
            let server = start_server(
                Box::new(FixedPixelSource::new(64, 32, expected.clone())),
                Arc::new(FakeInputSink::new()),
                1,
            )
            .await;
            let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
            handshake_client(&mut client).await;
            let mut decoder = TestZlibDecoder::new();

            for pixel in [
                client_pixel_format(24, false, 16, 8, 0),
                client_pixel_format(24, false, 0, 8, 16),
                client_pixel_format(24, true, 0, 8, 16),
                client_pixel_format(32, false, 16, 8, 0),
            ] {
                set_pixel_format(&mut client, pixel).await;
                set_encodings(&mut client, &[ENCODING_TIGHT]).await;
                // Two updates reuse the one per-client Tight zlib stream.
                for _ in 0..2 {
                    request_update(&mut client, false, 64, 32).await;
                    let rects = read_update_with_pixel(&mut client, pixel).await;
                    assert_eq!(rects.len(), 1);
                    assert_eq!(rects[0].encoding, ENCODING_TIGHT);
                    let (decoded, control) =
                        decode_tight(&mut decoder, &rects[0].data, 64, 32, pixel);
                    assert_eq!(decoded, expected, "{name} decoded wrong over the wire");
                    assert_eq!(control, expected_control, "{name} used the wrong filter");
                }
            }
            server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn tight_and_zlib_use_independent_persistent_streams() {
        let expected = tight_copy_pixels(64, 32);
        let server = start_server(
            Box::new(FixedPixelSource::new(64, 32, expected.clone())),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let mut tight_decoder = TestZlibDecoder::new();
        let mut zlib_decoder = TestZlibDecoder::new();

        for pixel in [
            client_pixel_format(24, false, 16, 8, 0),
            client_pixel_format(24, false, 0, 8, 16),
        ] {
            set_pixel_format(&mut client, pixel).await;
            set_encodings(&mut client, &[ENCODING_TIGHT]).await;
            for _ in 0..2 {
                request_update(&mut client, false, 64, 32).await;
                let rects = read_update_with_pixel(&mut client, pixel).await;
                assert_eq!(rects[0].encoding, ENCODING_TIGHT);
                let (decoded, _) = decode_tight(&mut tight_decoder, &rects[0].data, 64, 32, pixel);
                assert_eq!(decoded, expected);
            }

            // ZLib has its own stream and must not disturb Tight's history.
            set_encodings(&mut client, &[ENCODING_ZLIB]).await;
            request_update(&mut client, false, 64, 32).await;
            let rects = read_update_with_pixel(&mut client, pixel).await;
            assert_eq!(rects[0].encoding, ENCODING_ZLIB);
            let decoded = decode_zlib_rect(&mut zlib_decoder, &rects[0].data, 64, 32, pixel);
            assert_eq!(decoded, expected);

            // Returning to Tight continues the original Tight stream.
            set_encodings(&mut client, &[ENCODING_TIGHT]).await;
            request_update(&mut client, false, 64, 32).await;
            let rects = read_update_with_pixel(&mut client, pixel).await;
            assert_eq!(rects[0].encoding, ENCODING_TIGHT);
            let (decoded, _) = decode_tight(&mut tight_decoder, &rects[0].data, 64, 32, pixel);
            assert_eq!(decoded, expected);
        }
        server.shutdown().await;
    }

    #[tokio::test]
    async fn tight_jpeg_frames_decode_over_the_wire_and_follow_quality_hints() {
        let (width, height) = (64usize, 32usize);
        let expected = tight_copy_pixels(width, height);
        let server = start_server(
            Box::new(FixedPixelSource::new(
                width as u32,
                height as u32,
                expected.clone(),
            )),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        set_pixel_format(&mut client, pixel).await;

        // Tight without a JPEG quality hint must stay lossless.
        set_encodings(&mut client, &[ENCODING_TIGHT]).await;
        request_update(&mut client, false, width as u16, height as u16).await;
        let rects = read_update(&mut client).await;
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_TIGHT);
        assert_ne!(rects[0].data[0] & 0xf0, TIGHT_JPEG);
        let mut decoder = TestZlibDecoder::new();
        let (decoded, _) = decode_tight(&mut decoder, &rects[0].data, width, height, pixel);
        assert_eq!(decoded, expected);

        let jpeg_length = |rect: &WireRect| -> usize {
            assert_eq!(rect.encoding, ENCODING_TIGHT);
            assert_eq!(rect.data[0] & 0xf0, TIGHT_JPEG);
            let mut position = 1usize;
            let length = decode_tight_compact_length(&rect.data, &mut position);
            assert!(length <= TIGHT_MAX_RECT_BYTES);
            assert_eq!(position + length, rect.data.len());
            decode_jpeg(&rect.data[position..position + length], width, height);
            length
        };

        set_encodings(&mut client, &[ENCODING_TIGHT, ENCODING_JPEG_QUALITY_HIGH]).await;
        request_update(&mut client, false, width as u16, height as u16).await;
        let rects = read_update(&mut client).await;
        assert_eq!(rects.len(), 1);
        let high_length = jpeg_length(&rects[0]);

        set_encodings(&mut client, &[ENCODING_TIGHT, ENCODING_JPEG_QUALITY_LOW]).await;
        request_update(&mut client, false, width as u16, height as u16).await;
        let rects = read_update(&mut client).await;
        assert_eq!(rects.len(), 1);
        let low_length = jpeg_length(&rects[0]);

        assert!(
            high_length > low_length,
            "quality -23 ({high_length}) must produce more JPEG bytes than -32 ({low_length})"
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn server_resize_sends_extended_desktop_size_and_resized_pixels_for_all_encodings() {
        let first = (10u8, 20u8, 30u8);
        let second = (200u8, 100u8, 50u8);
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        let cases = [
            (ENCODING_RAW, "raw"),
            (ENCODING_ZRLE, "zrle"),
            (ENCODING_HEXTILE, "hextile"),
            (ENCODING_ZLIB, "zlib"),
            (ENCODING_TIGHT, "tight"),
        ];

        for (encoding, name) in cases {
            let (frames_tx, frames_rx) = mpsc::unbounded_channel();
            let server = start_server(
                Box::new(ChannelSource::new(FrameFormat::rgb24(4, 2), frames_rx)),
                Arc::new(FakeInputSink::new()),
                1,
            )
            .await;
            let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
            let (width, height, _) = handshake_client(&mut client).await;
            assert_eq!((width, height), (4, 2));

            frames_tx.send(solid_rgb_frame(4, 2, first)).unwrap();
            wait_for(|| server.stats().frames_published >= 1).await;
            set_pixel_format(&mut client, pixel).await;
            set_encodings(&mut client, &[encoding, ENCODING_EXTENDED_DESKTOP_SIZE]).await;

            // Serve the ServerInit-sized frame first.
            request_update(&mut client, false, 4, 2).await;
            let rects = read_update_with_pixel(&mut client, pixel).await;
            assert_eq!(rects.len(), 1, "{name}: initial update");
            assert_eq!(rects[0].encoding, encoding, "{name}: initial encoding");
            assert_eq!((rects[0].width, rects[0].height), (4, 2));
            let mut zrle_decoder = TestZlibDecoder::new();
            let mut zlib_decoder = TestZlibDecoder::new();
            let mut tight_decoder = TestZlibDecoder::new();
            assert_eq!(
                decode_wire_rect(
                    &rects[0],
                    pixel,
                    &mut zrle_decoder,
                    &mut zlib_decoder,
                    &mut tight_decoder,
                ),
                vec![first; 4 * 2],
                "{name}: initial pixels"
            );

            // The capture source resizes. The next update must announce it
            // before the new pixels, even though the client asked for the old
            // 4x2 rectangle incrementally.
            frames_tx.send(solid_rgb_frame(8, 4, second)).unwrap();
            wait_for(|| server.stats().frames_published >= 2).await;
            request_update(&mut client, true, 4, 2).await;
            let rects = read_update_with_pixel(&mut client, pixel).await;
            assert_eq!(rects.len(), 2, "{name}: resize update");
            let resize = &rects[0];
            assert_eq!(
                resize.encoding, ENCODING_EXTENDED_DESKTOP_SIZE,
                "{name}: ExtendedDesktopSize must precede pixels"
            );
            assert_eq!(
                (resize.x, resize.y),
                (
                    DESKTOP_SIZE_REASON_SERVER,
                    DesktopSizeResult::Success.code()
                ),
                "{name}: server-initiated success reason/result"
            );
            assert_eq!((resize.width, resize.height), (8, 4));
            assert_eq!(resize.data[0], 1, "{name}: one screen");
            assert_eq!(
                u16::from_be_bytes([resize.data[9], resize.data[10]]),
                8,
                "{name}: screen width"
            );
            assert_eq!(
                u16::from_be_bytes([resize.data[11], resize.data[12]]),
                4,
                "{name}: screen height"
            );

            let pixels = &rects[1];
            assert_eq!(pixels.encoding, encoding, "{name}: resized encoding");
            assert_eq!((pixels.x, pixels.y), (0, 0));
            assert_eq!((pixels.width, pixels.height), (8, 4));
            assert_eq!(
                decode_wire_rect(
                    pixels,
                    pixel,
                    &mut zrle_decoder,
                    &mut zlib_decoder,
                    &mut tight_decoder,
                ),
                vec![second; 8 * 4],
                "{name}: resized pixels through the same encoder stream"
            );

            assert!(server.stats().desktop_resize_sent >= 1);
            assert!(server.stats().framebuffer_resizes >= 1);
            server.shutdown().await;
        }
    }

    #[tokio::test]
    async fn first_frame_geometry_change_after_server_init_sends_resize() {
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let server = start_server(
            Box::new(ChannelSource::new(FrameFormat::rgb24(4, 2), frames_rx)),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        let (width, height, _) = handshake_client(&mut client).await;
        assert_eq!(
            (width, height),
            (4, 2),
            "ServerInit uses the source's declared geometry before any frame"
        );

        let colour = (40u8, 50u8, 60u8);
        frames_tx.send(solid_rgb_frame(8, 4, colour)).unwrap();
        wait_for(|| server.stats().frames_published >= 1).await;
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        set_pixel_format(&mut client, pixel).await;
        set_encodings(&mut client, &[ENCODING_RAW, ENCODING_EXTENDED_DESKTOP_SIZE]).await;
        request_update(&mut client, false, 4, 2).await;

        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 2);
        assert_eq!(rects[0].encoding, ENCODING_EXTENDED_DESKTOP_SIZE);
        assert_eq!((rects[0].x, rects[0].y), (0, 0));
        assert_eq!((rects[0].width, rects[0].height), (8, 4));
        assert_eq!(rects[1].encoding, ENCODING_RAW);
        assert_eq!((rects[1].width, rects[1].height), (8, 4));
        assert_eq!(
            decode_raw_pixels(&rects[1].data, 8, 4, pixel),
            vec![colour; 8 * 4]
        );
        server.shutdown().await;
    }

    #[tokio::test]
    async fn resize_is_not_announced_to_a_client_without_extended_desktop_size() {
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let server = start_server(
            Box::new(ChannelSource::new(FrameFormat::rgb24(4, 2), frames_rx)),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        set_pixel_format(&mut client, pixel).await;
        // No -308: the client cannot be told that the framebuffer changed.
        set_encodings(&mut client, &[ENCODING_RAW]).await;

        frames_tx.send(solid_rgb_frame(4, 2, (1, 2, 3))).unwrap();
        wait_for(|| server.stats().frames_published >= 1).await;
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 1);
        assert_eq!((rects[0].width, rects[0].height), (4, 2));

        // Growing beyond the client's ServerInit size is clipped, not sent as
        // out-of-bounds pixels or a pseudo-encoding the client never requested.
        frames_tx.send(solid_rgb_frame(8, 4, (4, 5, 6))).unwrap();
        wait_for(|| server.stats().frames_published >= 2).await;
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(
            rects.len(),
            1,
            "no ExtendedDesktopSize for a -308-less client"
        );
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        assert_eq!((rects[0].width, rects[0].height), (4, 2));

        // Shrinking is clamped to the new frame as well.
        frames_tx.send(solid_rgb_frame(2, 1, (7, 8, 9))).unwrap();
        wait_for(|| server.stats().frames_published >= 3).await;
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 1);
        assert_eq!((rects[0].width, rects[0].height), (2, 1));
        assert!(server.stats().framebuffer_resizes >= 2);
        assert_eq!(server.stats().desktop_resize_sent, 0);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_resize_frames_are_dropped_without_moving_the_framebuffer() {
        let (frames_tx, frames_rx) = mpsc::unbounded_channel();
        let server = start_server(
            Box::new(ChannelSource::new(FrameFormat::rgb24(4, 2), frames_rx)),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        set_pixel_format(&mut client, pixel).await;
        set_encodings(&mut client, &[ENCODING_RAW, ENCODING_EXTENDED_DESKTOP_SIZE]).await;

        frames_tx.send(solid_rgb_frame(4, 2, (1, 2, 3))).unwrap();
        wait_for(|| server.stats().frames_published >= 1).await;
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 1);

        // Width exceeds the RFB u16 geometry range, so the pump must drop it
        // rather than publish a framebuffer it cannot describe in ServerInit or
        // a rectangle header.
        frames_tx
            .send(solid_rgb_frame(u16::MAX as usize + 1, 1, (9, 9, 9)))
            .unwrap();
        wait_for(|| server.stats().source_errors >= 1).await;
        assert_eq!(server.stats().frames_published, 1);
        assert_eq!(server.stats().framebuffer_resizes, 0);

        request_update(&mut client, false, 4, 2).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 1, "the old 4x2 framebuffer stays intact");
        assert_eq!((rects[0].width, rects[0].height), (4, 2));
        server.shutdown().await;
    }

    #[test]
    fn set_desktop_size_bounds_are_typed() {
        assert_eq!(
            validate_set_desktop_size(0, 4, 1),
            DesktopSizeResult::InvalidScreenLayout
        );
        assert_eq!(
            validate_set_desktop_size(4, 0, 1),
            DesktopSizeResult::InvalidScreenLayout
        );
        assert_eq!(
            validate_set_desktop_size(4, 2, 0),
            DesktopSizeResult::InvalidScreenLayout
        );
        assert_eq!(
            validate_set_desktop_size(4, 2, MAX_DESKTOP_SCREENS as u16 + 1),
            DesktopSizeResult::InvalidScreenLayout
        );
        assert_eq!(
            validate_set_desktop_size(u16::MAX, u16::MAX, 1),
            DesktopSizeResult::OutOfResources
        );
        // Exactly the 16-megapixel bound is structurally valid but still
        // prohibited: the capture source has no resize API.
        assert_eq!(
            validate_set_desktop_size(4096, 4096, 1),
            DesktopSizeResult::Prohibited
        );
        assert_eq!(
            validate_set_desktop_size(8, 4, 1),
            DesktopSizeResult::Prohibited
        );
    }

    #[tokio::test]
    async fn set_desktop_size_is_parsed_and_denied_without_fake_success() {
        let server = start_server(
            Box::new(TestSource::new(4, 2).solid(10, 20, 30)),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        set_pixel_format(&mut client, pixel).await;
        set_encodings(&mut client, &[ENCODING_RAW, ENCODING_EXTENDED_DESKTOP_SIZE]).await;

        // A structurally valid request is still denied: the pixel source is
        // read-only from VNC's perspective.
        write_set_desktop_size(&mut client, 8, 4, 1).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 1);
        let denial = &rects[0];
        assert_eq!(denial.encoding, ENCODING_EXTENDED_DESKTOP_SIZE);
        assert_eq!(
            (denial.x, denial.y),
            (
                DESKTOP_SIZE_REASON_CLIENT,
                DesktopSizeResult::Prohibited.code()
            ),
            "SetDesktopSize must be reported as a client-reason prohibition"
        );
        assert_eq!(
            (denial.width, denial.height),
            (4, 2),
            "a denial echoes the current geometry, never the requested one"
        );
        assert_eq!(denial.data[0], 1);
        assert_eq!(u16::from_be_bytes([denial.data[9], denial.data[10]]), 4);
        assert_eq!(u16::from_be_bytes([denial.data[11], denial.data[12]]), 2);

        // Zero-sized and oversized layouts get their specific result codes.
        write_set_desktop_size(&mut client, 0, 0, 1).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects[0].y, DesktopSizeResult::InvalidScreenLayout.code());
        write_set_desktop_size(&mut client, u16::MAX, u16::MAX, 1).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects[0].y, DesktopSizeResult::OutOfResources.code());

        // The framebuffer itself is untouched and still serves 4x2 pixels.
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        assert_eq!((rects[0].width, rects[0].height), (4, 2));
        assert_eq!(
            decode_raw_pixels(&rects[0].data, 4, 2, pixel),
            vec![(10, 20, 30); 4 * 2]
        );

        let stats = server.stats();
        assert_eq!(stats.desktop_resize_requests, 3);
        assert_eq!(stats.desktop_resize_denied, 3);
        assert_eq!(stats.desktop_resize_sent, 0);
        assert_eq!(stats.framebuffer_resizes, 0);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn set_desktop_size_without_extended_desktop_size_gets_no_wire_reply() {
        let server = start_server(
            Box::new(TestSource::new(4, 2).solid(10, 20, 30)),
            Arc::new(FakeInputSink::new()),
            1,
        )
        .await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;
        let pixel = client_pixel_format(24, false, 16, 8, 0);
        set_pixel_format(&mut client, pixel).await;
        set_encodings(&mut client, &[ENCODING_RAW]).await;

        write_set_desktop_size(&mut client, 8, 4, 1).await;

        // The next message must be the requested framebuffer update, not a
        // denial pseudo-rectangle the client never negotiated.
        request_update(&mut client, false, 4, 2).await;
        let rects = read_update_with_pixel(&mut client, pixel).await;
        assert_eq!(rects.len(), 1);
        assert_eq!(rects[0].encoding, ENCODING_RAW);
        assert_eq!((rects[0].width, rects[0].height), (4, 2));
        assert_eq!(server.stats().desktop_resize_requests, 1);
        assert_eq!(server.stats().desktop_resize_denied, 1);
        assert_eq!(server.stats().desktop_resize_sent, 0);
        server.shutdown().await;
    }

    #[tokio::test]
    async fn oversized_set_encodings_is_rejected_typed() {
        let sink: SharedSink = Arc::new(FakeInputSink::new());
        let server = start_server(Box::new(TestSource::new(2, 2)), sink, 1).await;
        let mut client = TcpStream::connect(server.bound_addr()).await.unwrap();
        handshake_client(&mut client).await;

        let mut header = vec![2u8, 0];
        header.extend_from_slice(&((MAX_ENCODINGS + 1) as u16).to_be_bytes());
        write_all(&mut client, &header).await;
        assert_connection_closed(&mut client).await;
        wait_for(|| server.stats().protocol_errors >= 1).await;
        server.shutdown().await;
    }
}
