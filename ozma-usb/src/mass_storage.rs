// SPDX-License-Identifier: AGPL-3.0-only
//! SCSI block data path for virtual USB mass storage.
//!
//! This module is the safe Rust counterpart of
//! `softnode/usb_mass_storage.py`: it decodes USB Mass Storage Bulk-Only
//! Transport (BBB) command blocks, serves the SCSI block commands a host uses
//! for a removable disk/CD-ROM, and routes writable media through the same
//! bounded [`WriteOverlay`] used by the NBD data path.
//!
//! The base image is immutable:
//!
//! * read-only media (FAT32/ISO/CD-ROM) is served directly and rejects every
//!   write with the typed [`UsbError::ReadOnlyMedia`] or a SCSI
//!   `DATA PROTECT / WRITE PROTECTED` sense;
//! * writable media (`MassStorageDevice::writable_*`) reads through the
//!   overlay, copies a touched base block into the overlay before patching it,
//!   and never opens the base for writing;
//! * `SYNCHRONIZE CACHE(10)` commits a configured checksummed overlay
//!   persistence file, and [`MassStorageDevice::stop`] commits it when the
//!   persistence policy requests it.
//!
//! No method mounts a filesystem, opens a block device, spawns a process, or
//! performs any real USB transfer. The caller supplies the CBW bytes and any
//! OUT data; an in-memory or FunctionFS transport can drive the same device.
//!
//! # SCSI command support
//!
//! `TEST UNIT READY`, `REQUEST SENSE`, `INQUIRY`, `MODE SENSE(6)`,
//! `MODE SENSE(10)`, `START STOP UNIT`, `PREVENT ALLOW MEDIUM REMOVAL`,
//! `READ CAPACITY(10)`, `READ FORMAT CAPACITIES`, `READ(10)`, `WRITE(10)`, and
//! `SYNCHRONIZE CACHE(10)`. Unknown commands return a SCSI `ILLEGAL REQUEST`
//! check condition rather than a fabricated success.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::error::{io_error, UsbError};
use crate::media::MassStorageImage;
use crate::overlay::{OverlayConfig, WriteOverlay};

/// Logical SCSI sector size used by the BBB path.
pub const SCSI_SECTOR_BYTES: usize = 512;

/// Size of one USB Mass Storage command block wrapper.
pub const SCSI_CBW_BYTES: usize = 31;

/// Size of one USB Mass Storage command status wrapper.
pub const SCSI_CSW_BYTES: usize = 13;

/// Maximum data phase accepted in one command (1 MiB / 2048 sectors).
pub const SCSI_MAX_TRANSFER_BYTES: usize = 1024 * 1024;

/// Default read-only media bound (256 MiB).
pub const SCSI_DEFAULT_MAX_MEDIA_BYTES: u64 = 256 * 1024 * 1024;

const CBW_SIGNATURE: u32 = 0x4342_5355; // "USBC" little-endian
const CSW_SIGNATURE: u32 = 0x5342_5355; // "USBS" little-endian

const SCSI_TEST_UNIT_READY: u8 = 0x00;
const SCSI_REQUEST_SENSE: u8 = 0x03;
const SCSI_INQUIRY: u8 = 0x12;
const SCSI_MODE_SENSE_6: u8 = 0x1a;
const SCSI_START_STOP_UNIT: u8 = 0x1b;
const SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL: u8 = 0x1e;
const SCSI_READ_FORMAT_CAPACITIES: u8 = 0x23;
const SCSI_READ_CAPACITY_10: u8 = 0x25;
const SCSI_READ_10: u8 = 0x28;
const SCSI_WRITE_10: u8 = 0x2a;
const SCSI_SYNCHRONIZE_CACHE_10: u8 = 0x35;
const SCSI_MODE_SENSE_10: u8 = 0x5a;

/// SCSI sense key `NO SENSE`.
pub const SENSE_NO_SENSE: u8 = 0x00;
/// SCSI sense key `MEDIUM ERROR`.
pub const SENSE_MEDIUM_ERROR: u8 = 0x03;
/// SCSI sense key `ILLEGAL REQUEST`.
pub const SENSE_ILLEGAL_REQUEST: u8 = 0x05;
/// SCSI sense key `DATA PROTECT`.
pub const SENSE_DATA_PROTECT: u8 = 0x07;

/// Command status wrapper status byte.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScsiStatus {
    /// Command completed successfully.
    Good,
    /// Command failed; fetch sense with `REQUEST SENSE`.
    CheckCondition,
    /// Command/data direction mismatch.
    PhaseError,
}

impl ScsiStatus {
    /// CSW status byte.
    pub fn as_byte(self) -> u8 {
        match self {
            Self::Good => 0,
            Self::CheckCondition => 1,
            Self::PhaseError => 2,
        }
    }
}

/// Fixed-format sense data returned by `REQUEST SENSE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ScsiSense {
    /// Sense key (`0x05` illegal request, `0x07` data protect, ...).
    pub key: u8,
    /// Additional sense code.
    pub asc: u8,
    /// Additional sense code qualifier.
    pub ascq: u8,
}

impl ScsiSense {
    /// No error.
    pub fn none() -> Self {
        Self {
            key: SENSE_NO_SENSE,
            asc: 0,
            ascq: 0,
        }
    }

    /// `ILLEGAL REQUEST / LBA OUT OF RANGE`.
    pub fn lba_out_of_range() -> Self {
        Self {
            key: SENSE_ILLEGAL_REQUEST,
            asc: 0x21,
            ascq: 0,
        }
    }

    /// `ILLEGAL REQUEST / INVALID FIELD IN CDB`.
    pub fn invalid_field() -> Self {
        Self {
            key: SENSE_ILLEGAL_REQUEST,
            asc: 0x24,
            ascq: 0,
        }
    }

    /// `ILLEGAL REQUEST / INVALID COMMAND OPERATION CODE`.
    pub fn invalid_command() -> Self {
        Self {
            key: SENSE_ILLEGAL_REQUEST,
            asc: 0x20,
            ascq: 0,
        }
    }

    /// `DATA PROTECT / WRITE PROTECTED`.
    pub fn write_protected() -> Self {
        Self {
            key: SENSE_DATA_PROTECT,
            asc: 0x27,
            ascq: 0,
        }
    }

    /// `MEDIUM ERROR / WRITE ERROR`.
    pub fn write_error() -> Self {
        Self {
            key: SENSE_MEDIUM_ERROR,
            asc: 0x0c,
            ascq: 0,
        }
    }

    /// Encode 18 bytes of fixed-format sense data (all zeros when no error).
    pub fn to_bytes(self) -> [u8; 18] {
        let mut bytes = [0u8; 18];
        bytes[0] = 0x70; // current errors, fixed format
        bytes[2] = self.key & 0x0f;
        bytes[7] = 10; // additional sense length
        bytes[12] = self.asc;
        bytes[13] = self.ascq;
        bytes
    }
}

/// One decoded USB Mass Storage command block wrapper.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScsiCommandBlock {
    /// Host command tag, echoed in the CSW.
    pub tag: u32,
    /// Expected data-phase length.
    pub transfer_length: u32,
    /// Whether the data phase is device-to-host.
    pub direction_in: bool,
    /// Logical unit number (only LUN 0 is served).
    pub lun: u8,
    /// SCSI operation code.
    pub opcode: u8,
    /// Command descriptor block bytes.
    pub cdb: [u8; 16],
    /// Significant CDB bytes (`1..=16`).
    pub cdb_length: u8,
}

impl ScsiCommandBlock {
    /// Parse a 31-byte CBW. A malformed signature/length is a typed protocol
    /// error rather than an implicit success.
    pub fn parse(bytes: &[u8]) -> Result<Self, UsbError> {
        if bytes.len() < SCSI_CBW_BYTES {
            return Err(UsbError::Protocol {
                protocol: "scsi",
                reason: format!("CBW is {SCSI_CBW_BYTES} bytes, got {}", bytes.len()),
            });
        }
        let signature = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]);
        if signature != CBW_SIGNATURE {
            return Err(UsbError::Protocol {
                protocol: "scsi",
                reason: format!("bad CBW signature 0x{signature:08x}"),
            });
        }
        let tag = u32::from_le_bytes([bytes[4], bytes[5], bytes[6], bytes[7]]);
        let transfer_length = u32::from_le_bytes([bytes[8], bytes[9], bytes[10], bytes[11]]);
        let flags = bytes[12];
        let lun = bytes[13];
        let cdb_length = bytes[14] & 0x1f;
        if cdb_length == 0 || cdb_length as usize > 16 {
            return Err(UsbError::Protocol {
                protocol: "scsi",
                reason: format!("CBW CDB length {cdb_length} is outside 1..=16"),
            });
        }
        let mut cdb = [0u8; 16];
        cdb[..cdb_length as usize].copy_from_slice(&bytes[15..15 + cdb_length as usize]);
        Ok(Self {
            tag,
            transfer_length,
            direction_in: flags & 0x80 != 0,
            lun,
            opcode: cdb[0],
            cdb,
            cdb_length,
        })
    }

    /// Encode a 13-byte CSW for this command.
    pub fn csw(&self, residue: u32, status: ScsiStatus) -> Vec<u8> {
        let mut bytes = Vec::with_capacity(SCSI_CSW_BYTES);
        bytes.extend_from_slice(&CSW_SIGNATURE.to_le_bytes());
        bytes.extend_from_slice(&self.tag.to_le_bytes());
        bytes.extend_from_slice(&residue.to_le_bytes());
        bytes.push(status.as_byte());
        bytes
    }

    fn lba_and_count(&self) -> (u32, u16) {
        let lba = u32::from_be_bytes([self.cdb[2], self.cdb[3], self.cdb[4], self.cdb[5]]);
        let count = u16::from_be_bytes([self.cdb[7], self.cdb[8]]);
        (lba, count)
    }
}

/// Result of one SCSI command: optional IN data, residue, status, and sense.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ScsiReply {
    /// Device-to-host data phase.
    pub data_in: Vec<u8>,
    /// Number of bytes not transferred relative to the CBW length.
    pub residue: u32,
    /// CSW status.
    pub status: ScsiStatus,
    /// Sense data for a check condition (also returned by `REQUEST SENSE`).
    pub sense: Option<ScsiSense>,
}

impl ScsiReply {
    fn good(data_in: Vec<u8>, transfer_length: u32) -> Self {
        let residue = transfer_length.saturating_sub(data_in.len() as u32);
        Self {
            data_in,
            residue,
            status: ScsiStatus::Good,
            sense: None,
        }
    }

    fn empty() -> Self {
        Self {
            data_in: Vec::new(),
            residue: 0,
            status: ScsiStatus::Good,
            sense: None,
        }
    }

    fn check(sense: ScsiSense) -> Self {
        Self {
            data_in: Vec::new(),
            residue: 0,
            status: ScsiStatus::CheckCondition,
            sense: Some(sense),
        }
    }

    fn phase_error() -> Self {
        Self {
            data_in: Vec::new(),
            residue: 0,
            status: ScsiStatus::PhaseError,
            sense: None,
        }
    }

    /// CSW for the command that produced this reply.
    pub fn csw(&self, command: &ScsiCommandBlock) -> Vec<u8> {
        command.csw(self.residue, self.status)
    }
}

/// When a writable mass-storage overlay commits its persistence file.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MassStoragePersistPolicy {
    /// Commit on SCSI `SYNCHRONIZE CACHE(10)`.
    pub persist_on_flush: bool,
    /// Commit when the device is stopped/unbound.
    pub persist_on_stop: bool,
}

impl Default for MassStoragePersistPolicy {
    fn default() -> Self {
        Self {
            persist_on_flush: true,
            persist_on_stop: true,
        }
    }
}

impl MassStoragePersistPolicy {
    /// Commit only on flush.
    pub fn flush_only() -> Self {
        Self {
            persist_on_flush: true,
            persist_on_stop: false,
        }
    }

    /// Never persist automatically.
    pub fn manual() -> Self {
        Self {
            persist_on_flush: false,
            persist_on_stop: false,
        }
    }
}

/// Live overlay summary for a writable mass-storage LUN.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MassStorageOverlayStatus {
    /// Copy-on-write block size.
    pub block_size: usize,
    /// Immutable base image length.
    pub base_bytes: u64,
    /// Bytes currently allocated in the overlay.
    pub allocated_bytes: u64,
    /// Number of allocated overlay blocks.
    pub allocated_blocks: usize,
    /// Configured allocation ceiling.
    pub max_overlay_bytes: u64,
    /// Whether writes are uncommitted.
    pub dirty: bool,
    /// Persistence file, when configured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub persistence: Option<PathBuf>,
    /// Commit on flush.
    pub persist_on_flush: bool,
    /// Commit on stop.
    pub persist_on_stop: bool,
}

enum MediaBase {
    Memory(Arc<[u8]>),
    File { path: PathBuf, len: u64, file: File },
}

impl MediaBase {
    fn len(&self) -> u64 {
        match self {
            Self::Memory(bytes) => bytes.len() as u64,
            Self::File { len, .. } => *len,
        }
    }

    fn read_into(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), UsbError> {
        match self {
            Self::Memory(bytes) => {
                let start = offset as usize;
                let end = start
                    .checked_add(buffer.len())
                    .ok_or_else(|| bounds("read range overflows"))?;
                if end > bytes.len() {
                    return Err(bounds(format!(
                        "read [{offset}, {}) exceeds base length {}",
                        offset.saturating_add(buffer.len() as u64),
                        bytes.len()
                    )));
                }
                buffer.copy_from_slice(&bytes[start..end]);
                Ok(())
            }
            Self::File { path, len, file } => {
                let end = offset
                    .checked_add(buffer.len() as u64)
                    .ok_or_else(|| bounds("read range overflows"))?;
                if end > *len {
                    return Err(bounds(format!(
                        "read [{offset}, {end}) exceeds base length {len}"
                    )));
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|source| io_error("seek media image", path.clone(), source))?;
                file.read_exact(buffer)
                    .map_err(|source| io_error("read media image", path.clone(), source))
            }
        }
    }
}

enum Backing {
    ReadOnly(MediaBase),
    Overlay {
        overlay: WriteOverlay,
        policy: MassStoragePersistPolicy,
    },
}

/// A bounded SCSI block device over read-only or copy-on-write media.
pub struct MassStorageDevice {
    backing: Backing,
    vendor: [u8; 8],
    product: [u8; 16],
    revision: [u8; 4],
    last_sense: Option<ScsiSense>,
}

impl std::fmt::Debug for MassStorageDevice {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MassStorageDevice")
            .field("read_only", &self.is_read_only())
            .field("sectors", &self.sector_count())
            .field("overlay", &self.overlay_status())
            .finish_non_exhaustive()
    }
}

impl MassStorageDevice {
    /// A read-only in-memory image (raw, FAT32, or ISO bytes) with a byte bound.
    pub fn read_only_bytes(base: impl Into<Vec<u8>>, max_bytes: u64) -> Result<Self, UsbError> {
        let base = base.into();
        validate_image_len(base.len() as u64, max_bytes, "raw", Path::new("<memory>"))?;
        Ok(Self::from_read_only(MediaBase::Memory(Arc::from(base))))
    }

    /// A read-only regular file, opened read-only and bounded by `max_bytes`.
    pub fn read_only_file(path: impl AsRef<Path>, max_bytes: u64) -> Result<Self, UsbError> {
        let path = path.as_ref();
        let metadata = std::fs::metadata(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                UsbError::MediaNotFound(path.to_path_buf())
            } else {
                io_error("stat media image", path.to_path_buf(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(path.to_path_buf()));
        }
        validate_image_len(metadata.len(), max_bytes, "raw", path)?;
        let file = File::open(path)
            .map_err(|source| io_error("open media image", path.to_path_buf(), source))?;
        Ok(Self::from_read_only(MediaBase::File {
            path: path.to_path_buf(),
            len: metadata.len(),
            file,
        }))
    }

    /// A writable in-memory image backed by the bounded overlay.
    pub fn writable_bytes(
        base: impl Into<Vec<u8>>,
        config: OverlayConfig,
        policy: MassStoragePersistPolicy,
    ) -> Result<Self, UsbError> {
        let base = base.into();
        validate_image_len(
            base.len() as u64,
            config.max_base_bytes,
            "raw",
            Path::new("<memory>"),
        )?;
        let overlay = WriteOverlay::from_bytes(base, config)?;
        Ok(Self::from_overlay(overlay, policy))
    }

    /// A writable regular file read once as the immutable base.
    pub fn writable_file(
        path: impl AsRef<Path>,
        config: OverlayConfig,
        policy: MassStoragePersistPolicy,
    ) -> Result<Self, UsbError> {
        let path = path.as_ref();
        let metadata = std::fs::metadata(path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                UsbError::MediaNotFound(path.to_path_buf())
            } else {
                io_error("stat media image", path.to_path_buf(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(path.to_path_buf()));
        }
        validate_image_len(metadata.len(), config.max_base_bytes, "raw", path)?;
        let overlay = WriteOverlay::from_file(path, config)?;
        Ok(Self::from_overlay(overlay, policy))
    }

    /// A writable regular file plus an existing checksummed overlay file.
    pub fn load_writable_file(
        base: impl AsRef<Path>,
        config: OverlayConfig,
        policy: MassStoragePersistPolicy,
        persistence: impl AsRef<Path>,
    ) -> Result<Self, UsbError> {
        let overlay = WriteOverlay::load_from_file(base, config, persistence)?;
        validate_image_len(
            overlay.base_len(),
            overlay.base_len(),
            "overlay",
            Path::new("<overlay>"),
        )?;
        Ok(Self::from_overlay(overlay, policy))
    }

    /// A read-only FAT32 image built by this crate.
    pub fn read_only_fat32(image: &crate::fat32::Fat32Image) -> Result<Self, UsbError> {
        Self::read_only_bytes(image.as_bytes().to_vec(), SCSI_DEFAULT_MAX_MEDIA_BYTES)
    }

    /// Open an existing `MassStorageImage` using the same read-only policy as
    /// ConfigFS attachment.
    ///
    /// CD-ROM, ISO9660, and explicitly read-only media are served directly and
    /// reject writes. A writable raw disk is served through the overlay. FAT32
    /// images created by this crate are read-only at the model layer and stay
    /// byte-identical.
    pub fn from_media(
        media: &MassStorageImage,
        config: OverlayConfig,
        policy: MassStoragePersistPolicy,
    ) -> Result<Self, UsbError> {
        media.validate()?;
        if media.read_only || media.mode.is_cdrom() || media.format.is_read_only() {
            Self::read_only_file(&media.path, config.max_base_bytes)
        } else {
            Self::writable_file(&media.path, config, policy)
        }
    }

    fn from_read_only(base: MediaBase) -> Self {
        Self {
            backing: Backing::ReadOnly(base),
            vendor: *b"OZMA    ",
            product: *b"Virtual Media   ",
            revision: *b"1.0 ",
            last_sense: None,
        }
    }

    fn from_overlay(overlay: WriteOverlay, policy: MassStoragePersistPolicy) -> Self {
        Self {
            backing: Backing::Overlay { overlay, policy },
            vendor: *b"OZMA    ",
            product: *b"Virtual Media   ",
            revision: *b"1.0 ",
            last_sense: None,
        }
    }

    /// Override the INQUIRY vendor/product/revision strings.
    pub fn with_inquiry(mut self, vendor: &str, product: &str, revision: &str) -> Self {
        self.vendor = fixed_ascii::<8>(vendor);
        self.product = fixed_ascii::<16>(product);
        self.revision = fixed_ascii::<4>(revision);
        self
    }

    /// Whether writes are rejected.
    pub fn is_read_only(&self) -> bool {
        matches!(self.backing, Backing::ReadOnly(_))
    }

    /// Immutable base image length.
    pub fn base_len(&self) -> u64 {
        match &self.backing {
            Backing::ReadOnly(base) => base.len(),
            Backing::Overlay { overlay, .. } => overlay.base_len(),
        }
    }

    /// Number of whole 512-byte sectors.
    pub fn sector_count(&self) -> u64 {
        self.base_len() / SCSI_SECTOR_BYTES as u64
    }

    /// Live overlay summary, or `None` for read-only media.
    pub fn overlay_status(&self) -> Option<MassStorageOverlayStatus> {
        match &self.backing {
            Backing::ReadOnly(_) => None,
            Backing::Overlay { overlay, policy } => Some(MassStorageOverlayStatus {
                block_size: overlay.block_size(),
                base_bytes: overlay.base_len(),
                allocated_bytes: overlay.allocated_bytes(),
                allocated_blocks: overlay.allocated_blocks(),
                max_overlay_bytes: overlay.max_overlay_bytes(),
                dirty: overlay.is_dirty(),
                persistence: overlay.persistence_path().map(Path::to_path_buf),
                persist_on_flush: policy.persist_on_flush,
                persist_on_stop: policy.persist_on_stop,
            }),
        }
    }

    /// Read exactly `buffer.len()` bytes at a byte offset.
    pub fn read_at(&mut self, offset: u64, buffer: &mut [u8]) -> Result<(), UsbError> {
        check_range(offset, buffer.len(), self.base_len())?;
        match &mut self.backing {
            Backing::ReadOnly(base) => base.read_into(offset, buffer),
            Backing::Overlay { overlay, .. } => overlay.read_into(offset, buffer),
        }
    }

    /// Write bytes at a byte offset through the overlay.
    ///
    /// Read-only media is rejected before any I/O. Out-of-range and
    /// overlay-allocation failures are typed.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<(), UsbError> {
        if self.is_read_only() {
            return Err(UsbError::ReadOnlyMedia(format!(
                "mass-storage media is read-only; refusing {}-byte write at offset {offset}",
                data.len()
            )));
        }
        check_range(offset, data.len(), self.base_len())?;
        match &mut self.backing {
            Backing::ReadOnly(_) => unreachable!("read-only media returned above"),
            Backing::Overlay { overlay, .. } => overlay.write_at(offset, data).map(|_| ()),
        }
    }

    /// Read whole sectors by logical block address.
    pub fn read_sectors(&mut self, lba: u64, count: u16) -> Result<Vec<u8>, UsbError> {
        let length = checked_transfer_len(count)?;
        let offset = lba
            .checked_mul(SCSI_SECTOR_BYTES as u64)
            .ok_or_else(|| bounds("sector offset overflows"))?;
        let mut buffer = vec![0u8; length];
        self.read_at(offset, &mut buffer)?;
        Ok(buffer)
    }

    /// Write whole sectors by logical block address through the overlay.
    pub fn write_sectors(&mut self, lba: u64, data: &[u8]) -> Result<(), UsbError> {
        if self.is_read_only() {
            return Err(UsbError::ReadOnlyMedia(
                "mass-storage media is read-only".to_owned(),
            ));
        }
        if data.len() % SCSI_SECTOR_BYTES != 0 {
            return Err(bounds(format!(
                "write length {} is not a multiple of {SCSI_SECTOR_BYTES}",
                data.len()
            )));
        }
        if data.len() > SCSI_MAX_TRANSFER_BYTES {
            return Err(bounds(format!(
                "write is {} bytes; maximum is {SCSI_MAX_TRANSFER_BYTES}",
                data.len()
            )));
        }
        let offset = lba
            .checked_mul(SCSI_SECTOR_BYTES as u64)
            .ok_or_else(|| bounds("sector offset overflows"))?;
        self.write_at(offset, data)
    }

    /// Commit the overlay to its configured persistence path immediately,
    /// independent of the persistence policy.
    pub fn commit(&mut self) -> Result<(), UsbError> {
        match &mut self.backing {
            Backing::ReadOnly(_) => Ok(()),
            Backing::Overlay { overlay, .. } => overlay.commit().map(|_| ()),
        }
    }

    /// Commit the overlay if the flush policy requests it.
    pub fn flush(&mut self) -> Result<(), UsbError> {
        match &mut self.backing {
            Backing::ReadOnly(_) => Ok(()),
            Backing::Overlay { overlay, policy } => {
                if policy.persist_on_flush {
                    overlay.commit()?;
                }
                Ok(())
            }
        }
    }

    /// Commit (if configured) when the LUN is stopped/unbound.
    pub fn stop(&mut self) -> Result<(), UsbError> {
        match &mut self.backing {
            Backing::ReadOnly(_) => Ok(()),
            Backing::Overlay { overlay, policy } => {
                if policy.persist_on_stop {
                    overlay.commit()?;
                }
                Ok(())
            }
        }
    }

    /// Execute one CBW and optional OUT data phase.
    ///
    /// A malformed CBW is a typed [`UsbError::Protocol`]; SCSI command failures
    /// are encoded as a check condition with sense data. Write data larger than
    /// [`SCSI_MAX_TRANSFER_BYTES`] or a length mismatch is a phase error.
    pub fn execute_cbw(
        &mut self,
        cbw_bytes: &[u8],
        data_out: &[u8],
    ) -> Result<ScsiReply, UsbError> {
        let command = ScsiCommandBlock::parse(cbw_bytes)?;
        if command.lun != 0 {
            return Ok(self.check(ScsiSense::invalid_field()));
        }
        let reply = match command.opcode {
            SCSI_TEST_UNIT_READY => ScsiReply::empty(),
            SCSI_REQUEST_SENSE => {
                let sense = self.last_sense.take().unwrap_or_else(ScsiSense::none);
                ScsiReply::good(sense.to_bytes().to_vec(), command.transfer_length)
            }
            SCSI_INQUIRY => self.inquiry(&command),
            SCSI_MODE_SENSE_6 => self.mode_sense_6(&command),
            SCSI_MODE_SENSE_10 => self.mode_sense_10(&command),
            SCSI_START_STOP_UNIT => ScsiReply::empty(),
            SCSI_PREVENT_ALLOW_MEDIUM_REMOVAL => ScsiReply::empty(),
            SCSI_READ_CAPACITY_10 => self.read_capacity(&command),
            SCSI_READ_FORMAT_CAPACITIES => self.read_format_capacities(&command),
            SCSI_READ_10 => self.read_10(&command)?,
            SCSI_WRITE_10 => self.write_10(&command, data_out)?,
            SCSI_SYNCHRONIZE_CACHE_10 => {
                self.flush()?;
                ScsiReply::empty()
            }
            _ => self.check(ScsiSense::invalid_command()),
        };
        if reply.status == ScsiStatus::CheckCondition {
            self.last_sense = reply.sense;
        }
        Ok(reply)
    }

    fn check(&mut self, sense: ScsiSense) -> ScsiReply {
        self.last_sense = Some(sense);
        ScsiReply::check(sense)
    }

    fn inquiry(&self, command: &ScsiCommandBlock) -> ScsiReply {
        let allocation = u16::from_be_bytes([command.cdb[3], command.cdb[4]]) as usize;
        let limit = if allocation == 0 {
            36
        } else {
            allocation.min(36)
        };
        let mut data = vec![0u8; 36];
        data[0] = 0x00; // direct-access block device
        data[1] = if self.is_read_only() { 0x80 } else { 0x00 };
        data[2] = 0x02; // SPC-2
        data[3] = 0x02; // response data format
        data[4] = 31; // additional length
        data[8..16].copy_from_slice(&self.vendor);
        data[16..32].copy_from_slice(&self.product);
        data[32..36].copy_from_slice(&self.revision);
        data.truncate(limit);
        ScsiReply::good(data, command.transfer_length)
    }

    fn mode_sense_6(&self, command: &ScsiCommandBlock) -> ScsiReply {
        let mut data = vec![0u8; 4];
        data[0] = 3;
        if self.is_read_only() {
            data[2] = 0x80;
        }
        ScsiReply::good(data, command.transfer_length)
    }

    fn mode_sense_10(&self, command: &ScsiCommandBlock) -> ScsiReply {
        let mut data = vec![0u8; 8];
        data[1] = 6;
        if self.is_read_only() {
            data[3] = 0x80;
        }
        ScsiReply::good(data, command.transfer_length)
    }

    fn read_capacity(&self, command: &ScsiCommandBlock) -> ScsiReply {
        let last_lba = self.sector_count().saturating_sub(1).min(u32::MAX as u64) as u32;
        let mut data = Vec::with_capacity(8);
        data.extend_from_slice(&last_lba.to_be_bytes());
        data.extend_from_slice(&(SCSI_SECTOR_BYTES as u32).to_be_bytes());
        ScsiReply::good(data, command.transfer_length)
    }

    fn read_format_capacities(&self, command: &ScsiCommandBlock) -> ScsiReply {
        let mut data = vec![0u8; 12];
        data[3] = 8; // capacity list length
        let blocks = self.sector_count().min(u32::MAX as u64) as u32;
        data[4..8].copy_from_slice(&blocks.to_be_bytes());
        data[8] = 0x02; // formatted media
        data[9..12].copy_from_slice(&(SCSI_SECTOR_BYTES as u32).to_be_bytes()[1..4]);
        ScsiReply::good(data, command.transfer_length)
    }

    fn read_10(&mut self, command: &ScsiCommandBlock) -> Result<ScsiReply, UsbError> {
        let (lba, count) = command.lba_and_count();
        if !command.direction_in {
            return Ok(ScsiReply::phase_error());
        }
        if count as usize * SCSI_SECTOR_BYTES > SCSI_MAX_TRANSFER_BYTES {
            return Ok(self.check(ScsiSense::invalid_field()));
        }
        if command.transfer_length as usize > SCSI_MAX_TRANSFER_BYTES {
            return Ok(self.check(ScsiSense::invalid_field()));
        }
        match self.read_sectors(u64::from(lba), count) {
            Ok(data) => Ok(ScsiReply::good(data, command.transfer_length)),
            Err(UsbError::OverlayBounds { .. }) => Ok(self.check(ScsiSense::lba_out_of_range())),
            Err(error) => Err(error),
        }
    }

    fn write_10(
        &mut self,
        command: &ScsiCommandBlock,
        data_out: &[u8],
    ) -> Result<ScsiReply, UsbError> {
        let (lba, count) = command.lba_and_count();
        let requested = count as usize * SCSI_SECTOR_BYTES;
        if command.direction_in {
            return Ok(ScsiReply::phase_error());
        }
        if requested > SCSI_MAX_TRANSFER_BYTES
            || command.transfer_length as usize > SCSI_MAX_TRANSFER_BYTES
        {
            return Ok(self.check(ScsiSense::invalid_field()));
        }
        if data_out.len() != requested || data_out.len() != command.transfer_length as usize {
            return Ok(ScsiReply::phase_error());
        }
        if self.is_read_only() {
            return Ok(self.check(ScsiSense::write_protected()));
        }
        let offset = u64::from(lba) * SCSI_SECTOR_BYTES as u64;
        if offset
            .checked_add(requested as u64)
            .is_none_or(|end| end > self.base_len())
        {
            return Ok(self.check(ScsiSense::lba_out_of_range()));
        }
        match self.write_sectors(u64::from(lba), data_out) {
            Ok(()) => Ok(ScsiReply::empty()),
            Err(UsbError::OverlayBounds { .. }) => Ok(self.check(ScsiSense::write_error())),
            Err(UsbError::ReadOnlyMedia(_)) => Ok(self.check(ScsiSense::write_protected())),
            Err(_) => Ok(self.check(ScsiSense::write_error())),
        }
    }
}

fn fixed_ascii<const N: usize>(value: &str) -> [u8; N] {
    let mut bytes = [b' '; N];
    for (slot, byte) in bytes.iter_mut().zip(value.as_bytes()) {
        *slot = if byte.is_ascii_graphic() { *byte } else { b' ' };
    }
    bytes
}

fn validate_image_len(
    length: u64,
    max_bytes: u64,
    format: &'static str,
    path: &Path,
) -> Result<(), UsbError> {
    if length == 0 {
        return Err(UsbError::InvalidMedia {
            format,
            path: path.to_path_buf(),
            reason: "media image must not be empty".to_owned(),
        });
    }
    if length % SCSI_SECTOR_BYTES as u64 != 0 {
        return Err(UsbError::InvalidMedia {
            format,
            path: path.to_path_buf(),
            reason: format!("media image length {length} is not a multiple of {SCSI_SECTOR_BYTES}"),
        });
    }
    if length > max_bytes {
        return Err(UsbError::InvalidMedia {
            format,
            path: path.to_path_buf(),
            reason: format!("media image is {length} bytes; the configured bound is {max_bytes}"),
        });
    }
    Ok(())
}

fn checked_transfer_len(count: u16) -> Result<usize, UsbError> {
    let length = count as usize * SCSI_SECTOR_BYTES;
    if length > SCSI_MAX_TRANSFER_BYTES {
        return Err(bounds(format!(
            "transfer of {length} bytes exceeds the {SCSI_MAX_TRANSFER_BYTES}-byte limit"
        )));
    }
    Ok(length)
}

fn check_range(offset: u64, length: usize, base_len: u64) -> Result<(), UsbError> {
    let end = offset
        .checked_add(length as u64)
        .ok_or_else(|| bounds("range overflows"))?;
    if end > base_len {
        return Err(bounds(format!(
            "range [{offset}, {end}) exceeds media length {base_len}"
        )));
    }
    Ok(())
}

fn bounds(reason: impl Into<String>) -> UsbError {
    UsbError::OverlayBounds {
        reason: reason.into(),
    }
}
