// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded, read-only-by-default NBD server.
//!
//! The server speaks the fixed-newstyle NBD handshake over a TCP or Unix
//! stream, lists exports, negotiates `EXPORT_NAME`/`INFO`/`GO`, and serves
//! `READ`, `WRITE`, `FLUSH`, `TRIM`, and `DISC`. It never opens a block device:
//! exports are backed by a regular file or an in-memory image
//! ([`NbdMemoryImage`]), and ISO/FAT32 media stay read-only.
//!
//! Every read/write is bounds-checked against both the export size and
//! [`NbdServerConfig::max_request_bytes`]. Connections are bounded by
//! [`NbdServerConfig::max_clients`]. The listener, stream, and clock are
//! injectable so tests can run a session over an in-memory duplex without a
//! socket (`serve_session` with [`NbdTransport`]/[`ManualClock`]).
//!
//! ```no_run
//! use ozma_usb::nbd_server::{
//!     NbdExportCatalog, NbdExportSource, NbdServer, NbdServerConfig,
//! };
//!
//! # fn main() -> Result<(), ozma_usb::UsbError> {
//! let export = NbdExportSource::memory("media", vec![0u8; 4096], true)?;
//! let catalog = NbdExportCatalog::new(vec![export])?;
//! let server = NbdServer::bind_tcp(
//!     std::net::SocketAddr::from(([127, 0, 0, 1], 0)),
//!     catalog,
//!     NbdServerConfig::default(),
//! )?;
//! let handle = server.start()?;
//! println!("nbd://{}", handle.local_addr().unwrap());
//! handle.stop();
//! # Ok(())
//! # }
//! ```

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::net::{Shutdown, SocketAddr, TcpListener, TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::fat32::Fat32Image;
use crate::media::MassStorageImage;
use crate::nbd::{
    NbdCommand, NbdExport, NbdReply, NbdRequestHeader, NBD_FLAG_C_FIXED_NEWSTYLE,
    NBD_FLAG_C_NO_ZEROES, NBD_FLAG_FIXED_NEWSTYLE, NBD_FLAG_HAS_FLAGS, NBD_FLAG_NO_ZEROES,
    NBD_FLAG_READ_ONLY, NBD_FLAG_SEND_FLUSH, NBD_FLAG_SEND_TRIM, NBD_INFO_BLOCK_SIZE,
    NBD_INFO_DESCRIPTION, NBD_INFO_EXPORT, NBD_INFO_NAME, NBD_MAGIC, NBD_MAX_REQUEST_BYTES,
    NBD_NEWSTYLE_MAGIC, NBD_OPT_ABORT, NBD_OPT_EXPORT_NAME, NBD_OPT_GO, NBD_OPT_INFO, NBD_OPT_LIST,
    NBD_OPT_STARTTLS, NBD_REPLY_HEADER_LEN, NBD_REP_ACK, NBD_REP_ERR_INVALID, NBD_REP_ERR_TOO_BIG,
    NBD_REP_ERR_UNKNOWN, NBD_REP_ERR_UNSUP, NBD_REP_INFO, NBD_REP_MAGIC, NBD_REP_SERVER,
    NBD_REQUEST_HEADER_LEN,
};

const EPERM: u32 = 1;
const EIO: u32 = 5;
const EINVAL: u32 = 22;
const EOPNOTSUPP: u32 = 95;

const POLL_INTERVAL: Duration = Duration::from_millis(50);
const MAX_OPTION_BYTES: u32 = 1024 * 1024;
const ZERO_TAIL: [u8; 124] = [0u8; 124];

/// A clock used for handshake and idle deadlines.
///
/// Tests inject [`ManualClock`]; the server defaults to [`SystemClock`].
pub trait NbdClock: Send + Sync {
    /// Milliseconds since the Unix epoch.
    fn now_millis(&self) -> u64;
}

/// Wall-clock implementation.
#[derive(Clone, Copy, Debug, Default)]
pub struct SystemClock;

impl NbdClock for SystemClock {
    fn now_millis(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|duration| duration.as_millis() as u64)
            .unwrap_or(0)
    }
}

/// Deterministic clock for tests.
#[derive(Debug, Default)]
pub struct ManualClock {
    millis: AtomicU64,
}

impl ManualClock {
    /// Start at `millis`.
    pub fn new(millis: u64) -> Self {
        Self {
            millis: AtomicU64::new(millis),
        }
    }

    /// Advance the clock.
    pub fn advance(&self, millis: u64) {
        self.millis.fetch_add(millis, Ordering::SeqCst);
    }

    /// Set the clock.
    pub fn set(&self, millis: u64) {
        self.millis.store(millis, Ordering::SeqCst);
    }
}

impl NbdClock for ManualClock {
    fn now_millis(&self) -> u64 {
        self.millis.load(Ordering::SeqCst)
    }
}

/// A bounded NBD backing store.
///
/// Implementations must be safe to share across client threads. The export
/// size is fixed at construction; the server checks every range before calling
/// the backend.
pub trait NbdBackend: Send + Sync {
    /// Export length in bytes.
    fn len(&self) -> u64;

    /// Whether the backend rejects writes.
    fn is_read_only(&self) -> bool;

    /// Whether this backend is empty (never for a valid export).
    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read exactly `buffer.len()` bytes at `offset`.
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), UsbError>;

    /// Write `data` at `offset`.
    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), UsbError> {
        let _ = (offset, data);
        Err(UsbError::ReadOnlyMedia(
            "the NBD backend does not support writes".to_owned(),
        ))
    }

    /// Flush pending writes.
    fn flush(&self) -> Result<(), UsbError> {
        Ok(())
    }

    /// Discard a range (best-effort; the default is a successful no-op).
    fn trim(&self, offset: u64, length: u64) -> Result<(), UsbError> {
        let _ = (offset, length);
        Ok(())
    }
}

/// An in-memory NBD image.
#[derive(Debug)]
pub struct NbdMemoryImage {
    bytes: std::sync::RwLock<Vec<u8>>,
    read_only: bool,
}

impl NbdMemoryImage {
    /// Wrap an existing byte image.
    pub fn new(bytes: impl Into<Vec<u8>>, read_only: bool) -> Result<Self, UsbError> {
        let bytes = bytes.into();
        if bytes.is_empty() {
            return Err(invalid_nbd_image("NBD image is empty"));
        }
        Ok(Self {
            bytes: std::sync::RwLock::new(bytes),
            read_only,
        })
    }

    /// A read-only clone of the current bytes (tests/diagnostics).
    pub fn snapshot(&self) -> Vec<u8> {
        self.bytes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }
}

impl NbdBackend for NbdMemoryImage {
    fn len(&self) -> u64 {
        self.bytes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .len() as u64
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), UsbError> {
        let bytes = self
            .bytes
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = usize::try_from(offset).map_err(|_| range_error())?;
        let end = start.checked_add(buffer.len()).ok_or_else(range_error)?;
        let slice = bytes.get(start..end).ok_or_else(range_error)?;
        buffer.copy_from_slice(slice);
        Ok(())
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), UsbError> {
        if self.read_only {
            return Err(UsbError::ReadOnlyMedia(
                "the NBD image is read-only".to_owned(),
            ));
        }
        let mut bytes = self
            .bytes
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let start = usize::try_from(offset).map_err(|_| range_error())?;
        let end = start.checked_add(data.len()).ok_or_else(range_error)?;
        let slice = bytes.get_mut(start..end).ok_or_else(range_error)?;
        slice.copy_from_slice(data);
        Ok(())
    }
}

/// A regular-file NBD image. Never opens a block device.
#[derive(Debug)]
pub struct NbdFileImage {
    file: Mutex<File>,
    length: u64,
    read_only: bool,
}

impl NbdFileImage {
    /// Open an existing regular file read-only or read-write.
    pub fn open(path: impl AsRef<Path>, read_only: bool) -> Result<Self, UsbError> {
        let path = path.as_ref();
        let metadata = fs::metadata(path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                UsbError::MediaNotFound(path.to_path_buf())
            } else {
                crate::error::io_error("stat NBD image", path.to_path_buf(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(path.to_path_buf()));
        }
        if metadata.len() == 0 {
            return Err(invalid_nbd_image(format!(
                "NBD image {} is empty",
                path.display()
            )));
        }
        let file = OpenOptions::new()
            .read(true)
            .write(!read_only)
            .create(false)
            .open(path)
            .map_err(|source| {
                crate::error::io_error("open NBD image", path.to_path_buf(), source)
            })?;
        Ok(Self {
            file: Mutex::new(file),
            length: metadata.len(),
            read_only,
        })
    }
}

impl NbdBackend for NbdFileImage {
    fn len(&self) -> u64 {
        self.length
    }

    fn is_read_only(&self) -> bool {
        self.read_only
    }

    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> Result<(), UsbError> {
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.seek(SeekFrom::Start(offset)).map_err(|source| {
            crate::error::io_error("seek NBD image", "<nbd-file>".into(), source)
        })?;
        file.read_exact(buffer)
            .map_err(|source| crate::error::io_error("read NBD image", "<nbd-file>".into(), source))
    }

    fn write_at(&self, offset: u64, data: &[u8]) -> Result<(), UsbError> {
        if self.read_only {
            return Err(UsbError::ReadOnlyMedia(
                "the NBD file is read-only".to_owned(),
            ));
        }
        let mut file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.seek(SeekFrom::Start(offset)).map_err(|source| {
            crate::error::io_error("seek NBD image", "<nbd-file>".into(), source)
        })?;
        file.write_all(data).map_err(|source| {
            crate::error::io_error("write NBD image", "<nbd-file>".into(), source)
        })
    }

    fn flush(&self) -> Result<(), UsbError> {
        if self.read_only {
            return Ok(());
        }
        let file = self
            .file
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        file.sync_data().map_err(|source| {
            crate::error::io_error("flush NBD image", "<nbd-file>".into(), source)
        })
    }
}

/// One export plus its backing image.
#[derive(Clone)]
pub struct NbdExportSource {
    /// Validated export metadata.
    pub export: NbdExport,
    /// Backing store.
    pub backend: Arc<dyn NbdBackend>,
}

impl NbdExportSource {
    /// An export over an in-memory image.
    pub fn memory(
        name: impl Into<String>,
        bytes: impl Into<Vec<u8>>,
        read_only: bool,
    ) -> Result<Self, UsbError> {
        let image = NbdMemoryImage::new(bytes, read_only)?;
        Self::from_backend(name, Arc::new(image), read_only)
    }

    /// An export over an existing regular file.
    pub fn file(
        name: impl Into<String>,
        path: impl AsRef<Path>,
        read_only: bool,
    ) -> Result<Self, UsbError> {
        let path = path.as_ref();
        let image = NbdFileImage::open(path, read_only)?;
        let mut export = export_for(name, image.len(), read_only);
        export.description = Some(path.display().to_string());
        Self::from_backend_with_export(export, Arc::new(image))
    }

    /// A read-only export of a built FAT32 image.
    pub fn fat32(name: impl Into<String>, image: &Fat32Image) -> Result<Self, UsbError> {
        Self::memory(name, image.as_bytes().to_vec(), true)
    }

    /// Build an export from a [`MassStorageImage`].
    ///
    /// ISO and FAT32 formats are probed read-only and forced read-only.
    /// `MediaFormat::Nbd`/`Raw` files may be writable only when the media model
    /// says so; no block device is ever opened.
    pub fn from_media(name: impl Into<String>, media: &MassStorageImage) -> Result<Self, UsbError> {
        media.validate()?;
        let metadata = fs::metadata(&media.path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                UsbError::MediaNotFound(media.path.clone())
            } else {
                crate::error::io_error("stat NBD media", media.path.clone(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(media.path.clone()));
        }
        if metadata.len() == 0 {
            return Err(invalid_nbd_image(format!(
                "media image {} is empty",
                media.path.display()
            )));
        }
        match media.format {
            crate::media::MediaFormat::Iso9660 => {
                crate::iso::validate_iso_image(&media.path)?;
            }
            crate::media::MediaFormat::Fat32 => {
                crate::fat32::validate_fat32_boot_sector(&media.path)?;
            }
            crate::media::MediaFormat::Raw | crate::media::MediaFormat::Nbd => {}
        }
        let read_only = media.read_only || media.format.is_read_only() || media.mode.is_cdrom();
        Self::file(name, &media.path, read_only)
    }

    /// Wrap a custom backend.
    pub fn from_backend(
        name: impl Into<String>,
        backend: Arc<dyn NbdBackend>,
        read_only: bool,
    ) -> Result<Self, UsbError> {
        let export = export_for(name, backend.len(), read_only);
        Self::from_backend_with_export(export, backend)
    }

    /// Wrap a custom backend with explicit export metadata.
    pub fn from_backend_with_export(
        export: NbdExport,
        backend: Arc<dyn NbdBackend>,
    ) -> Result<Self, UsbError> {
        export.validate()?;
        if backend.len() != export.size_bytes {
            return Err(invalid_nbd_image(format!(
                "export size {} does not match backend size {}",
                export.size_bytes,
                backend.len()
            )));
        }
        Ok(Self { export, backend })
    }

    /// Set the export description.
    pub fn with_description(mut self, description: impl Into<String>) -> Self {
        self.export.description = Some(description.into());
        self
    }

    /// Transmission flags advertised for this export.
    pub fn transmission_flags(&self) -> u16 {
        let mut flags = (NBD_FLAG_HAS_FLAGS | NBD_FLAG_SEND_FLUSH) as u16;
        if self.is_read_only() {
            flags |= NBD_FLAG_READ_ONLY as u16;
        } else {
            flags |= NBD_FLAG_SEND_TRIM as u16;
        }
        flags
    }

    /// Whether writes are rejected.
    pub fn is_read_only(&self) -> bool {
        self.export.read_only || self.backend.is_read_only()
    }
}

impl std::fmt::Debug for NbdExportSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("NbdExportSource")
            .field("export", &self.export)
            .field("read_only", &self.is_read_only())
            .finish()
    }
}

fn export_for(name: impl Into<String>, size_bytes: u64, read_only: bool) -> NbdExport {
    let mut export = if read_only {
        NbdExport::read_only(name, size_bytes)
    } else {
        NbdExport::writable(name, size_bytes)
    };
    export.block_size = 4096;
    export
}

/// The set of exports a server offers. Names are unique.
#[derive(Clone, Debug)]
pub struct NbdExportCatalog {
    entries: Vec<NbdExportSource>,
    default_name: Option<String>,
}

impl NbdExportCatalog {
    /// Validate and build a catalog.
    pub fn new(entries: Vec<NbdExportSource>) -> Result<Self, UsbError> {
        if entries.is_empty() {
            return Err(invalid_nbd_image(
                "an NBD catalog needs at least one export",
            ));
        }
        if entries.len() > 256 {
            return Err(invalid_nbd_image("at most 256 NBD exports are supported"));
        }
        let mut seen = std::collections::HashSet::new();
        for entry in &entries {
            entry.export.validate()?;
            if entry.export.size_bytes != entry.backend.len() {
                return Err(invalid_nbd_image(format!(
                    "export {:?} size does not match its backend",
                    entry.export.name
                )));
            }
            if !seen.insert(entry.export.name.clone()) {
                return Err(invalid_nbd_image(format!(
                    "duplicate NBD export name {:?}",
                    entry.export.name
                )));
            }
        }
        Ok(Self {
            entries,
            default_name: None,
        })
    }

    /// Mark the export used when a client negotiates the empty export name.
    pub fn with_default(mut self, name: impl Into<String>) -> Result<Self, UsbError> {
        let name = name.into();
        if !self.entries.iter().any(|entry| entry.export.name == name) {
            return Err(invalid_nbd_image(format!(
                "default NBD export {name:?} is not in the catalog"
            )));
        }
        self.default_name = Some(name);
        Ok(self)
    }

    /// Exports in catalog order.
    pub fn entries(&self) -> &[NbdExportSource] {
        &self.entries
    }

    /// Resolve an export name. An empty name selects the configured default or
    /// the sole export.
    pub fn find(&self, name: &str) -> Option<NbdExportSource> {
        if name.is_empty() {
            if let Some(default) = &self.default_name {
                return self
                    .entries
                    .iter()
                    .find(|entry| &entry.export.name == default)
                    .cloned();
            }
            if self.entries.len() == 1 {
                return self.entries.first().cloned();
            }
            return None;
        }
        self.entries
            .iter()
            .find(|entry| entry.export.name == name)
            .cloned()
    }

    /// Number of exports.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Whether the catalog is empty (never for a valid catalog).
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

/// Bounds for one NBD server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct NbdServerConfig {
    /// Largest read/write payload accepted (bounded by the protocol maximum).
    pub max_request_bytes: u32,
    /// Maximum simultaneous client sessions.
    pub max_clients: usize,
    /// Complete-handshake deadline.
    pub handshake_timeout_ms: u64,
    /// Optional idle deadline between requests.
    pub idle_timeout_ms: Option<u64>,
    /// Largest option payload accepted.
    pub option_max_bytes: u32,
}

impl Default for NbdServerConfig {
    fn default() -> Self {
        Self {
            max_request_bytes: 1024 * 1024,
            max_clients: 8,
            handshake_timeout_ms: 10_000,
            idle_timeout_ms: None,
            option_max_bytes: 64 * 1024,
        }
    }
}

impl NbdServerConfig {
    /// Validate bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.max_request_bytes == 0 || self.max_request_bytes > NBD_MAX_REQUEST_BYTES {
            return Err(invalid_nbd_image(format!(
                "max_request_bytes must be 1..={NBD_MAX_REQUEST_BYTES}"
            )));
        }
        if self.max_clients == 0 || self.max_clients > 1024 {
            return Err(invalid_nbd_image("max_clients must be in 1..=1024"));
        }
        if self.handshake_timeout_ms == 0 {
            return Err(invalid_nbd_image(
                "handshake_timeout_ms must be greater than zero",
            ));
        }
        if matches!(self.idle_timeout_ms, Some(0)) {
            return Err(invalid_nbd_image(
                "idle_timeout_ms must be greater than zero when set",
            ));
        }
        if self.option_max_bytes == 0 || self.option_max_bytes > MAX_OPTION_BYTES {
            return Err(invalid_nbd_image(format!(
                "option_max_bytes must be 1..={MAX_OPTION_BYTES}"
            )));
        }
        Ok(())
    }
}

/// Cumulative server counters.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NbdServerStats {
    /// Accepted connections.
    pub connections_accepted: u64,
    /// Connections refused because `max_clients` was reached.
    pub connections_rejected: u64,
    /// Completed or failed sessions.
    pub sessions: u64,
    /// READ requests.
    pub read_requests: u64,
    /// WRITE requests (including rejected read-only writes).
    pub write_requests: u64,
    /// FLUSH requests.
    pub flush_requests: u64,
    /// TRIM requests.
    pub trim_requests: u64,
    /// DISC requests.
    pub disconnect_requests: u64,
    /// Bytes returned to clients.
    pub bytes_read: u64,
    /// Bytes accepted from clients.
    pub bytes_written: u64,
    /// Malformed frames, unknown options, and I/O failures.
    pub protocol_errors: u64,
}

#[derive(Debug, Default)]
struct Counters {
    accepted: AtomicU64,
    rejected: AtomicU64,
    sessions: AtomicU64,
    reads: AtomicU64,
    writes: AtomicU64,
    flushes: AtomicU64,
    trims: AtomicU64,
    disconnects: AtomicU64,
    bytes_read: AtomicU64,
    bytes_written: AtomicU64,
    errors: AtomicU64,
}

impl Counters {
    fn add_outcome(&self, outcome: &NbdSessionOutcome) {
        self.sessions.fetch_add(1, Ordering::Relaxed);
        self.reads
            .fetch_add(outcome.read_requests, Ordering::Relaxed);
        self.writes
            .fetch_add(outcome.write_requests, Ordering::Relaxed);
        self.flushes
            .fetch_add(outcome.flush_requests, Ordering::Relaxed);
        self.trims
            .fetch_add(outcome.trim_requests, Ordering::Relaxed);
        self.disconnects
            .fetch_add(outcome.disconnect_requests, Ordering::Relaxed);
        self.bytes_read
            .fetch_add(outcome.bytes_read, Ordering::Relaxed);
        self.bytes_written
            .fetch_add(outcome.bytes_written, Ordering::Relaxed);
    }

    fn snapshot(&self) -> NbdServerStats {
        NbdServerStats {
            connections_accepted: self.accepted.load(Ordering::Relaxed),
            connections_rejected: self.rejected.load(Ordering::Relaxed),
            sessions: self.sessions.load(Ordering::Relaxed),
            read_requests: self.reads.load(Ordering::Relaxed),
            write_requests: self.writes.load(Ordering::Relaxed),
            flush_requests: self.flushes.load(Ordering::Relaxed),
            trim_requests: self.trims.load(Ordering::Relaxed),
            disconnect_requests: self.disconnects.load(Ordering::Relaxed),
            bytes_read: self.bytes_read.load(Ordering::Relaxed),
            bytes_written: self.bytes_written.load(Ordering::Relaxed),
            protocol_errors: self.errors.load(Ordering::Relaxed),
        }
    }
}

/// Why a session ended normally.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NbdSessionEnd {
    /// The client sent `NBD_CMD_DISC`.
    Disconnect,
    /// The client sent `NBD_OPT_ABORT`.
    Aborted,
    /// The peer closed the stream.
    PeerClosed,
    /// The server handle requested shutdown.
    Stopped,
    /// A configured deadline expired.
    Timeout,
}

/// A completed session summary.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NbdSessionOutcome {
    /// Export selected by the client, if negotiation completed.
    pub export: Option<String>,
    /// READ requests.
    pub read_requests: u64,
    /// WRITE requests.
    pub write_requests: u64,
    /// FLUSH requests.
    pub flush_requests: u64,
    /// TRIM requests.
    pub trim_requests: u64,
    /// DISC requests.
    pub disconnect_requests: u64,
    /// Bytes returned to the client.
    pub bytes_read: u64,
    /// Bytes accepted from the client.
    pub bytes_written: u64,
    /// Normal termination reason.
    pub end: Option<NbdSessionEnd>,
}

/// A stream that can carry an NBD session.
///
/// `TcpStream` and `UnixStream` implement this; tests can provide an
/// in-memory duplex. The default timeout implementation is a no-op.
pub trait NbdTransport: Read + Write {
    /// Configure read/write poll timeouts (used for shutdown/deadline checks).
    fn set_io_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        let _ = timeout;
        Ok(())
    }

    /// Human-readable peer label for diagnostics.
    fn peer_label(&self) -> Option<String> {
        None
    }
}

impl NbdTransport for TcpStream {
    fn set_io_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }

    fn peer_label(&self) -> Option<String> {
        self.peer_addr().ok().map(|address| address.to_string())
    }
}

#[cfg(unix)]
impl NbdTransport for std::os::unix::net::UnixStream {
    fn set_io_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        self.set_read_timeout(timeout)?;
        self.set_write_timeout(timeout)
    }
}

/// A bound NBD listener waiting for `start`.
pub struct NbdServer {
    listener: Listener,
    catalog: Arc<NbdExportCatalog>,
    config: NbdServerConfig,
    clock: Arc<dyn NbdClock>,
}

enum Listener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixListener, PathBuf),
}

impl NbdServer {
    /// Bind a TCP listener.
    pub fn bind_tcp(
        address: impl ToSocketAddrs,
        catalog: NbdExportCatalog,
        config: NbdServerConfig,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        let listener = TcpListener::bind(address).map_err(|source| {
            crate::error::io_error("bind NBD TCP", "<nbd-listener>".into(), source)
        })?;
        Ok(Self {
            listener: Listener::Tcp(listener),
            catalog: Arc::new(catalog),
            config,
            clock: Arc::new(SystemClock),
        })
    }

    /// Bind a Unix-domain listener. The socket is removed on stop.
    #[cfg(unix)]
    pub fn bind_unix(
        path: impl AsRef<Path>,
        catalog: NbdExportCatalog,
        config: NbdServerConfig,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        let path = path.as_ref().to_path_buf();
        if !path.is_absolute() {
            return Err(invalid_nbd_image("NBD Unix socket path must be absolute"));
        }
        let listener = std::os::unix::net::UnixListener::bind(&path).map_err(|source| {
            crate::error::io_error("bind NBD Unix socket", path.clone(), source)
        })?;
        Ok(Self {
            listener: Listener::Unix(listener, path),
            catalog: Arc::new(catalog),
            config,
            clock: Arc::new(SystemClock),
        })
    }

    /// Inject a clock (tests can use [`ManualClock`]).
    pub fn with_clock(mut self, clock: Arc<dyn NbdClock>) -> Self {
        self.clock = clock;
        self
    }

    /// Configured export catalog.
    pub fn catalog(&self) -> &NbdExportCatalog {
        &self.catalog
    }

    /// Bound TCP address, when this is a TCP server.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        match &self.listener {
            Listener::Tcp(listener) => listener.local_addr().ok(),
            #[cfg(unix)]
            Listener::Unix(_, _) => None,
        }
    }

    /// Bound Unix socket path, when this is a Unix server.
    pub fn unix_path(&self) -> Option<&Path> {
        match &self.listener {
            Listener::Tcp(_) => None,
            #[cfg(unix)]
            Listener::Unix(_, path) => Some(path),
        }
    }

    /// Start accepting clients on a background thread.
    pub fn start(self) -> Result<NbdServerHandle, UsbError> {
        let NbdServer {
            listener,
            catalog,
            config,
            clock,
        } = self;
        listener.set_nonblocking(true).map_err(|source| {
            crate::error::io_error("configure NBD listener", "<nbd-listener>".into(), source)
        })?;

        let stop = Arc::new(AtomicBool::new(false));
        let counters = Arc::new(Counters::default());
        let active = Arc::new(AtomicU64::new(0));
        let wake = listener.wake_target();
        let local_addr = match &listener {
            Listener::Tcp(listener) => listener.local_addr().ok(),
            #[cfg(unix)]
            Listener::Unix(_, _) => None,
        };
        let unix_path = match &listener {
            Listener::Tcp(_) => None,
            #[cfg(unix)]
            Listener::Unix(_, path) => Some(path.clone()),
        };

        let accept_stop = Arc::clone(&stop);
        let accept_counters = Arc::clone(&counters);
        let accept_active = Arc::clone(&active);
        let accept_config = config.clone();
        let accept_catalog = Arc::clone(&catalog);
        let accept_clock = Arc::clone(&clock);
        let join = thread::Builder::new()
            .name("ozma-nbd-accept".to_owned())
            .spawn(move || {
                accept_loop(
                    listener,
                    accept_catalog,
                    accept_config,
                    accept_clock,
                    accept_stop,
                    accept_counters,
                    accept_active,
                );
            })
            .map_err(|source| UsbError::Spawn {
                command: "ozma-nbd-accept".to_owned(),
                source,
            })?;

        Ok(NbdServerHandle {
            stop,
            join: Some(join),
            counters,
            active,
            wake,
            local_addr,
            unix_path,
        })
    }
}

fn accept_loop(
    listener: Listener,
    catalog: Arc<NbdExportCatalog>,
    config: NbdServerConfig,
    clock: Arc<dyn NbdClock>,
    stop: Arc<AtomicBool>,
    counters: Arc<Counters>,
    active: Arc<AtomicU64>,
) {
    let mut clients: Vec<JoinHandle<()>> = Vec::new();
    loop {
        if stop.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok(mut stream) => {
                if stop.load(Ordering::SeqCst) {
                    break;
                }
                let live = active.fetch_add(1, Ordering::SeqCst) + 1;
                if live > config.max_clients as u64 {
                    active.fetch_sub(1, Ordering::SeqCst);
                    counters.rejected.fetch_add(1, Ordering::Relaxed);
                    let _ = stream.shutdown();
                    continue;
                }
                counters.accepted.fetch_add(1, Ordering::Relaxed);
                let _ = stream.set_io_timeout(Some(POLL_INTERVAL));
                let catalog = Arc::clone(&catalog);
                let config = config.clone();
                let clock = Arc::clone(&clock);
                let stop = Arc::clone(&stop);
                let client_counters = Arc::clone(&counters);
                let client_active = Arc::clone(&active);
                match thread::Builder::new()
                    .name("ozma-nbd-client".to_owned())
                    .spawn(move || {
                        let result =
                            serve_session(&mut stream, &catalog, &config, clock.as_ref(), &stop);
                        match result {
                            Ok(outcome) => client_counters.add_outcome(&outcome),
                            Err(error) => {
                                client_counters.sessions.fetch_add(1, Ordering::Relaxed);
                                client_counters.errors.fetch_add(1, Ordering::Relaxed);
                                let _ = error;
                            }
                        }
                        client_active.fetch_sub(1, Ordering::SeqCst);
                    }) {
                    Ok(handle) => clients.push(handle),
                    Err(_) => {
                        active.fetch_sub(1, Ordering::SeqCst);
                        counters.errors.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
            Err(_) => {
                counters.errors.fetch_add(1, Ordering::Relaxed);
                thread::sleep(POLL_INTERVAL);
            }
        }
    }
    for client in clients {
        let _ = client.join();
    }
    listener.remove_socket();
}

/// A running NBD server. Dropping it stops the listener and wakes clients.
pub struct NbdServerHandle {
    stop: Arc<AtomicBool>,
    join: Option<JoinHandle<()>>,
    counters: Arc<Counters>,
    active: Arc<AtomicU64>,
    wake: WakeTarget,
    local_addr: Option<SocketAddr>,
    unix_path: Option<PathBuf>,
}

impl NbdServerHandle {
    /// Bound TCP address.
    pub fn local_addr(&self) -> Option<SocketAddr> {
        self.local_addr
    }

    /// Bound Unix socket path.
    pub fn unix_path(&self) -> Option<&Path> {
        self.unix_path.as_deref()
    }

    /// Current counters.
    pub fn stats(&self) -> NbdServerStats {
        self.counters.snapshot()
    }

    /// Currently active client sessions.
    pub fn active_clients(&self) -> u64 {
        self.active.load(Ordering::SeqCst)
    }

    /// Stop accepting, wake active sessions, join the listener, and return the
    /// final counters. A client that ignores `shutdown` is bounded by the
    /// configured idle timeout.
    pub fn stop(mut self) -> NbdServerStats {
        self.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
        self.counters.snapshot()
    }

    fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
        self.wake.wake();
    }
}

impl Drop for NbdServerHandle {
    fn drop(&mut self) {
        self.request_stop();
        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

enum WakeTarget {
    Tcp(Option<SocketAddr>),
    #[cfg(unix)]
    Unix(PathBuf),
}

impl WakeTarget {
    fn wake(&self) {
        match self {
            Self::Tcp(Some(address)) => {
                let _ = TcpStream::connect_timeout(address, Duration::from_millis(100));
            }
            Self::Tcp(None) => {}
            #[cfg(unix)]
            Self::Unix(path) => {
                let _ = std::os::unix::net::UnixStream::connect(path);
            }
        }
    }
}

impl Listener {
    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Tcp(listener) => listener.set_nonblocking(nonblocking),
            #[cfg(unix)]
            Self::Unix(listener, _) => listener.set_nonblocking(nonblocking),
        }
    }

    fn wake_target(&self) -> WakeTarget {
        match self {
            Self::Tcp(listener) => {
                let address = listener.local_addr().ok().map(|address| {
                    let ip = if address.ip().is_unspecified() {
                        std::net::Ipv4Addr::LOCALHOST.into()
                    } else {
                        address.ip()
                    };
                    SocketAddr::new(ip, address.port())
                });
                WakeTarget::Tcp(address)
            }
            #[cfg(unix)]
            Self::Unix(_, path) => WakeTarget::Unix(path.clone()),
        }
    }

    fn accept(&self) -> io::Result<AcceptedStream> {
        match self {
            Self::Tcp(listener) => listener
                .accept()
                .map(|(stream, _)| AcceptedStream::Tcp(stream)),
            #[cfg(unix)]
            Self::Unix(listener, _) => listener
                .accept()
                .map(|(stream, _)| AcceptedStream::Unix(stream)),
        }
    }

    fn remove_socket(&self) {
        #[cfg(unix)]
        if let Self::Unix(_, path) = self {
            use std::os::unix::fs::FileTypeExt;
            if fs::symlink_metadata(path)
                .map(|metadata| metadata.file_type().is_socket())
                .unwrap_or(false)
            {
                let _ = fs::remove_file(path);
            }
        }
    }
}

enum AcceptedStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(std::os::unix::net::UnixStream),
}

impl Read for AcceptedStream {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buffer),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buffer),
        }
    }
}

impl Write for AcceptedStream {
    fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(buffer),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buffer),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
        }
    }
}

impl NbdTransport for AcceptedStream {
    fn set_io_timeout(&mut self, timeout: Option<Duration>) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_io_timeout(timeout),
            #[cfg(unix)]
            Self::Unix(stream) => stream.set_io_timeout(timeout),
        }
    }
}

impl AcceptedStream {
    fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => stream.shutdown(Shutdown::Both),
        }
    }
}

enum SessionSignal {
    Complete,
    Eof,
    Stopped,
    TimedOut,
}

struct Session<'a, S: NbdTransport> {
    stream: &'a mut S,
    catalog: &'a NbdExportCatalog,
    config: &'a NbdServerConfig,
    clock: &'a dyn NbdClock,
    stop: &'a AtomicBool,
    outcome: NbdSessionOutcome,
}

/// Serve one NBD session over an injected transport.
///
/// This is the test hook for the listener: pass a TCP/Unix stream, a custom
/// [`NbdTransport`], and a [`NbdClock`]. It returns a summary on clean
/// disconnect/abort/stop, and a typed protocol/I/O error for malformed frames.
pub fn serve_session<S: NbdTransport>(
    stream: &mut S,
    catalog: &NbdExportCatalog,
    config: &NbdServerConfig,
    clock: &dyn NbdClock,
    stop: &AtomicBool,
) -> Result<NbdSessionOutcome, UsbError> {
    config.validate()?;
    let _ = stream.set_io_timeout(Some(POLL_INTERVAL));
    let mut session = Session {
        stream,
        catalog,
        config,
        clock,
        stop,
        outcome: NbdSessionOutcome::default(),
    };
    session.run()
}

impl<'a, S: NbdTransport> Session<'a, S> {
    fn run(&mut self) -> Result<NbdSessionOutcome, UsbError> {
        let handshake_deadline = self
            .clock
            .now_millis()
            .saturating_add(self.config.handshake_timeout_ms);
        let export = match self.handshake(handshake_deadline)? {
            HandshakeOutcome::Aborted => {
                self.outcome.end = Some(NbdSessionEnd::Aborted);
                return Ok(std::mem::take(&mut self.outcome));
            }
            HandshakeOutcome::Export(export) => export,
            HandshakeOutcome::Closed(end) => {
                self.outcome.end = Some(end);
                return Ok(std::mem::take(&mut self.outcome));
            }
        };
        self.outcome.export = Some(export.export.name.clone());
        self.transmit(&export)
    }

    fn handshake(&mut self, deadline: u64) -> Result<HandshakeOutcome, UsbError> {
        let mut banner = Vec::with_capacity(18);
        banner.extend_from_slice(NBD_MAGIC);
        banner.extend_from_slice(&NBD_NEWSTYLE_MAGIC.to_be_bytes());
        banner.extend_from_slice(&(NBD_FLAG_FIXED_NEWSTYLE | NBD_FLAG_NO_ZEROES).to_be_bytes());
        self.write_all(&banner)?;

        let mut client_flags = [0u8; 4];
        match self.read_exact(&mut client_flags, Some(deadline))? {
            SessionSignal::Complete => {}
            other => return Ok(HandshakeOutcome::Closed(end_for(other))),
        }
        let client_flags = u32::from_be_bytes(client_flags);
        if client_flags & !(NBD_FLAG_C_FIXED_NEWSTYLE | NBD_FLAG_C_NO_ZEROES) != 0 {
            return Err(protocol_error("unknown client handshake flags"));
        }
        let no_zeroes = client_flags & NBD_FLAG_C_NO_ZEROES != 0;

        loop {
            let mut header = [0u8; 16];
            match self.read_exact(&mut header, Some(deadline))? {
                SessionSignal::Complete => {}
                other => return Ok(HandshakeOutcome::Closed(end_for(other))),
            }
            let magic = u64::from_be_bytes(header[0..8].try_into().unwrap_or([0; 8]));
            if magic != NBD_NEWSTYLE_MAGIC {
                return Err(protocol_error("bad option magic"));
            }
            let option = u32::from_be_bytes(header[8..12].try_into().unwrap_or([0; 4]));
            let length = u32::from_be_bytes(header[12..16].try_into().unwrap_or([0; 4]));
            if length > self.config.option_max_bytes {
                let _ = self.send_option_reply(option, NBD_REP_ERR_TOO_BIG, &[]);
                return Err(protocol_error(format!(
                    "option payload {length} exceeds {} bytes",
                    self.config.option_max_bytes
                )));
            }
            let mut payload = vec![0u8; length as usize];
            match self.read_exact(&mut payload, Some(deadline))? {
                SessionSignal::Complete => {}
                other => return Ok(HandshakeOutcome::Closed(end_for(other))),
            }

            match option {
                NBD_OPT_EXPORT_NAME => {
                    let name = parse_export_name(&payload)?;
                    let Some(export) = self.catalog.find(&name) else {
                        return Err(protocol_error(format!("unknown NBD export {name:?}")));
                    };
                    self.send_export_name(&export, no_zeroes)?;
                    return Ok(HandshakeOutcome::Export(export));
                }
                NBD_OPT_ABORT => {
                    self.send_option_reply(NBD_OPT_ABORT, NBD_REP_ACK, &[])?;
                    return Ok(HandshakeOutcome::Aborted);
                }
                NBD_OPT_LIST => {
                    if !payload.is_empty() {
                        self.send_option_reply(option, NBD_REP_ERR_INVALID, &[])?;
                        continue;
                    }
                    for entry in self.catalog.entries() {
                        self.send_option_reply(option, NBD_REP_SERVER, &server_entry(entry))?;
                    }
                    self.send_option_reply(option, NBD_REP_ACK, &[])?;
                }
                NBD_OPT_INFO | NBD_OPT_GO => {
                    let (name, requested) = match parse_info_payload(&payload) {
                        Ok(parsed) => parsed,
                        Err(reason) => {
                            self.send_option_reply(option, NBD_REP_ERR_INVALID, &[])?;
                            let _ = reason;
                            continue;
                        }
                    };
                    let Some(export) = self.catalog.find(&name) else {
                        self.send_option_reply(option, NBD_REP_ERR_UNKNOWN, &[])?;
                        continue;
                    };
                    for info in info_replies(&export, &requested, self.config) {
                        self.send_option_reply(option, NBD_REP_INFO, &info)?;
                    }
                    self.send_option_reply(option, NBD_REP_ACK, &[])?;
                    if option == NBD_OPT_GO {
                        return Ok(HandshakeOutcome::Export(export));
                    }
                }
                NBD_OPT_STARTTLS => {
                    // No TLS: the listener is loopback or behind an external
                    // terminator. Reply with the protocol's unsupported code.
                    self.send_option_reply(option, NBD_REP_ERR_UNSUP, &[])?;
                }
                other => {
                    self.send_option_reply(other, NBD_REP_ERR_UNSUP, &[])?;
                }
            }
        }
    }

    fn send_export_name(
        &mut self,
        export: &NbdExportSource,
        no_zeroes: bool,
    ) -> Result<(), UsbError> {
        let mut bytes = Vec::with_capacity(if no_zeroes { 10 } else { 134 });
        bytes.extend_from_slice(&export.export.size_bytes.to_be_bytes());
        bytes.extend_from_slice(&export.transmission_flags().to_be_bytes());
        if !no_zeroes {
            bytes.extend_from_slice(&ZERO_TAIL);
        }
        self.write_all(&bytes)
    }

    fn send_option_reply(
        &mut self,
        option: u32,
        reply_type: u32,
        payload: &[u8],
    ) -> Result<(), UsbError> {
        let mut bytes = Vec::with_capacity(20 + payload.len());
        bytes.extend_from_slice(&NBD_REP_MAGIC.to_be_bytes());
        bytes.extend_from_slice(&option.to_be_bytes());
        bytes.extend_from_slice(&reply_type.to_be_bytes());
        bytes.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        bytes.extend_from_slice(payload);
        self.write_all(&bytes)
    }

    fn transmit(&mut self, export: &NbdExportSource) -> Result<NbdSessionOutcome, UsbError> {
        let read_only = export.is_read_only();
        loop {
            let deadline = self
                .config
                .idle_timeout_ms
                .map(|millis| self.clock.now_millis().saturating_add(millis));
            let mut header = [0u8; NBD_REQUEST_HEADER_LEN];
            match self.read_exact(&mut header, deadline)? {
                SessionSignal::Complete => {}
                other => {
                    self.outcome.end = Some(end_for(other));
                    return Ok(std::mem::take(&mut self.outcome));
                }
            }
            let request = match NbdRequestHeader::decode(&header) {
                Ok(request) => request,
                Err(error) => {
                    self.outcome.end = Some(NbdSessionEnd::PeerClosed);
                    return Err(error);
                }
            };

            match request.command {
                NbdCommand::Read => {
                    self.outcome.read_requests += 1;
                    if request.length > self.config.max_request_bytes {
                        self.send_error(request.handle, EINVAL)?;
                        continue;
                    }
                    let mut data = vec![0u8; request.length as usize];
                    let response = match range_end(export, request.offset, request.length) {
                        Ok(()) => export.backend.read_at(request.offset, &mut data),
                        Err(error) => Err(error),
                    };
                    match response {
                        Ok(()) => {
                            self.send_reply_success(request.handle, &data)?;
                            self.outcome.bytes_read += u64::from(request.length);
                        }
                        Err(error) => self.send_error(request.handle, errno_for(&error))?,
                    }
                }
                NbdCommand::Write => {
                    self.outcome.write_requests += 1;
                    if request.length > self.config.max_request_bytes {
                        // Too large to drain safely; reply then terminate.
                        self.send_error(request.handle, EINVAL)?;
                        self.outcome.end = Some(NbdSessionEnd::PeerClosed);
                        return Ok(std::mem::take(&mut self.outcome));
                    }
                    let mut data = vec![0u8; request.length as usize];
                    match self.read_exact(&mut data, deadline)? {
                        SessionSignal::Complete => {}
                        other => {
                            self.outcome.end = Some(end_for(other));
                            return Ok(std::mem::take(&mut self.outcome));
                        }
                    }
                    self.outcome.bytes_written += u64::from(request.length);
                    let response = if read_only {
                        Err(UsbError::ReadOnlyMedia(
                            "the NBD export is read-only".to_owned(),
                        ))
                    } else {
                        match range_end(export, request.offset, request.length) {
                            Ok(()) => export.backend.write_at(request.offset, &data),
                            Err(error) => Err(error),
                        }
                    };
                    match response {
                        Ok(()) => {
                            if request.flags & crate::nbd::NBD_CMD_FLAG_FUA != 0 {
                                export.backend.flush()?;
                            }
                            self.send_error(request.handle, 0)?;
                        }
                        Err(error) => self.send_error(request.handle, errno_for(&error))?,
                    }
                }
                NbdCommand::Disconnect => {
                    self.outcome.disconnect_requests += 1;
                    self.outcome.end = Some(NbdSessionEnd::Disconnect);
                    return Ok(std::mem::take(&mut self.outcome));
                }
                NbdCommand::Flush => {
                    self.outcome.flush_requests += 1;
                    match export.backend.flush() {
                        Ok(()) => self.send_error(request.handle, 0)?,
                        Err(error) => self.send_error(request.handle, errno_for(&error))?,
                    }
                }
                NbdCommand::Trim => {
                    self.outcome.trim_requests += 1;
                    let response = if read_only {
                        Err(UsbError::ReadOnlyMedia(
                            "the NBD export is read-only".to_owned(),
                        ))
                    } else {
                        match range_end(export, request.offset, request.length) {
                            Ok(()) => export
                                .backend
                                .trim(request.offset, u64::from(request.length)),
                            Err(error) => Err(error),
                        }
                    };
                    match response {
                        Ok(()) => self.send_error(request.handle, 0)?,
                        Err(error) => self.send_error(request.handle, errno_for(&error))?,
                    }
                }
                NbdCommand::WriteZeroes => {
                    if read_only {
                        self.send_error(request.handle, EPERM)?;
                    } else {
                        // Not advertised (`NBD_FLAG_SEND_WRITE_ZEROES` is never
                        // set), so treat it as unsupported.
                        self.send_error(request.handle, EOPNOTSUPP)?;
                    }
                }
                NbdCommand::Unknown(_) => {
                    self.send_error(request.handle, EINVAL)?;
                    self.outcome.end = Some(NbdSessionEnd::PeerClosed);
                    return Ok(std::mem::take(&mut self.outcome));
                }
            }
        }
    }

    fn send_reply_success(&mut self, handle: u64, data: &[u8]) -> Result<(), UsbError> {
        let mut bytes = Vec::with_capacity(NBD_REPLY_HEADER_LEN + data.len());
        bytes.extend_from_slice(&NbdReply::success(handle).encode());
        bytes.extend_from_slice(data);
        self.write_all(&bytes)
    }

    fn send_error(&mut self, handle: u64, error: u32) -> Result<(), UsbError> {
        let reply = NbdReply::error(handle, error);
        self.write_all(&reply.encode())
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), UsbError> {
        self.stream.write_all(bytes).map_err(|source| {
            crate::error::io_error("write NBD stream", "<nbd-stream>".into(), source)
        })
    }

    fn read_exact(
        &mut self,
        buffer: &mut [u8],
        deadline: Option<u64>,
    ) -> Result<SessionSignal, UsbError> {
        let mut filled = 0usize;
        while filled < buffer.len() {
            if self.stop.load(Ordering::SeqCst) {
                return Ok(SessionSignal::Stopped);
            }
            match self.stream.read(&mut buffer[filled..]) {
                Ok(0) => return Ok(SessionSignal::Eof),
                Ok(count) => filled += count,
                Err(error)
                    if matches!(
                        error.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                    ) =>
                {
                    if self.stop.load(Ordering::SeqCst) {
                        return Ok(SessionSignal::Stopped);
                    }
                    if let Some(deadline) = deadline {
                        if self.clock.now_millis() >= deadline {
                            return Ok(SessionSignal::TimedOut);
                        }
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(source) => {
                    return Err(crate::error::io_error(
                        "read NBD stream",
                        "<nbd-stream>".into(),
                        source,
                    ));
                }
            }
        }
        Ok(SessionSignal::Complete)
    }
}

enum HandshakeOutcome {
    Export(NbdExportSource),
    Aborted,
    Closed(NbdSessionEnd),
}

fn end_for(signal: SessionSignal) -> NbdSessionEnd {
    match signal {
        SessionSignal::Complete => NbdSessionEnd::PeerClosed,
        SessionSignal::Eof => NbdSessionEnd::PeerClosed,
        SessionSignal::Stopped => NbdSessionEnd::Stopped,
        SessionSignal::TimedOut => NbdSessionEnd::Timeout,
    }
}

fn parse_export_name(payload: &[u8]) -> Result<String, UsbError> {
    let name = std::str::from_utf8(payload)
        .map_err(|_| protocol_error("export name is not valid UTF-8"))?
        .to_owned();
    if name.len() > 255 {
        return Err(protocol_error("export name is longer than 255 bytes"));
    }
    Ok(name)
}

fn parse_info_payload(payload: &[u8]) -> Result<(String, Vec<u16>), &'static str> {
    if payload.len() < 6 {
        return Err("truncated INFO/GO payload");
    }
    let name_len = u32::from_be_bytes(payload[0..4].try_into().unwrap_or([0; 4])) as usize;
    let name_start = 4usize;
    let name_end = name_start
        .checked_add(name_len)
        .ok_or("name length overflow")?;
    if name_end + 2 > payload.len() {
        return Err("truncated INFO/GO export name");
    }
    let name = std::str::from_utf8(&payload[name_start..name_end])
        .map_err(|_| "export name is not valid UTF-8")?
        .to_owned();
    if name.len() > 255 {
        return Err("export name is longer than 255 bytes");
    }
    let count =
        u16::from_be_bytes(payload[name_end..name_end + 2].try_into().unwrap_or([0; 2])) as usize;
    let mut requested = Vec::with_capacity(count.min(64));
    let mut offset = name_end + 2;
    for _ in 0..count.min(64) {
        if offset + 2 > payload.len() {
            return Err("truncated INFO/GO information list");
        }
        requested.push(u16::from_be_bytes(
            payload[offset..offset + 2].try_into().unwrap_or([0; 2]),
        ));
        offset += 2;
    }
    Ok((name, requested))
}

fn server_entry(entry: &NbdExportSource) -> Vec<u8> {
    let name = entry.export.name.as_bytes();
    let description = entry.export.description.as_deref().unwrap_or("").as_bytes();
    let mut bytes = Vec::with_capacity(4 + name.len() + description.len());
    bytes.extend_from_slice(&(name.len() as u32).to_be_bytes());
    bytes.extend_from_slice(name);
    bytes.extend_from_slice(description);
    bytes
}

fn info_replies(
    entry: &NbdExportSource,
    requested: &[u16],
    config: &NbdServerConfig,
) -> Vec<Vec<u8>> {
    let mut infos = vec![NBD_INFO_EXPORT];
    for info in requested {
        if !infos.contains(info) {
            infos.push(*info);
        }
    }
    let mut replies = Vec::with_capacity(infos.len());
    for info in infos {
        match info {
            NBD_INFO_EXPORT => {
                let mut payload = Vec::with_capacity(10);
                payload.extend_from_slice(&NBD_INFO_EXPORT.to_be_bytes());
                payload.extend_from_slice(&entry.export.size_bytes.to_be_bytes());
                payload.extend_from_slice(&entry.transmission_flags().to_be_bytes());
                replies.push(payload);
            }
            NBD_INFO_NAME => {
                let mut payload = Vec::new();
                payload.extend_from_slice(&NBD_INFO_NAME.to_be_bytes());
                payload.extend_from_slice(entry.export.name.as_bytes());
                replies.push(payload);
            }
            NBD_INFO_DESCRIPTION => {
                if let Some(description) = &entry.export.description {
                    let mut payload = Vec::new();
                    payload.extend_from_slice(&NBD_INFO_DESCRIPTION.to_be_bytes());
                    payload.extend_from_slice(description.as_bytes());
                    replies.push(payload);
                }
            }
            NBD_INFO_BLOCK_SIZE => {
                let mut payload = Vec::with_capacity(14);
                payload.extend_from_slice(&NBD_INFO_BLOCK_SIZE.to_be_bytes());
                payload.extend_from_slice(&1u32.to_be_bytes());
                payload.extend_from_slice(&entry.export.block_size.to_be_bytes());
                payload.extend_from_slice(&config.max_request_bytes.to_be_bytes());
                replies.push(payload);
            }
            _ => {}
        }
    }
    replies
}

fn range_end(export: &NbdExportSource, offset: u64, length: u32) -> Result<(), UsbError> {
    let end = offset
        .checked_add(u64::from(length))
        .ok_or_else(range_error)?;
    if end > export.export.size_bytes {
        return Err(range_error());
    }
    Ok(())
}

fn errno_for(error: &UsbError) -> u32 {
    match error {
        UsbError::ReadOnlyMedia(_) => EPERM,
        UsbError::MediaNotFound(_) | UsbError::MediaNotFile(_) => EIO,
        UsbError::InvalidMedia { .. }
        | UsbError::InvalidConfig(_)
        | UsbError::InvalidField { .. } => EINVAL,
        UsbError::UnsupportedMediaFormat { .. } | UsbError::UnsupportedPlatform => EOPNOTSUPP,
        UsbError::Io { .. } | UsbError::ConfigFs { .. } => EIO,
        _ => EINVAL,
    }
}

fn protocol_error(reason: impl Into<String>) -> UsbError {
    UsbError::Protocol {
        protocol: "nbd",
        reason: reason.into(),
    }
}

fn range_error() -> UsbError {
    UsbError::Protocol {
        protocol: "nbd",
        reason: "request range is outside the export".to_owned(),
    }
}

fn invalid_nbd_image(reason: impl Into<String>) -> UsbError {
    UsbError::InvalidMedia {
        format: "nbd",
        path: PathBuf::from("<nbd-server>"),
        reason: reason.into(),
    }
}
