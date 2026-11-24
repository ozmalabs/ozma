// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded copy-on-write write overlay for FAT32 or raw images.
//!
//! The overlay keeps the base image immutable and records only the blocks a
//! guest has written. A read returns the overlay block when present and the
//! base bytes otherwise; a write copies the touched base block into the overlay
//! first (copy-on-write), applies the patch in memory, and never opens the base
//! for writing. There are no mounts, block devices, subprocesses, or `unsafe`
//! operations in this module.
//!
//! Allocation is explicit and bounded:
//!
//! * the first write to a block allocates that block (a full block except for
//!   the final partial block), and later writes to the same block are free;
//! * a write is rejected if it would exceed [`OverlayConfig::max_overlay_bytes`]
//!   or extend past the base image, and rejected writes do not partially
//!   allocate;
//! * [`WriteOverlay::commit_to`] writes a checksummed persistence file
//!   atomically via a sibling temporary file and `rename`.
//!
//! # Persistence file
//!
//! The file is little-endian and starts with a 32-byte header:
//!
//! ```text
//! offset  size  field
//! 0       4     magic "OZOV"
//! 4       2     version (1)
//! 6       2     header length (32)
//! 8       4     overlay block size
//! 12      8     base image length
//! 20      4     number of allocated blocks
//! 24      4     FNV-1a checksum of the 32-byte header with this field zeroed
//! 28      4     FNV-1a checksum of the base image bytes
//! ```
//!
//! Each block follows the header, sorted by block index:
//!
//! ```text
//! 8  block index
//! 4  block length (the final block may be shorter than the block size)
//! 4  FNV-1a checksum of the block bytes
//! N  block bytes
//! ```
//!
//! An overlay is rejected as [`UsbError::OverlayCorrupt`] when the magic,
//! version, header length/checksum, block size, base length/checksum, block
//! count, block bounds, block length, duplicate index, per-block checksum, or
//! trailing bytes are inconsistent. The base image is never included in the
//! file, so a persistence file is only valid with the base it was created
//! against; a same-length but different base is rejected by the base checksum.

use std::collections::BTreeMap;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::error::{io_error, UsbError};
use crate::fat32::{Fat32Image, FAT32_SECTOR_SIZE};

/// Overlay persistence magic.
pub const OVERLAY_MAGIC: [u8; 4] = *b"OZOV";
/// Current persistence format version.
pub const OVERLAY_VERSION: u16 = 1;
/// Persistence header size in bytes.
pub const OVERLAY_HEADER_BYTES: usize = 32;
/// Default overlay block size (one 4 KiB page).
pub const OVERLAY_DEFAULT_BLOCK_SIZE: usize = 4096;
/// Smallest accepted overlay block size (one FAT sector).
pub const OVERLAY_MIN_BLOCK_SIZE: usize = FAT32_SECTOR_SIZE;
/// Largest accepted overlay block size.
pub const OVERLAY_MAX_BLOCK_SIZE: usize = 64 * 1024;
/// Default allocation ceiling for overlay data (64 MiB).
pub const OVERLAY_DEFAULT_MAX_BYTES: u64 = 64 * 1024 * 1024;
/// Default base-image read bound (256 MiB).
pub const OVERLAY_DEFAULT_MAX_BASE_BYTES: u64 = 256 * 1024 * 1024;

const FNV_OFFSET_BASIS: u32 = 0x811c_9dc5;
const FNV_PRIME: u32 = 0x0100_0193;

/// Overlay geometry, allocation ceiling, and optional persistence path.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayConfig {
    /// Copy-on-write block size in bytes (power of two, 512..=65536).
    pub block_size: usize,
    /// Maximum bytes the overlay may allocate for changed blocks.
    pub max_overlay_bytes: u64,
    /// Maximum base image size accepted by the constructors.
    pub max_base_bytes: u64,
    /// Default persistence path used by [`WriteOverlay::commit`] and
    /// [`WriteOverlay::reload`].
    pub persistence: Option<PathBuf>,
}

impl Default for OverlayConfig {
    fn default() -> Self {
        Self {
            block_size: OVERLAY_DEFAULT_BLOCK_SIZE,
            max_overlay_bytes: OVERLAY_DEFAULT_MAX_BYTES,
            max_base_bytes: OVERLAY_DEFAULT_MAX_BASE_BYTES,
            persistence: None,
        }
    }
}

impl OverlayConfig {
    /// A configuration with a block size and allocation ceiling.
    pub fn new(block_size: usize, max_overlay_bytes: u64) -> Self {
        Self {
            block_size,
            max_overlay_bytes,
            ..Self::default()
        }
    }

    /// Use `path` for [`WriteOverlay::commit`] and [`WriteOverlay::reload`].
    pub fn with_persistence(mut self, path: impl Into<PathBuf>) -> Self {
        self.persistence = Some(path.into());
        self
    }

    /// Validate block geometry and bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.block_size < OVERLAY_MIN_BLOCK_SIZE
            || self.block_size > OVERLAY_MAX_BLOCK_SIZE
            || !self.block_size.is_power_of_two()
        {
            return Err(UsbError::InvalidField {
                field: "overlay.block_size",
                reason: format!(
                    "block size must be a power of two from {OVERLAY_MIN_BLOCK_SIZE} to \
                     {OVERLAY_MAX_BLOCK_SIZE} bytes"
                ),
            });
        }
        if self.max_overlay_bytes == 0 {
            return Err(UsbError::InvalidField {
                field: "overlay.max_overlay_bytes",
                reason: "allocation ceiling must be non-zero".to_owned(),
            });
        }
        if self.max_base_bytes == 0 {
            return Err(UsbError::InvalidField {
                field: "overlay.max_base_bytes",
                reason: "base image bound must be non-zero".to_owned(),
            });
        }
        Ok(())
    }
}

/// Result of one copy-on-write write.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OverlayWriteReport {
    /// Absolute byte offset of the write.
    pub offset: u64,
    /// Number of bytes copied into the overlay.
    pub bytes_written: usize,
    /// Number of distinct blocks touched by this write.
    pub blocks_touched: usize,
    /// Bytes allocated by this write (zero for already-overlaid blocks).
    pub newly_allocated_bytes: u64,
    /// Total overlay allocation after the write.
    pub allocated_bytes: u64,
}

/// A bounded copy-on-write overlay over an immutable base image.
#[derive(Clone, Debug)]
pub struct WriteOverlay {
    base: Arc<[u8]>,
    block_size: usize,
    max_overlay_bytes: u64,
    persistence: Option<PathBuf>,
    blocks: BTreeMap<u64, Vec<u8>>,
    allocated_bytes: u64,
    dirty: bool,
}

impl WriteOverlay {
    /// Wrap an owned base image.
    ///
    /// The base bytes are moved into an immutable shared buffer. The caller
    /// cannot mutate them through this type.
    pub fn from_bytes(base: impl Into<Vec<u8>>, config: OverlayConfig) -> Result<Self, UsbError> {
        config.validate()?;
        let base = base.into();
        let length = base.len() as u64;
        if length == 0 {
            return Err(UsbError::InvalidField {
                field: "overlay.base",
                reason: "base image must not be empty".to_owned(),
            });
        }
        if length > config.max_base_bytes {
            return Err(UsbError::InvalidField {
                field: "overlay.base",
                reason: format!(
                    "base image is {length} bytes; the configured bound is {} bytes",
                    config.max_base_bytes
                ),
            });
        }
        Ok(Self {
            base: Arc::from(base),
            block_size: config.block_size,
            max_overlay_bytes: config.max_overlay_bytes,
            persistence: config.persistence,
            blocks: BTreeMap::new(),
            allocated_bytes: 0,
            dirty: false,
        })
    }

    /// Wrap a FAT32 image built or parsed by [`crate::fat32`].
    pub fn from_fat32(image: &Fat32Image, config: OverlayConfig) -> Result<Self, UsbError> {
        Self::from_bytes(image.as_bytes().to_vec(), config)
    }

    /// Read an existing regular file as the base image.
    ///
    /// The file is opened read-only and read exactly once into memory; it is
    /// never written or memory-mapped.
    pub fn from_file(path: impl AsRef<Path>, config: OverlayConfig) -> Result<Self, UsbError> {
        config.validate()?;
        let path = path.as_ref();
        let metadata = fs::metadata(path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                UsbError::MediaNotFound(path.to_path_buf())
            } else {
                io_error("stat overlay base", path.to_path_buf(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(path.to_path_buf()));
        }
        if metadata.len() > config.max_base_bytes {
            return Err(UsbError::InvalidField {
                field: "overlay.base",
                reason: format!(
                    "base image {} is {} bytes; the configured bound is {} bytes",
                    path.display(),
                    metadata.len(),
                    config.max_base_bytes
                ),
            });
        }
        let mut file = File::open(path)
            .map_err(|source| io_error("open overlay base", path.to_path_buf(), source))?;
        let mut base = Vec::with_capacity(metadata.len() as usize);
        file.read_to_end(&mut base)
            .map_err(|source| io_error("read overlay base", path.to_path_buf(), source))?;
        Self::from_bytes(base, config)
    }

    /// Load a base file plus an existing persistence file.
    pub fn load_from_file(
        base: impl AsRef<Path>,
        config: OverlayConfig,
        persistence: impl AsRef<Path>,
    ) -> Result<Self, UsbError> {
        let mut overlay = Self::from_file(base, config)?;
        overlay.load_from(persistence)?;
        Ok(overlay)
    }

    /// Base image length in bytes.
    pub fn base_len(&self) -> u64 {
        self.base.len() as u64
    }

    /// Immutable base bytes (for diagnostics and immutability tests).
    pub fn base_bytes(&self) -> &[u8] {
        &self.base
    }

    /// Copy-on-write block size.
    pub fn block_size(&self) -> usize {
        self.block_size
    }

    /// Configured allocation ceiling.
    pub fn max_overlay_bytes(&self) -> u64 {
        self.max_overlay_bytes
    }

    /// Bytes currently allocated in the overlay.
    pub fn allocated_bytes(&self) -> u64 {
        self.allocated_bytes
    }

    /// Number of allocated overlay blocks.
    pub fn allocated_blocks(&self) -> usize {
        self.blocks.len()
    }

    /// Whether writes have not been committed since construction/load/commit.
    pub fn is_dirty(&self) -> bool {
        self.dirty
    }

    /// Configured default persistence path.
    pub fn persistence_path(&self) -> Option<&Path> {
        self.persistence.as_deref()
    }

    /// Read a byte range, preferring overlay blocks over base bytes.
    pub fn read_at(&self, offset: u64, length: usize) -> Result<Vec<u8>, UsbError> {
        let mut buffer = vec![0u8; length];
        self.read_into(offset, &mut buffer)?;
        Ok(buffer)
    }

    /// Read into an existing buffer.
    pub fn read_into(&self, offset: u64, buffer: &mut [u8]) -> Result<(), UsbError> {
        let end = offset
            .checked_add(buffer.len() as u64)
            .ok_or_else(|| bounds_error("read range overflows"))?;
        if end > self.base_len() {
            return Err(bounds_error(format!(
                "read [{offset}, {end}) exceeds base length {}",
                self.base_len()
            )));
        }
        if buffer.is_empty() {
            return Ok(());
        }

        let block_size = self.block_size as u64;
        let mut cursor = offset;
        let mut written = 0usize;
        while cursor < end {
            let block = cursor / block_size;
            let block_start = block * block_size;
            let block_len = self.block_len(block);
            let available = (block_start + block_len as u64 - cursor) as usize;
            let chunk = available.min(buffer.len() - written);
            let block_offset = (cursor - block_start) as usize;
            match self.blocks.get(&block) {
                Some(data) => buffer[written..written + chunk]
                    .copy_from_slice(&data[block_offset..block_offset + chunk]),
                None => {
                    let start = cursor as usize;
                    buffer[written..written + chunk]
                        .copy_from_slice(&self.base[start..start + chunk]);
                }
            }
            cursor += chunk as u64;
            written += chunk;
        }
        Ok(())
    }

    /// Read whole 512-byte FAT sectors.
    pub fn read_sectors(&self, sector: u64, count: usize) -> Result<Vec<u8>, UsbError> {
        let offset = sector
            .checked_mul(FAT32_SECTOR_SIZE as u64)
            .ok_or_else(|| bounds_error("sector offset overflows"))?;
        let length = count
            .checked_mul(FAT32_SECTOR_SIZE)
            .ok_or_else(|| bounds_error("sector length overflows"))?;
        self.read_at(offset, length)
    }

    /// Apply a copy-on-write write.
    ///
    /// Allocation is preflighted before any block is copied, so a rejected
    /// write leaves the overlay unchanged.
    pub fn write_at(&mut self, offset: u64, data: &[u8]) -> Result<OverlayWriteReport, UsbError> {
        let end = offset
            .checked_add(data.len() as u64)
            .ok_or_else(|| bounds_error("write range overflows"))?;
        if end > self.base_len() {
            return Err(bounds_error(format!(
                "write [{offset}, {end}) exceeds base length {}",
                self.base_len()
            )));
        }
        if data.is_empty() {
            return Ok(OverlayWriteReport {
                offset,
                bytes_written: 0,
                blocks_touched: 0,
                newly_allocated_bytes: 0,
                allocated_bytes: self.allocated_bytes,
            });
        }

        let block_size = self.block_size as u64;
        let first = offset / block_size;
        let last = (end - 1) / block_size;
        let mut newly_allocated = 0u64;
        let mut blocks_touched = 0usize;
        let mut block = first;
        while block <= last {
            blocks_touched += 1;
            if !self.blocks.contains_key(&block) {
                newly_allocated += self.block_len(block) as u64;
            }
            block += 1;
        }
        if self.allocated_bytes + newly_allocated > self.max_overlay_bytes {
            return Err(bounds_error(format!(
                "write needs {newly_allocated} new overlay bytes; {} of {} are already used",
                self.allocated_bytes, self.max_overlay_bytes
            )));
        }

        let mut block = first;
        while block <= last {
            let block_start = block * block_size;
            let block_len = self.block_len(block);
            let block_end = block_start + block_len as u64;
            let write_start = offset.max(block_start);
            let write_end = end.min(block_end);
            let source_start = (write_start - offset) as usize;
            let source_end = (write_end - offset) as usize;
            let destination_start = (write_start - block_start) as usize;
            let destination_end = (write_end - block_start) as usize;

            if !self.blocks.contains_key(&block) {
                let start = block_start as usize;
                let base_block = self.base[start..start + block_len].to_vec();
                self.blocks.insert(block, base_block);
                self.allocated_bytes += block_len as u64;
            }
            if let Some(block_data) = self.blocks.get_mut(&block) {
                block_data[destination_start..destination_end]
                    .copy_from_slice(&data[source_start..source_end]);
            }
            block += 1;
        }
        self.dirty = true;
        Ok(OverlayWriteReport {
            offset,
            bytes_written: data.len(),
            blocks_touched,
            newly_allocated_bytes: newly_allocated,
            allocated_bytes: self.allocated_bytes,
        })
    }

    /// Write whole 512-byte FAT sectors.
    pub fn write_sectors(
        &mut self,
        sector: u64,
        data: &[u8],
    ) -> Result<OverlayWriteReport, UsbError> {
        if data.len() % FAT32_SECTOR_SIZE != 0 {
            return Err(UsbError::InvalidField {
                field: "overlay.write_sectors",
                reason: "sector payload must be a multiple of 512 bytes".to_owned(),
            });
        }
        let offset = sector
            .checked_mul(FAT32_SECTOR_SIZE as u64)
            .ok_or_else(|| bounds_error("sector offset overflows"))?;
        self.write_at(offset, data)
    }

    /// Materialise the current base-plus-overlay bytes without changing either.
    pub fn materialize(&self) -> Vec<u8> {
        let mut bytes = self.base.to_vec();
        for (index, data) in &self.blocks {
            let start = (*index * self.block_size as u64) as usize;
            bytes[start..start + data.len()].copy_from_slice(data);
        }
        bytes
    }

    /// Discard every overlay block, returning to the immutable base image.
    pub fn discard_all(&mut self) {
        self.blocks.clear();
        self.allocated_bytes = 0;
        self.dirty = false;
    }

    /// Commit to the configured persistence path.
    pub fn commit(&mut self) -> Result<PathBuf, UsbError> {
        let Some(path) = self.persistence.clone() else {
            return Err(UsbError::InvalidField {
                field: "overlay.persistence",
                reason: "no persistence path was configured".to_owned(),
            });
        };
        self.commit_to(&path)?;
        Ok(path)
    }

    /// Atomically write the overlay to `path`.
    ///
    /// A sibling temporary file is written, synced, and renamed over the target.
    /// The base image is not included. On failure the temporary file is removed
    /// and the previous persistence file (if any) is left untouched.
    pub fn commit_to(&mut self, path: impl AsRef<Path>) -> Result<(), UsbError> {
        let path = path.as_ref();
        let bytes = self.encode()?;
        let temporary = temporary_path(path);
        let result = (|| -> Result<(), UsbError> {
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&temporary)
                .map_err(|source| {
                    io_error("create overlay persistence", temporary.clone(), source)
                })?;
            file.write_all(&bytes).map_err(|source| {
                io_error("write overlay persistence", temporary.clone(), source)
            })?;
            file.sync_all().map_err(|source| {
                io_error("sync overlay persistence", temporary.clone(), source)
            })?;
            drop(file);
            fs::rename(&temporary, path).map_err(|source| {
                io_error("rename overlay persistence", path.to_path_buf(), source)
            })
        })();
        if result.is_err() {
            let _ = fs::remove_file(&temporary);
        }
        result?;
        self.persistence = Some(path.to_path_buf());
        self.dirty = false;
        Ok(())
    }

    /// Reload blocks from the configured persistence path.
    pub fn reload(&mut self) -> Result<(), UsbError> {
        let Some(path) = self.persistence.clone() else {
            return Err(UsbError::InvalidField {
                field: "overlay.persistence",
                reason: "no persistence path was configured".to_owned(),
            });
        };
        self.load_from(path)
    }

    /// Replace all overlay blocks with those in `path`.
    ///
    /// Loading validates the header, base length, block geometry, every block
    /// checksum, and the absence of duplicate/trailing records before changing
    /// the in-memory overlay.
    pub fn load_from(&mut self, path: impl AsRef<Path>) -> Result<(), UsbError> {
        let path = path.as_ref();
        let persistence = self.read_persistence(path)?;
        self.blocks = persistence;
        self.allocated_bytes = self.blocks.values().map(|block| block.len() as u64).sum();
        self.persistence = Some(path.to_path_buf());
        self.dirty = false;
        Ok(())
    }

    fn block_len(&self, block: u64) -> usize {
        let block_size = self.block_size as u64;
        let start = block.saturating_mul(block_size);
        let remaining = self.base_len().saturating_sub(start);
        remaining.min(block_size) as usize
    }

    fn encode(&self) -> Result<Vec<u8>, UsbError> {
        let block_count =
            u32::try_from(self.blocks.len()).map_err(|_| UsbError::OverlayBounds {
                reason: "too many overlay blocks for the persistence format".to_owned(),
            })?;
        let capacity = OVERLAY_HEADER_BYTES
            .checked_add((self.blocks.len() * 16).saturating_add(self.allocated_bytes as usize))
            .ok_or_else(|| UsbError::OverlayBounds {
                reason: "overlay persistence size overflows".to_owned(),
            })?;
        let mut bytes = Vec::with_capacity(capacity);
        bytes.extend_from_slice(&OVERLAY_MAGIC);
        bytes.extend_from_slice(&OVERLAY_VERSION.to_le_bytes());
        bytes.extend_from_slice(&(OVERLAY_HEADER_BYTES as u16).to_le_bytes());
        bytes.extend_from_slice(&(self.block_size as u32).to_le_bytes());
        bytes.extend_from_slice(&self.base_len().to_le_bytes());
        bytes.extend_from_slice(&block_count.to_le_bytes());
        bytes.extend_from_slice(&0u32.to_le_bytes()); // checksum placeholder
        bytes.extend_from_slice(&fnv1a(&self.base).to_le_bytes());
        debug_assert_eq!(bytes.len(), OVERLAY_HEADER_BYTES);
        let checksum = fnv1a(&bytes);
        bytes[24..28].copy_from_slice(&checksum.to_le_bytes());

        for (index, data) in &self.blocks {
            let length = u32::try_from(data.len()).map_err(|_| UsbError::OverlayBounds {
                reason: "overlay block length overflows the persistence format".to_owned(),
            })?;
            bytes.extend_from_slice(&index.to_le_bytes());
            bytes.extend_from_slice(&length.to_le_bytes());
            bytes.extend_from_slice(&fnv1a(data).to_le_bytes());
            bytes.extend_from_slice(data);
        }
        Ok(bytes)
    }

    fn read_persistence(&self, path: &Path) -> Result<BTreeMap<u64, Vec<u8>>, UsbError> {
        let metadata = fs::metadata(path).map_err(|source| {
            if source.kind() == io::ErrorKind::NotFound {
                UsbError::MediaNotFound(path.to_path_buf())
            } else {
                io_error("stat overlay persistence", path.to_path_buf(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(path.to_path_buf()));
        }
        let max_blocks = self.base_len().div_ceil(self.block_size as u64);
        let overhead = OVERLAY_HEADER_BYTES as u64 + max_blocks.saturating_mul(16);
        let allowed = self
            .max_overlay_bytes
            .saturating_add(overhead)
            .saturating_add(fnv_overhead_padding());
        if metadata.len() > allowed {
            return Err(corrupt(
                path,
                "persistence file exceeds the configured bounds",
            ));
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        File::open(path)
            .and_then(|mut file| file.read_to_end(&mut bytes))
            .map_err(|source| io_error("read overlay persistence", path.to_path_buf(), source))?;
        self.parse_persistence(path, &bytes)
    }

    fn parse_persistence(
        &self,
        path: &Path,
        bytes: &[u8],
    ) -> Result<BTreeMap<u64, Vec<u8>>, UsbError> {
        if bytes.len() < OVERLAY_HEADER_BYTES {
            return Err(corrupt(path, "header is truncated"));
        }
        if bytes[0..4] != OVERLAY_MAGIC {
            return Err(corrupt(path, "bad magic"));
        }
        let version = u16::from_le_bytes([bytes[4], bytes[5]]);
        if version != OVERLAY_VERSION {
            return Err(corrupt(path, format!("unsupported version {version}")));
        }
        let header_len = u16::from_le_bytes([bytes[6], bytes[7]]) as usize;
        if header_len != OVERLAY_HEADER_BYTES {
            return Err(corrupt(path, "bad header length"));
        }
        let mut header = [0u8; OVERLAY_HEADER_BYTES];
        header.copy_from_slice(&bytes[..OVERLAY_HEADER_BYTES]);
        let stored_checksum = u32::from_le_bytes([header[24], header[25], header[26], header[27]]);
        header[24..28].fill(0);
        if fnv1a(&header) != stored_checksum {
            return Err(corrupt(path, "header checksum mismatch"));
        }
        let base_checksum = u32::from_le_bytes([header[28], header[29], header[30], header[31]]);
        if base_checksum != fnv1a(&self.base) {
            return Err(corrupt(
                path,
                "base checksum mismatch (the persistence file belongs to a different base)",
            ));
        }
        let block_size =
            u32::from_le_bytes([header[8], header[9], header[10], header[11]]) as usize;
        if block_size != self.block_size {
            return Err(corrupt(
                path,
                format!(
                    "block size {block_size} does not match overlay block size {}",
                    self.block_size
                ),
            ));
        }
        let base_len = u64::from_le_bytes([
            header[12], header[13], header[14], header[15], header[16], header[17], header[18],
            header[19],
        ]);
        if base_len != self.base_len() {
            return Err(corrupt(
                path,
                format!(
                    "persistence base length {base_len} does not match base length {}",
                    self.base_len()
                ),
            ));
        }
        let block_count =
            u32::from_le_bytes([header[20], header[21], header[22], header[23]]) as u64;
        let max_blocks = self.base_len().div_ceil(self.block_size as u64);
        if block_count > max_blocks {
            return Err(corrupt(path, "block count exceeds the base image"));
        }

        let mut cursor = OVERLAY_HEADER_BYTES;
        let mut blocks = BTreeMap::new();
        let mut allocated = 0u64;
        for _ in 0..block_count {
            if bytes.len().saturating_sub(cursor) < 16 {
                return Err(corrupt(path, "block header is truncated"));
            }
            let index = u64::from_le_bytes(bytes[cursor..cursor + 8].try_into().unwrap());
            let length =
                u32::from_le_bytes(bytes[cursor + 8..cursor + 12].try_into().unwrap()) as usize;
            let checksum = u32::from_le_bytes(bytes[cursor + 12..cursor + 16].try_into().unwrap());
            cursor += 16;
            if index >= max_blocks {
                return Err(corrupt(path, "block index is outside the base image"));
            }
            let expected = self.block_len(index);
            if length != expected {
                return Err(corrupt(
                    path,
                    format!("block {index} length {length} does not match expected {expected}"),
                ));
            }
            let end = cursor
                .checked_add(length)
                .ok_or_else(|| corrupt(path, "block range overflows"))?;
            let data = bytes
                .get(cursor..end)
                .ok_or_else(|| corrupt(path, "block data is truncated"))?;
            if fnv1a(data) != checksum {
                return Err(corrupt(path, format!("block {index} checksum mismatch")));
            }
            if blocks.insert(index, data.to_vec()).is_some() {
                return Err(corrupt(path, format!("duplicate block {index}")));
            }
            allocated = allocated.saturating_add(length as u64);
            if allocated > self.max_overlay_bytes {
                return Err(corrupt(path, "persistence exceeds the allocation ceiling"));
            }
            cursor = end;
        }
        if cursor != bytes.len() {
            return Err(corrupt(path, "trailing bytes after the final block"));
        }
        Ok(blocks)
    }
}

fn fnv1a(bytes: &[u8]) -> u32 {
    let mut hash = FNV_OFFSET_BASIS;
    for byte in bytes {
        hash ^= u32::from(*byte);
        hash = hash.wrapping_mul(FNV_PRIME);
    }
    hash
}

fn fnv_overhead_padding() -> u64 {
    // Allow a full block of slack for headers and a final partial block.
    OVERLAY_HEADER_BYTES as u64 + OVERLAY_MAX_BLOCK_SIZE as u64
}

fn temporary_path(path: &Path) -> PathBuf {
    let mut name = path
        .file_name()
        .map(|name| name.to_os_string())
        .unwrap_or_else(|| "overlay".into());
    name.push(format!(".tmp-{}", std::process::id()));
    path.with_file_name(name)
}

fn bounds_error(reason: impl Into<String>) -> UsbError {
    UsbError::OverlayBounds {
        reason: reason.into(),
    }
}

fn corrupt(path: &Path, reason: impl Into<String>) -> UsbError {
    UsbError::OverlayCorrupt {
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_base() -> Vec<u8> {
        (0..(OVERLAY_DEFAULT_BLOCK_SIZE * 2) as u32)
            .map(|value| (value % 251) as u8)
            .collect()
    }

    #[test]
    fn persistence_round_trip_in_memory() {
        let base = sample_base();
        let config = OverlayConfig::new(OVERLAY_DEFAULT_BLOCK_SIZE, 1024 * 1024);
        let mut overlay = WriteOverlay::from_bytes(base.clone(), config).unwrap();
        overlay.write_at(10, b"hello").unwrap();
        let temp = tempfile::tempdir().unwrap();
        let path = temp.path().join("overlay.bin");
        overlay.commit_to(&path).unwrap();
        let mut reloaded = WriteOverlay::from_bytes(
            base,
            OverlayConfig::new(OVERLAY_DEFAULT_BLOCK_SIZE, 1024 * 1024),
        )
        .unwrap();
        reloaded.load_from(&path).unwrap();
        assert_eq!(reloaded.read_at(10, 5).unwrap(), b"hello");
        assert_eq!(
            reloaded.allocated_bytes(),
            OVERLAY_DEFAULT_BLOCK_SIZE as u64
        );
    }
}
