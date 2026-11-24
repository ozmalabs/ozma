// SPDX-License-Identifier: AGPL-3.0-only
//! Read-only ISO 9660 media inspection.
//!
//! This module only reads existing ISO images. It never mounts a filesystem,
//! writes an image, creates a loop device, or parses directory trees beyond
//! the primary volume descriptor needed for validation. ISO media is always
//! presented read-only by [`IsoImage`] and by the corresponding
//! [`MassStorageImage`](crate::media::MassStorageImage) constructors.

use std::fs::{self, File};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

use crate::error::UsbError;

/// ISO 9660 logical sector size.
pub const ISO9660_SECTOR_SIZE: u64 = 2048;
/// Sector containing the first volume descriptor.
pub const ISO9660_FIRST_DESCRIPTOR_SECTOR: u64 = 16;
/// Volume descriptor identifier.
pub const ISO9660_IDENTIFIER: &[u8; 5] = b"CD001";
/// Primary volume descriptor type.
pub const ISO9660_PRIMARY: u8 = 1;
/// Volume descriptor set terminator type.
pub const ISO9660_TERMINATOR: u8 = 255;
/// Maximum number of volume descriptors inspected.
pub const ISO9660_MAX_DESCRIPTORS: u32 = 32;

/// Fields extracted from the ISO 9660 primary volume descriptor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsoVolumeDescriptor {
    /// Volume identifier (trimmed).
    pub volume_id: String,
    /// System identifier (trimmed).
    pub system_id: String,
    /// Logical block size in bytes.
    pub block_size: u32,
    /// Volume size in logical blocks.
    pub block_count: u32,
    /// Root directory extent (logical block).
    pub root_extent: u32,
    /// Root directory data length in bytes.
    pub root_size: u32,
    /// Volume sequence number.
    pub volume_sequence_number: u16,
}

/// A read-only ISO image inspected from disk.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct IsoImage {
    /// Host path.
    pub path: PathBuf,
    /// File size in bytes.
    pub size_bytes: u64,
    /// ISO media is always read-only.
    pub read_only: bool,
    /// Primary volume descriptor.
    pub volume: IsoVolumeDescriptor,
}

impl IsoImage {
    /// Inspect an existing ISO file without mounting it.
    pub fn inspect(path: impl Into<PathBuf>) -> Result<Self, UsbError> {
        let path = path.into();
        let metadata = fs::metadata(&path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                UsbError::MediaNotFound(path.clone())
            } else {
                crate::error::io_error("stat ISO image", path.clone(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(path.clone()));
        }
        if metadata.len() == 0 {
            return Err(invalid_iso(&path, "ISO image is empty"));
        }

        let start = ISO9660_FIRST_DESCRIPTOR_SECTOR * ISO9660_SECTOR_SIZE;
        let wanted = u64::from(ISO9660_MAX_DESCRIPTORS) * ISO9660_SECTOR_SIZE;
        let length = metadata.len().saturating_sub(start).min(wanted);
        let mut bytes = vec![0u8; usize::try_from(length).unwrap_or(0)];
        if !bytes.is_empty() {
            let mut file = File::open(&path)
                .map_err(|source| crate::error::io_error("open ISO image", path.clone(), source))?;
            file.seek(SeekFrom::Start(start))
                .map_err(|source| crate::error::io_error("seek ISO image", path.clone(), source))?;
            file.read_exact(&mut bytes)
                .map_err(|source| crate::error::io_error("read ISO image", path.clone(), source))?;
        }
        let volume =
            parse_iso_volume_descriptors(&bytes).map_err(|reason| invalid_iso(&path, reason))?;
        Ok(Self {
            path,
            size_bytes: metadata.len(),
            read_only: true,
            volume,
        })
    }

    /// A read-only [`MassStorageImage`](crate::media::MassStorageImage) for
    /// this ISO.
    pub fn media_image(&self) -> crate::media::MassStorageImage {
        crate::media::MassStorageImage::iso(&self.path)
            .with_size(self.size_bytes)
            .unwrap_or_else(|_| crate::media::MassStorageImage::iso(&self.path))
    }

    /// Re-validate the primary volume descriptor bounds.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.volume.block_size < 512
            || self.volume.block_size > 4096
            || !self.volume.block_size.is_power_of_two()
        {
            return Err(invalid_iso(&self.path, "invalid logical block size"));
        }
        if self.volume.block_count == 0 {
            return Err(invalid_iso(&self.path, "volume declares zero blocks"));
        }
        let declared = u64::from(self.volume.block_count) * u64::from(self.volume.block_size);
        if declared > self.size_bytes {
            return Err(invalid_iso(
                &self.path,
                "volume descriptor is larger than the backing file",
            ));
        }
        Ok(())
    }
}

/// Parse volume descriptors from a byte slice that starts at sector 16.
///
/// The slice may contain only the descriptor area; no directory tree or file
/// content is read.
pub fn parse_iso_volume_descriptors(bytes: &[u8]) -> Result<IsoVolumeDescriptor, String> {
    let sector = ISO9660_SECTOR_SIZE as usize;
    let mut offset = 0usize;
    let mut found = None;
    let mut descriptors = 0u32;
    while offset + sector <= bytes.len() && descriptors < ISO9660_MAX_DESCRIPTORS {
        let descriptor = &bytes[offset..offset + sector];
        if &descriptor[1..6] != ISO9660_IDENTIFIER {
            if descriptors == 0 {
                return Err("missing CD001 volume descriptor identifier".to_owned());
            }
            break;
        }
        if descriptor[6] != 1 {
            return Err("unsupported ISO 9660 descriptor version".to_owned());
        }
        match descriptor[0] {
            ISO9660_PRIMARY => {
                if found.is_none() {
                    found = Some(parse_primary_descriptor(descriptor)?);
                }
            }
            ISO9660_TERMINATOR => break,
            _ => {}
        }
        descriptors += 1;
        offset += sector;
    }
    found.ok_or_else(|| "no ISO 9660 primary volume descriptor found".to_owned())
}

/// Inspect an existing ISO file (read-only).
pub fn validate_iso_image(path: impl AsRef<Path>) -> Result<IsoImage, UsbError> {
    IsoImage::inspect(path.as_ref().to_path_buf())
}

fn parse_primary_descriptor(descriptor: &[u8]) -> Result<IsoVolumeDescriptor, String> {
    if descriptor.len() < 2048 {
        return Err("truncated primary volume descriptor".to_owned());
    }
    let block_count = both_endian_u32(descriptor, 80, "volume space size")?;
    let block_size = both_endian_u32(descriptor, 128, "logical block size")?;
    if block_size < 512 || block_size > 4096 || !block_size.is_power_of_two() {
        return Err(format!("invalid logical block size {block_size}"));
    }
    if block_count == 0 {
        return Err("volume space size must be non-zero".to_owned());
    }
    let volume_sequence_number = both_endian_u16(descriptor, 124, "volume sequence number")?;

    let root = descriptor
        .get(156..156 + 34)
        .ok_or_else(|| "missing root directory record".to_owned())?;
    if root[0] == 0 {
        return Err("root directory record has no extent".to_owned());
    }
    let root_extent = u32::from_le_bytes([root[2], root[3], root[4], root[5]]);
    let root_size = u32::from_le_bytes([root[10], root[11], root[12], root[13]]);
    if root_extent == 0 || root_size == 0 {
        return Err("root directory record is empty".to_owned());
    }

    Ok(IsoVolumeDescriptor {
        volume_id: trim_ascii(&descriptor[40..72]),
        system_id: trim_ascii(&descriptor[8..40]),
        block_size,
        block_count,
        root_extent,
        root_size,
        volume_sequence_number,
    })
}

fn both_endian_u32(bytes: &[u8], offset: usize, field: &str) -> Result<u32, String> {
    let raw = bytes
        .get(offset..offset + 8)
        .ok_or_else(|| format!("truncated {field}"))?;
    let little = u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]);
    let big = u32::from_be_bytes([raw[4], raw[5], raw[6], raw[7]]);
    if little != big {
        return Err(format!("{field} little/big endian values disagree"));
    }
    Ok(little)
}

fn both_endian_u16(bytes: &[u8], offset: usize, field: &str) -> Result<u16, String> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| format!("truncated {field}"))?;
    let little = u16::from_le_bytes([raw[0], raw[1]]);
    let big = u16::from_be_bytes([raw[2], raw[3]]);
    if little != big {
        return Err(format!("{field} little/big endian values disagree"));
    }
    Ok(little)
}

fn trim_ascii(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes).trim().to_owned()
}

fn invalid_iso(path: &Path, reason: impl Into<String>) -> UsbError {
    UsbError::InvalidMedia {
        format: "iso9660",
        path: path.to_path_buf(),
        reason: reason.into(),
    }
}
