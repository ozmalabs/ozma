// SPDX-License-Identifier: AGPL-3.0-only
//! Pure-Rust, bounded FAT32 image construction and read-only validation.
//!
//! [`Fat32ImageBuilder`] turns an in-memory directory/file set or a host
//! directory into a standalone FAT32 image. Everything is computed in memory;
//! the builder never mounts a filesystem, opens a block device, runs a
//! subprocess, or writes to the source files.
//!
//! The layout is deliberately conservative and static: one sector per
//! cluster by default (compact images), two FATs, a 32-sector reserved area,
//! and at least the FAT32 minimum of 65,525 data clusters. The resulting
//! image is immutable and is intended to be exposed read-only as virtual
//! media. Live sector synthesis and write overlays belong to the node's
//! userspace SCSI layer, not this crate.
//!
//! Parsing helpers read an existing FAT32 image without mounting it. They are
//! intentionally strict about the BPB and bounded about directory/FAT walks.

use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use crate::error::UsbError;

/// FAT sector size in bytes.
pub const FAT32_SECTOR_SIZE: usize = 512;
/// Default cluster size (one sector).
pub const FAT32_DEFAULT_CLUSTER_SIZE: usize = 512;
/// FAT32 requires at least this many data clusters in practice.
pub const FAT32_MIN_CLUSTERS: u32 = 65_525;
/// Reserved sectors before the FATs.
pub const FAT32_RESERVED_SECTORS: u16 = 32;
/// Number of FAT copies.
pub const FAT32_NUM_FATS: u8 = 2;
/// First data cluster (cluster 2 is the root directory).
pub const FAT32_ROOT_CLUSTER: u32 = 2;
/// Largest volume label accepted by the FAT specification.
pub const FAT32_MAX_LABEL: usize = 11;
/// Largest individual file the default builder accepts (64 MiB).
pub const FAT32_DEFAULT_MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Largest image the default builder accepts (128 MiB).
pub const FAT32_DEFAULT_MAX_IMAGE_BYTES: u64 = 128 * 1024 * 1024;

const FAT_EOC: u32 = 0x0FFF_FFFF;
const FAT_BAD: u32 = 0x0FFF_FFF7;
const DIR_ENTRY_SIZE: usize = 32;
const ATTR_READ_ONLY: u8 = 0x01;
const ATTR_DIRECTORY: u8 = 0x10;
const ATTR_VOLUME_ID: u8 = 0x08;
const ATTR_LFN: u8 = 0x0F;

/// Builder limits and geometry options.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fat32Options {
    /// Volume label (upper-cased and truncated to 11 bytes).
    pub label: String,
    /// Volume serial number stored in the boot sector.
    pub volume_id: u32,
    /// Cluster size in bytes; a power of two from 512 to 65536.
    pub cluster_size: usize,
    /// Optional minimum image size. The image can be larger due to geometry.
    pub total_size_bytes: Option<u64>,
    /// Hard upper bound for the produced image.
    pub max_image_bytes: u64,
    /// Hard upper bound for one file.
    pub max_file_bytes: u64,
    /// Maximum number of files (directories do not count).
    pub max_files: usize,
    /// Maximum nested directory depth.
    pub max_depth: usize,
}

impl Default for Fat32Options {
    fn default() -> Self {
        Self {
            label: "OZMA".to_owned(),
            volume_id: 0x1234_5678,
            cluster_size: FAT32_DEFAULT_CLUSTER_SIZE,
            total_size_bytes: None,
            max_image_bytes: FAT32_DEFAULT_MAX_IMAGE_BYTES,
            max_file_bytes: FAT32_DEFAULT_MAX_FILE_BYTES,
            max_files: 4096,
            max_depth: 32,
        }
    }
}

impl Fat32Options {
    /// Options with a volume label.
    pub fn with_label(label: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            ..Self::default()
        }
    }

    /// Validate stand-alone options before building.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.label.trim().is_empty() {
            return Err(invalid_fat32("volume label must not be empty"));
        }
        if self.label.len() > FAT32_MAX_LABEL {
            return Err(invalid_fat32(format!(
                "volume label must be at most {FAT32_MAX_LABEL} bytes"
            )));
        }
        if self.label.chars().any(char::is_control) {
            return Err(invalid_fat32(
                "volume label must not contain control characters",
            ));
        }
        if self.cluster_size < FAT32_SECTOR_SIZE
            || self.cluster_size > 65_536
            || !self.cluster_size.is_power_of_two()
            || self.cluster_size % FAT32_SECTOR_SIZE != 0
        {
            return Err(invalid_fat32(
                "cluster size must be a power of two from 512 to 65536 bytes",
            ));
        }
        if self.max_files == 0 {
            return Err(invalid_fat32("max_files must be greater than zero"));
        }
        if self.max_depth == 0 {
            return Err(invalid_fat32("max_depth must be greater than zero"));
        }
        if self.max_file_bytes == 0 || self.max_image_bytes < FAT32_SECTOR_SIZE as u64 {
            return Err(invalid_fat32("size bounds must be greater than zero"));
        }
        if let Some(total) = self.total_size_bytes {
            if total == 0 {
                return Err(invalid_fat32("total_size_bytes must be greater than zero"));
            }
            if total > self.max_image_bytes {
                return Err(invalid_fat32(format!(
                    "requested size {total} exceeds max_image_bytes {}",
                    self.max_image_bytes
                )));
            }
        }
        Ok(())
    }
}

/// A node in the image's file tree.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fat32Node {
    /// A regular file.
    File {
        /// File name (single path component).
        name: String,
        /// File contents.
        data: Vec<u8>,
    },
    /// A directory.
    Directory {
        /// Directory name (empty only for the root).
        name: String,
        /// Children.
        entries: Vec<Fat32Node>,
    },
}

impl Fat32Node {
    /// A file node.
    pub fn file(name: impl Into<String>, data: impl Into<Vec<u8>>) -> Self {
        Self::File {
            name: name.into(),
            data: data.into(),
        }
    }

    /// A directory node.
    pub fn directory(name: impl Into<String>, entries: Vec<Fat32Node>) -> Self {
        Self::Directory {
            name: name.into(),
            entries,
        }
    }

    /// Node name.
    pub fn name(&self) -> &str {
        match self {
            Self::File { name, .. } | Self::Directory { name, .. } => name,
        }
    }

    /// Whether this is a directory.
    pub fn is_dir(&self) -> bool {
        matches!(self, Self::Directory { .. })
    }
}

/// A builder for a standalone FAT32 image.
#[derive(Clone, Debug)]
pub struct Fat32ImageBuilder {
    options: Fat32Options,
    files: BTreeMap<Vec<String>, Vec<u8>>,
    directories: BTreeSet<Vec<String>>,
    file_bytes: u64,
}

impl Fat32ImageBuilder {
    /// A builder with the given volume label.
    pub fn new(label: impl Into<String>) -> Self {
        Self {
            options: Fat32Options::with_label(label),
            files: BTreeMap::new(),
            directories: BTreeSet::new(),
            file_bytes: 0,
        }
    }

    /// Replace all options.
    pub fn with_options(mut self, options: Fat32Options) -> Self {
        self.options = options;
        self
    }

    /// Options currently in effect.
    pub fn options(&self) -> &Fat32Options {
        &self.options
    }

    /// Add or replace an empty directory.
    pub fn add_directory(&mut self, path: impl AsRef<Path>) -> Result<&mut Self, UsbError> {
        self.options.validate()?;
        let components = normalise_guest_path(path.as_ref())?;
        ensure_parents(self, &components)?;
        self.directories.insert(components);
        Ok(self)
    }

    /// Add a file from memory.
    pub fn add_file(
        &mut self,
        path: impl AsRef<Path>,
        data: impl Into<Vec<u8>>,
    ) -> Result<&mut Self, UsbError> {
        self.options.validate()?;
        let components = normalise_guest_path(path.as_ref())?;
        let data = data.into();
        check_file_size(self, &components, data.len() as u64)?;
        ensure_parents(self, &components)?;

        let key = components.clone();
        if self.directories.contains(&key) {
            return Err(invalid_fat32(format!(
                "path {} is already a directory",
                display_path(&components)
            )));
        }
        if self.files.insert(key, data).is_none() {
            if self.files.len() > self.options.max_files {
                self.files.remove(&components);
                return Err(invalid_fat32(format!(
                    "at most {} files are supported",
                    self.options.max_files
                )));
            }
        }
        self.file_bytes = self.files.values().map(|data| data.len() as u64).sum();
        Ok(self)
    }

    /// Add a file by reading a bounded amount from the host path.
    ///
    /// Symlinks and non-regular files are rejected; no source file is written.
    pub fn add_file_from_path(
        &mut self,
        guest_path: impl AsRef<Path>,
        host_path: impl AsRef<Path>,
    ) -> Result<&mut Self, UsbError> {
        self.options.validate()?;
        let host_path = host_path.as_ref();
        let metadata = fs::symlink_metadata(host_path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                UsbError::MediaNotFound(host_path.to_path_buf())
            } else {
                crate::error::io_error("stat FAT32 source", host_path.to_path_buf(), source)
            }
        })?;
        if metadata.file_type().is_symlink() {
            return Err(invalid_fat32(format!(
                "source {} is a symlink; refusing to follow it",
                host_path.display()
            )));
        }
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(host_path.to_path_buf()));
        }
        if metadata.len() > self.options.max_file_bytes {
            return Err(invalid_fat32(format!(
                "source {} is {} bytes; max_file_bytes is {}",
                host_path.display(),
                metadata.len(),
                self.options.max_file_bytes
            )));
        }
        let data = read_bounded(host_path, self.options.max_file_bytes)?;
        self.add_file(guest_path, data)
    }

    /// Recursively add a host directory.
    ///
    /// The walk skips dotfiles and symlinks, is depth-bounded, and never
    /// follows a symlink. It returns the number of files added.
    pub fn add_directory_tree(&mut self, host_root: impl AsRef<Path>) -> Result<usize, UsbError> {
        self.options.validate()?;
        let host_root = host_root.as_ref();
        let metadata = fs::symlink_metadata(host_root).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                UsbError::MediaNotFound(host_root.to_path_buf())
            } else {
                crate::error::io_error(
                    "stat FAT32 source directory",
                    host_root.to_path_buf(),
                    source,
                )
            }
        })?;
        if !metadata.is_dir() {
            return Err(UsbError::MediaNotFile(host_root.to_path_buf()));
        }
        let mut count = 0;
        self.walk_directory(host_root, Path::new(""), 0, &mut count)?;
        Ok(count)
    }

    /// Build the image.
    pub fn build(self) -> Result<Fat32Image, UsbError> {
        self.options.validate()?;

        let mut root = Node::directory(String::new());
        let mut files = self.files.into_iter().collect::<Vec<_>>();
        files.sort_by(|(left, _), (right, _)| left.cmp(right));
        let mut directories = self.directories.into_iter().collect::<Vec<_>>();
        directories.sort_by(|left, right| left.len().cmp(&right.len()).then(left.cmp(right)));

        for path in directories {
            insert_directory(&mut root, &path)?;
        }
        for (path, data) in files {
            insert_file(&mut root, &path, data)?;
        }

        let total_files = count_files(&root);
        if total_files > self.options.max_files {
            return Err(invalid_fat32(format!(
                "at most {} files are supported",
                self.options.max_files
            )));
        }
        sort_tree(&mut root);
        validate_tree(&root, &self.options, 0)?;
        assign_short_names(&mut root);
        compute_directory_clusters(&mut root, self.options.cluster_size);
        assign_clusters(&mut root, self.options.cluster_size)?;

        let needed_clusters = root.next_cluster.saturating_sub(FAT32_ROOT_CLUSTER).max(1);
        let mut total_clusters = needed_clusters.max(FAT32_MIN_CLUSTERS);
        if let Some(total_size) = self.options.total_size_bytes {
            let cluster_size = self.options.cluster_size as u64;
            let wanted = total_size.saturating_add(cluster_size - 1) / cluster_size;
            total_clusters = total_clusters.max(u32::try_from(wanted).unwrap_or(u32::MAX));
        }

        let sectors_per_cluster = u8::try_from(self.options.cluster_size / FAT32_SECTOR_SIZE)
            .map_err(|_| invalid_fat32("cluster size does not fit FAT sectors-per-cluster"))?;
        let fat_sectors = u32::try_from(
            (u64::from(total_clusters) + 2)
                .saturating_mul(4)
                .saturating_add(FAT32_SECTOR_SIZE as u64 - 1)
                / FAT32_SECTOR_SIZE as u64,
        )
        .map_err(|_| invalid_fat32("FAT is too large"))?;
        let total_sectors = u64::from(FAT32_RESERVED_SECTORS)
            + u64::from(FAT32_NUM_FATS) * u64::from(fat_sectors)
            + u64::from(total_clusters) * u64::from(sectors_per_cluster);
        let total_bytes = total_sectors * FAT32_SECTOR_SIZE as u64;
        if total_bytes > self.options.max_image_bytes {
            return Err(invalid_fat32(format!(
                "image would be {total_bytes} bytes; max_image_bytes is {}",
                self.options.max_image_bytes
            )));
        }
        let total_sectors = u32::try_from(total_sectors)
            .map_err(|_| invalid_fat32("image has too many sectors"))?;

        let layout = Fat32Layout {
            bytes_per_sector: FAT32_SECTOR_SIZE as u16,
            sectors_per_cluster,
            reserved_sectors: FAT32_RESERVED_SECTORS,
            num_fats: FAT32_NUM_FATS,
            fat_sectors,
            total_sectors,
            root_cluster: FAT32_ROOT_CLUSTER,
            cluster_size: self.options.cluster_size,
            total_clusters,
            data_start_sector: u32::from(FAT32_RESERVED_SECTORS)
                + u32::from(FAT32_NUM_FATS) * fat_sectors,
            label: normalise_label(&self.options.label),
            volume_id: self.options.volume_id,
        };

        let mut image = vec![0u8; total_bytes as usize];
        write_boot_sector(&mut image, &layout);
        write_fsinfo(&mut image, total_clusters.saturating_sub(needed_clusters));
        write_fat(&mut image, &layout, &root)?;
        write_data_region(&mut image, &layout, &root)?;

        let info = parse_fat32_image(&image)?;
        Ok(Fat32Image {
            bytes: image,
            layout,
            info,
            entries: collect_entries(&root),
        })
    }
}

/// Build an image from an in-memory node tree.
pub fn build_fat32_image(entries: Vec<Fat32Node>) -> Result<Fat32Image, UsbError> {
    let mut builder = Fat32ImageBuilder::new("OZMA");
    add_nodes(&mut builder, Path::new(""), &entries)?;
    builder.build()
}

/// Build a read-only image from a host directory.
pub fn fat32_image_from_directory(host_root: impl AsRef<Path>) -> Result<Fat32Image, UsbError> {
    let mut builder = Fat32ImageBuilder::new("OZMA");
    builder.add_directory_tree(host_root)?;
    builder.build()
}

/// Byte layout of a FAT32 image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fat32Layout {
    /// Bytes per logical sector.
    pub bytes_per_sector: u16,
    /// Sectors per cluster.
    pub sectors_per_cluster: u8,
    /// Reserved sectors before the first FAT.
    pub reserved_sectors: u16,
    /// Number of FAT copies.
    pub num_fats: u8,
    /// Sectors per FAT.
    pub fat_sectors: u32,
    /// Total logical sectors.
    pub total_sectors: u32,
    /// First cluster of the root directory.
    pub root_cluster: u32,
    /// Cluster size in bytes.
    pub cluster_size: usize,
    /// Total data clusters.
    pub total_clusters: u32,
    /// First sector of the data region.
    pub data_start_sector: u32,
    /// Normalised 11-byte volume label.
    pub label: [u8; FAT32_MAX_LABEL],
    /// Volume serial number.
    pub volume_id: u32,
}

impl Fat32Layout {
    /// Total image size in bytes.
    pub fn total_bytes(&self) -> u64 {
        u64::from(self.total_sectors) * u64::from(self.bytes_per_sector)
    }
}

/// One entry discovered by the read-only parser.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fat32EntryInfo {
    /// Long name when present, otherwise the 8.3 name.
    pub name: String,
    /// Whether this is a directory.
    pub is_dir: bool,
    /// File size in bytes (zero for directories).
    pub size_bytes: u32,
    /// First cluster (zero for an empty file).
    pub first_cluster: u32,
    /// Cluster chain starting at `first_cluster`.
    pub clusters: Vec<u32>,
}

/// Parsed summary of a FAT32 image.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fat32Info {
    /// Volume label.
    pub label: String,
    /// Volume serial number.
    pub volume_id: u32,
    /// Bytes per sector.
    pub bytes_per_sector: u16,
    /// Sectors per cluster.
    pub sectors_per_cluster: u8,
    /// Cluster size in bytes.
    pub cluster_size: usize,
    /// Total data clusters.
    pub total_clusters: u32,
    /// Root directory cluster.
    pub root_cluster: u32,
    /// Total image size in bytes.
    pub total_bytes: u64,
    /// Root directory entries.
    pub entries: Vec<Fat32EntryInfo>,
}

/// A complete FAT32 image held in memory.
#[derive(Clone, Debug)]
pub struct Fat32Image {
    bytes: Vec<u8>,
    layout: Fat32Layout,
    info: Fat32Info,
    entries: Vec<Fat32EntryInfo>,
}

impl Fat32Image {
    /// Raw image bytes.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes
    }

    /// Image length in bytes.
    pub fn len(&self) -> usize {
        self.bytes.len()
    }

    /// Whether the image is empty (it never is for a built image).
    pub fn is_empty(&self) -> bool {
        self.bytes.is_empty()
    }

    /// Image layout.
    pub fn layout(&self) -> &Fat32Layout {
        &self.layout
    }

    /// Parsed metadata, including root entries.
    pub fn info(&self) -> &Fat32Info {
        &self.info
    }

    /// All files and directories in the built tree.
    pub fn entries(&self) -> &[Fat32EntryInfo] {
        &self.entries
    }

    /// Read a 512-byte sector by absolute index.
    pub fn read_sector(&self, sector: u32) -> Option<&[u8]> {
        let start = usize::try_from(sector)
            .ok()?
            .checked_mul(FAT32_SECTOR_SIZE)?;
        self.bytes.get(start..start + FAT32_SECTOR_SIZE)
    }

    /// Re-run the read-only parser and return its summary.
    pub fn validate(&self) -> Result<Fat32Info, UsbError> {
        parse_fat32_image(&self.bytes)
    }

    /// The image is immutable; media should present it read-only.
    pub fn is_read_only(&self) -> bool {
        true
    }
}

impl AsRef<[u8]> for Fat32Image {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}

/// Fields extracted from a FAT32 boot sector/BPB.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Fat32BootInfo {
    /// Bytes per logical sector.
    pub bytes_per_sector: u16,
    /// Sectors per cluster.
    pub sectors_per_cluster: u8,
    /// Cluster size in bytes.
    pub cluster_size: usize,
    /// Reserved sectors before the first FAT.
    pub reserved_sectors: u16,
    /// Number of FAT copies.
    pub num_fats: u8,
    /// Sectors per FAT.
    pub fat_sectors: u32,
    /// Total logical sectors.
    pub total_sectors: u32,
    /// Total data clusters.
    pub total_clusters: u32,
    /// First cluster of the root directory.
    pub root_cluster: u32,
    /// Declared image size in bytes.
    pub total_bytes: u64,
    /// Volume label.
    pub label: String,
    /// Volume serial number.
    pub volume_id: u32,
}

/// Parse and validate a FAT32 boot sector/BPB.
pub fn parse_fat32_boot_sector(bytes: &[u8]) -> Result<Fat32BootInfo, UsbError> {
    if bytes.len() < FAT32_SECTOR_SIZE {
        return Err(invalid_fat32("image is smaller than one sector"));
    }
    let bytes_per_sector = read_u16(bytes, 11)? as usize;
    if !matches!(bytes_per_sector, 512 | 1024 | 2048 | 4096) {
        return Err(invalid_fat32("unsupported bytes-per-sector"));
    }
    let sectors_per_cluster = bytes[13];
    if sectors_per_cluster == 0 || !sectors_per_cluster.is_power_of_two() {
        return Err(invalid_fat32("sectors-per-cluster must be a power of two"));
    }
    let cluster_size = bytes_per_sector * sectors_per_cluster as usize;
    let reserved_sectors = read_u16(bytes, 14)?;
    if reserved_sectors == 0 {
        return Err(invalid_fat32("reserved sector count must be non-zero"));
    }
    let num_fats = bytes[16];
    if num_fats == 0 {
        return Err(invalid_fat32("number of FATs must be non-zero"));
    }
    if read_u16(bytes, 17)? != 0 {
        return Err(invalid_fat32("FAT32 root entry count must be zero"));
    }
    if read_u16(bytes, 19)? != 0 {
        return Err(invalid_fat32("FAT32 total-sectors-16 must be zero"));
    }
    if read_u16(bytes, 22)? != 0 {
        return Err(invalid_fat32("FAT32 FAT-size-16 must be zero"));
    }
    let total_sectors = read_u32(bytes, 32)?;
    if total_sectors == 0 {
        return Err(invalid_fat32("total sector count must be non-zero"));
    }
    let fat_sectors = read_u32(bytes, 36)?;
    if fat_sectors == 0 {
        return Err(invalid_fat32("FAT size must be non-zero"));
    }
    let root_cluster = read_u32(bytes, 44)? & 0x0FFF_FFFF;
    if root_cluster < FAT32_ROOT_CLUSTER {
        return Err(invalid_fat32("root cluster must be >= 2"));
    }
    if read_u16(bytes, 510)? != 0xAA55 {
        return Err(invalid_fat32("missing 0x55AA boot signature"));
    }
    let fs_type = String::from_utf8_lossy(&bytes[82..90]).trim().to_owned();
    if fs_type != "FAT32" {
        return Err(invalid_fat32("filesystem type string is not FAT32"));
    }
    let total_bytes = u64::from(total_sectors) * u64::from(bytes_per_sector as u32);
    let data_start_sector = u32::from(reserved_sectors) + u32::from(num_fats) * fat_sectors;
    let total_clusters = total_sectors
        .checked_sub(data_start_sector)
        .ok_or_else(|| invalid_fat32("data region starts after the volume ends"))?
        / u32::from(sectors_per_cluster);
    if total_clusters < 1 {
        return Err(invalid_fat32("image has no data clusters"));
    }
    Ok(Fat32BootInfo {
        bytes_per_sector: bytes_per_sector as u16,
        sectors_per_cluster,
        cluster_size,
        reserved_sectors,
        num_fats,
        fat_sectors,
        total_sectors,
        total_clusters,
        root_cluster,
        total_bytes,
        label: String::from_utf8_lossy(&bytes[71..82]).trim().to_owned(),
        volume_id: read_u32(bytes, 67)?,
    })
}

/// Validate an existing FAT32 image without mounting it.
pub fn parse_fat32_image(bytes: &[u8]) -> Result<Fat32Info, UsbError> {
    let boot = parse_fat32_boot_sector(bytes)?;
    if boot.total_bytes > bytes.len() as u64 {
        return Err(invalid_fat32(format!(
            "image declares {} bytes but only {} are available",
            boot.total_bytes,
            bytes.len()
        )));
    }
    let bytes_per_sector = boot.bytes_per_sector as usize;
    let sectors_per_cluster = boot.sectors_per_cluster;
    let cluster_size = boot.cluster_size;
    let reserved_sectors = boot.reserved_sectors;
    let fat_sectors = boot.fat_sectors;
    let root_cluster = boot.root_cluster;
    let total_clusters = boot.total_clusters;

    let fat_offset = usize::from(reserved_sectors) * bytes_per_sector;
    let fat_end = fat_offset
        .checked_add(fat_sectors as usize * bytes_per_sector)
        .ok_or_else(|| invalid_fat32("FAT range overflows"))?;
    if fat_end > bytes.len() {
        return Err(invalid_fat32("FAT extends past the image"));
    }
    let fat = &bytes[fat_offset..fat_end];
    let fat_entry = |cluster: u32| -> Option<u32> {
        let offset = usize::try_from(cluster).ok()?.checked_mul(4)?;
        let raw = fat.get(offset..offset + 4)?;
        Some(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]) & 0x0FFF_FFFF)
    };
    if fat_entry(0).ok_or_else(|| invalid_fat32("FAT is too small"))? & 0x0FFF_FFF8 != 0x0FFF_FFF8 {
        return Err(invalid_fat32("invalid first FAT entry"));
    }
    if fat_entry(1).ok_or_else(|| invalid_fat32("FAT is too small"))? & 0x0FFF_FFFF != FAT_EOC {
        return Err(invalid_fat32("invalid second FAT entry"));
    }
    if root_cluster >= total_clusters.saturating_add(FAT32_ROOT_CLUSTER) {
        return Err(invalid_fat32("root cluster is outside the data region"));
    }

    let root_chain = fat_chain(fat_entry, root_cluster, total_clusters)?;
    let data_start_sector = u32::from(reserved_sectors) + u32::from(boot.num_fats) * fat_sectors;
    let data_offset = usize::try_from(data_start_sector)
        .unwrap_or(usize::MAX)
        .saturating_mul(bytes_per_sector);
    let entries = read_directory(
        bytes,
        cluster_size,
        data_offset,
        total_clusters,
        &root_chain,
        fat_entry,
    )?;
    Ok(Fat32Info {
        label: boot.label,
        volume_id: boot.volume_id,
        bytes_per_sector: bytes_per_sector as u16,
        sectors_per_cluster,
        cluster_size,
        total_clusters,
        root_cluster,
        total_bytes: boot.total_bytes,
        entries,
    })
}

/// Validate a FAT32 image file's boot sector without reading its whole body.
pub fn validate_fat32_boot_sector(path: &Path) -> Result<Fat32BootInfo, UsbError> {
    let metadata = fs::metadata(path).map_err(|source| {
        if source.kind() == std::io::ErrorKind::NotFound {
            UsbError::MediaNotFound(path.to_path_buf())
        } else {
            crate::error::io_error("stat FAT32 image", path.to_path_buf(), source)
        }
    })?;
    if !metadata.is_file() {
        return Err(UsbError::MediaNotFile(path.to_path_buf()));
    }
    if metadata.len() < FAT32_SECTOR_SIZE as u64 {
        return Err(UsbError::InvalidMedia {
            format: "fat32",
            path: path.to_path_buf(),
            reason: "image is smaller than one sector".to_owned(),
        });
    }
    let mut file = File::open(path)
        .map_err(|source| crate::error::io_error("open FAT32 image", path.to_path_buf(), source))?;
    let mut bytes = [0u8; FAT32_SECTOR_SIZE];
    file.read_exact(&mut bytes)
        .map_err(|source| crate::error::io_error("read FAT32 image", path.to_path_buf(), source))?;
    parse_fat32_boot_sector(&bytes).map_err(|error| match error {
        UsbError::InvalidMedia { reason, .. } => UsbError::InvalidMedia {
            format: "fat32",
            path: path.to_path_buf(),
            reason,
        },
        other => other,
    })
}

// ---------------------------------------------------------------------------
// Internal tree model and builder helpers
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
struct Node {
    name: String,
    is_dir: bool,
    data: Vec<u8>,
    children: Vec<Node>,
    cluster: u32,
    clusters: u32,
    short_name: [u8; FAT32_MAX_LABEL],
    needs_lfn: bool,
    next_cluster: u32,
}

impl Node {
    fn directory(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            is_dir: true,
            data: Vec::new(),
            children: Vec::new(),
            cluster: 0,
            clusters: 0,
            short_name: [b' '; FAT32_MAX_LABEL],
            needs_lfn: false,
            next_cluster: 0,
        }
    }

    fn file(name: impl Into<String>, data: Vec<u8>) -> Self {
        Self {
            name: name.into(),
            is_dir: false,
            data,
            children: Vec::new(),
            cluster: 0,
            clusters: 0,
            short_name: [b' '; FAT32_MAX_LABEL],
            needs_lfn: false,
            next_cluster: 0,
        }
    }
}

fn add_nodes(
    builder: &mut Fat32ImageBuilder,
    prefix: &Path,
    nodes: &[Fat32Node],
) -> Result<(), UsbError> {
    for node in nodes {
        let path = prefix.join(node.name());
        match node {
            Fat32Node::File { data, .. } => {
                builder.add_file(&path, data.clone())?;
            }
            Fat32Node::Directory { entries, .. } => {
                builder.add_directory(&path)?;
                add_nodes(builder, &path, entries)?;
            }
        }
    }
    Ok(())
}

fn insert_directory(root: &mut Node, path: &[String]) -> Result<(), UsbError> {
    let mut current = root;
    for (index, component) in path.iter().enumerate() {
        let position = current
            .children
            .iter()
            .position(|child| name_key(&child.name) == name_key(component));
        let position = match position {
            Some(position) => {
                if !current.children[position].is_dir {
                    return Err(invalid_fat32(format!(
                        "{} is a file, not a directory",
                        display_path(&path[..=index])
                    )));
                }
                position
            }
            None => {
                current.children.push(Node::directory(component.clone()));
                current.children.len() - 1
            }
        };
        current = &mut current.children[position];
    }
    Ok(())
}

fn insert_file(root: &mut Node, path: &[String], data: Vec<u8>) -> Result<(), UsbError> {
    if path.is_empty() {
        return Err(invalid_fat32("file path must not be empty"));
    }
    let (parents, name) = path.split_at(path.len() - 1);
    let mut current = root;
    for (index, component) in parents.iter().enumerate() {
        let position = current
            .children
            .iter()
            .position(|child| name_key(&child.name) == name_key(component));
        let position = match position {
            Some(position) => {
                if !current.children[position].is_dir {
                    return Err(invalid_fat32(format!(
                        "{} is a file, not a directory",
                        display_path(&parents[..=index])
                    )));
                }
                position
            }
            None => {
                current.children.push(Node::directory(component.clone()));
                current.children.len() - 1
            }
        };
        current = &mut current.children[position];
    }
    if current
        .children
        .iter()
        .any(|child| name_key(&child.name) == name_key(&name[0]))
    {
        return Err(invalid_fat32(format!(
            "duplicate path {}",
            display_path(path)
        )));
    }
    current.children.push(Node::file(name[0].clone(), data));
    Ok(())
}

fn count_files(node: &Node) -> usize {
    if node.is_dir {
        node.children.iter().map(count_files).sum()
    } else {
        1
    }
}

fn sort_tree(node: &mut Node) {
    node.children.sort_by_key(|child| name_key(&child.name));
    for child in &mut node.children {
        sort_tree(child);
    }
}

fn validate_tree(node: &Node, options: &Fat32Options, depth: usize) -> Result<(), UsbError> {
    if depth > options.max_depth {
        return Err(invalid_fat32(format!(
            "directory depth exceeds {}",
            options.max_depth
        )));
    }
    for child in &node.children {
        validate_component(&child.name)?;
        if child.is_dir {
            validate_tree(child, options, depth + 1)?;
        } else if child.data.len() as u64 > options.max_file_bytes {
            return Err(invalid_fat32(format!(
                "file {:?} is {} bytes; max_file_bytes is {}",
                child.name,
                child.data.len(),
                options.max_file_bytes
            )));
        }
    }
    Ok(())
}

fn assign_short_names(node: &mut Node) {
    if !node.is_dir {
        return;
    }
    let mut used = BTreeSet::new();
    for child in &mut node.children {
        let (short, needs_lfn) = make_short_name(&child.name, &mut used);
        child.short_name = short;
        child.needs_lfn = needs_lfn;
        assign_short_names(child);
    }
}

fn compute_directory_clusters(node: &mut Node, cluster_size: usize) {
    if !node.is_dir {
        return;
    }
    let mut entry_count = if node.name.is_empty() { 1 } else { 2 };
    for child in &node.children {
        entry_count += 1;
        if child.needs_lfn {
            entry_count += lfn_entry_count(&child.name);
        }
    }
    entry_count += 1; // end marker
    let bytes = entry_count * DIR_ENTRY_SIZE;
    node.clusters = u32::try_from(bytes.div_ceil(cluster_size).max(1)).unwrap_or(u32::MAX);
    for child in &mut node.children {
        compute_directory_clusters(child, cluster_size);
    }
}

fn assign_clusters(root: &mut Node, cluster_size: usize) -> Result<(), UsbError> {
    root.cluster = FAT32_ROOT_CLUSTER;
    let mut next = FAT32_ROOT_CLUSTER
        .checked_add(root.clusters.max(1))
        .ok_or_else(|| invalid_fat32("cluster allocation overflow"))?;
    assign_children(root, &mut next, cluster_size)?;
    root.next_cluster = next;
    Ok(())
}

fn assign_children(node: &mut Node, next: &mut u32, cluster_size: usize) -> Result<(), UsbError> {
    for child in &mut node.children {
        if child.is_dir {
            child.cluster = *next;
            *next = next
                .checked_add(child.clusters.max(1))
                .ok_or_else(|| invalid_fat32("cluster allocation overflow"))?;
        } else if child.data.is_empty() {
            child.cluster = 0;
        } else {
            child.cluster = *next;
            child.clusters = u32::try_from(child.data.len().div_ceil(cluster_size).max(1))
                .map_err(|_| invalid_fat32("file is too large"))?;
            *next = next
                .checked_add(child.clusters)
                .ok_or_else(|| invalid_fat32("cluster allocation overflow"))?;
        }
    }
    for child in &mut node.children {
        if child.is_dir {
            assign_children(child, next, cluster_size)?;
        }
    }
    Ok(())
}

fn normalise_guest_path(path: &Path) -> Result<Vec<String>, UsbError> {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::Normal(value) => {
                let value = value
                    .to_str()
                    .ok_or_else(|| invalid_fat32("path components must be valid UTF-8"))?;
                validate_component(value)?;
                components.push(value.to_owned());
            }
            Component::CurDir | Component::RootDir => {}
            Component::ParentDir => {
                return Err(invalid_fat32("path must not contain '..'"));
            }
            Component::Prefix(_) => {
                return Err(invalid_fat32("path must be a relative guest path"));
            }
        }
    }
    if components.is_empty() {
        return Err(invalid_fat32("path must name at least one component"));
    }
    Ok(components)
}

fn validate_component(name: &str) -> Result<(), UsbError> {
    if name.is_empty() {
        return Err(invalid_fat32("file name must not be empty"));
    }
    if name.len() > 255 {
        return Err(invalid_fat32("file name must be at most 255 bytes"));
    }
    if name == "." || name == ".." {
        return Err(invalid_fat32("file name must not be '.' or '..'"));
    }
    if name
        .chars()
        .any(|character| character.is_control() || matches!(character, '/' | '\\' | '\0'))
    {
        return Err(invalid_fat32(format!(
            "file name {name:?} contains a forbidden character"
        )));
    }
    Ok(())
}

fn name_key(name: &str) -> String {
    name.to_lowercase()
}

fn display_path(path: &[String]) -> String {
    path.join("/")
}

fn check_file_size(
    builder: &Fat32ImageBuilder,
    path: &[String],
    size: u64,
) -> Result<(), UsbError> {
    if size > builder.options.max_file_bytes {
        return Err(invalid_fat32(format!(
            "file {} is {size} bytes; max_file_bytes is {}",
            display_path(path),
            builder.options.max_file_bytes
        )));
    }
    let new_total = builder
        .file_bytes
        .saturating_sub(
            builder
                .files
                .get(path)
                .map(|data| data.len() as u64)
                .unwrap_or(0),
        )
        .saturating_add(size);
    if new_total > builder.options.max_image_bytes {
        return Err(invalid_fat32(format!(
            "file data totals {new_total} bytes; max_image_bytes is {}",
            builder.options.max_image_bytes
        )));
    }
    Ok(())
}

fn ensure_parents(builder: &mut Fat32ImageBuilder, components: &[String]) -> Result<(), UsbError> {
    if components.len() > builder.options.max_depth {
        return Err(invalid_fat32(format!(
            "path depth exceeds {}",
            builder.options.max_depth
        )));
    }
    for index in 1..components.len() {
        builder.directories.insert(components[..index].to_vec());
    }
    Ok(())
}

impl Fat32ImageBuilder {
    fn walk_directory(
        &mut self,
        host: &Path,
        guest: &Path,
        depth: usize,
        count: &mut usize,
    ) -> Result<(), UsbError> {
        if depth >= self.options.max_depth {
            return Err(invalid_fat32(format!(
                "directory depth exceeds {}",
                self.options.max_depth
            )));
        }
        let mut entries = fs::read_dir(host)
            .map_err(|source| {
                crate::error::io_error("read FAT32 source", host.to_path_buf(), source)
            })?
            .filter_map(Result::ok)
            .collect::<Vec<_>>();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let name = entry.file_name();
            let Some(name) = name.to_str() else {
                continue;
            };
            if name.starts_with('.') {
                continue;
            }
            let file_type = entry.file_type().map_err(|source| {
                crate::error::io_error("stat FAT32 source", entry.path(), source)
            })?;
            if file_type.is_symlink() {
                continue;
            }
            let child_guest = guest.join(name);
            if file_type.is_dir() {
                self.add_directory(&child_guest)?;
                self.walk_directory(&entry.path(), &child_guest, depth + 1, count)?;
            } else if file_type.is_file() {
                self.add_file_from_path(&child_guest, entry.path())?;
                *count += 1;
                if *count > self.options.max_files {
                    return Err(invalid_fat32(format!(
                        "at most {} files are supported",
                        self.options.max_files
                    )));
                }
            }
        }
        Ok(())
    }
}

fn read_bounded(path: &Path, limit: u64) -> Result<Vec<u8>, UsbError> {
    let file = File::open(path).map_err(|source| {
        crate::error::io_error("open FAT32 source", path.to_path_buf(), source)
    })?;
    let mut data = Vec::new();
    file.take(limit.saturating_add(1))
        .read_to_end(&mut data)
        .map_err(|source| {
            crate::error::io_error("read FAT32 source", path.to_path_buf(), source)
        })?;
    if data.len() as u64 > limit {
        return Err(invalid_fat32(format!(
            "source {} exceeds the configured file bound",
            path.display()
        )));
    }
    Ok(data)
}

// ---------------------------------------------------------------------------
// On-disk writer
// ---------------------------------------------------------------------------

fn normalise_label(label: &str) -> [u8; FAT32_MAX_LABEL] {
    let mut result = [b' '; FAT32_MAX_LABEL];
    for (index, byte) in label.bytes().take(FAT32_MAX_LABEL).enumerate() {
        result[index] = byte.to_ascii_uppercase();
    }
    result
}

fn write_boot_sector(image: &mut [u8], layout: &Fat32Layout) {
    let mut boot = [0u8; FAT32_SECTOR_SIZE];
    boot[0..3].copy_from_slice(&[0xEB, 0x58, 0x90]);
    boot[3..11].copy_from_slice(b"OZMA    ");
    put_u16(&mut boot, 11, layout.bytes_per_sector);
    boot[13] = layout.sectors_per_cluster;
    put_u16(&mut boot, 14, layout.reserved_sectors);
    boot[16] = layout.num_fats;
    put_u16(&mut boot, 17, 0);
    put_u16(&mut boot, 19, 0);
    boot[21] = 0xF8;
    put_u16(&mut boot, 22, 0);
    put_u16(&mut boot, 24, 63);
    put_u16(&mut boot, 26, 255);
    put_u32(&mut boot, 28, 0);
    put_u32(&mut boot, 32, layout.total_sectors);
    put_u32(&mut boot, 36, layout.fat_sectors);
    put_u16(&mut boot, 40, 0);
    put_u16(&mut boot, 42, 0);
    put_u32(&mut boot, 44, layout.root_cluster);
    put_u16(&mut boot, 48, 1);
    put_u16(&mut boot, 50, 6);
    boot[64] = 0x80;
    boot[66] = 0x29;
    put_u32(&mut boot, 67, layout.volume_id);
    boot[71..82].copy_from_slice(&layout.label);
    boot[82..90].copy_from_slice(b"FAT32   ");
    boot[510] = 0x55;
    boot[511] = 0xAA;

    image[..FAT32_SECTOR_SIZE].copy_from_slice(&boot);
    let backup_offset = 6 * FAT32_SECTOR_SIZE;
    if image.len() >= backup_offset + FAT32_SECTOR_SIZE {
        image[backup_offset..backup_offset + FAT32_SECTOR_SIZE].copy_from_slice(&boot);
    }
}

fn write_fsinfo(image: &mut [u8], free_clusters: u32) {
    let mut fsinfo = [0u8; FAT32_SECTOR_SIZE];
    put_u32(&mut fsinfo, 0, 0x4161_5252);
    put_u32(&mut fsinfo, 484, 0x6141_7272);
    put_u32(&mut fsinfo, 488, free_clusters);
    put_u32(&mut fsinfo, 492, 3);
    fsinfo[510] = 0x55;
    fsinfo[511] = 0xAA;
    image[FAT32_SECTOR_SIZE..2 * FAT32_SECTOR_SIZE].copy_from_slice(&fsinfo);
    let backup_offset = 7 * FAT32_SECTOR_SIZE;
    if image.len() >= backup_offset + FAT32_SECTOR_SIZE {
        image[backup_offset..backup_offset + FAT32_SECTOR_SIZE].copy_from_slice(&fsinfo);
    }
}

fn write_fat(image: &mut [u8], layout: &Fat32Layout, root: &Node) -> Result<(), UsbError> {
    let entries = usize::try_from(layout.total_clusters)
        .ok()
        .and_then(|count| count.checked_add(2))
        .ok_or_else(|| invalid_fat32("FAT entry count overflow"))?;
    let mut fat = vec![0u8; entries * 4];
    put_u32(&mut fat, 0, 0x0FFF_FFF8);
    put_u32(&mut fat, 4, FAT_EOC);

    let mut chain = Vec::new();
    collect_chains(root, &mut chain)?;
    for (start, count) in chain {
        if count == 0 {
            continue;
        }
        for offset in 0..count {
            let cluster = start + offset;
            if cluster as usize >= entries {
                return Err(invalid_fat32("cluster chain exceeds FAT size"));
            }
            let value = if offset + 1 == count {
                FAT_EOC
            } else {
                cluster + 1
            };
            put_u32(&mut fat, cluster as usize * 4, value);
        }
    }

    let fat_offset = usize::from(layout.reserved_sectors) * FAT32_SECTOR_SIZE;
    let fat_bytes = usize::try_from(layout.fat_sectors).unwrap_or(0) * FAT32_SECTOR_SIZE;
    if fat_bytes < fat.len() || fat_offset + fat_bytes > image.len() {
        return Err(invalid_fat32("FAT does not fit in the image"));
    }
    for copy in 0..usize::from(layout.num_fats) {
        let offset = fat_offset + copy * fat_bytes;
        image[offset..offset + fat.len()].copy_from_slice(&fat);
    }
    Ok(())
}

fn collect_chains(node: &Node, chains: &mut Vec<(u32, u32)>) -> Result<(), UsbError> {
    if node.cluster != 0 {
        chains.push((node.cluster, node.clusters.max(1)));
    }
    for child in &node.children {
        collect_chains(child, chains)?;
    }
    Ok(())
}

fn write_data_region(image: &mut [u8], layout: &Fat32Layout, root: &Node) -> Result<(), UsbError> {
    let data_offset = layout.data_start_sector as usize * FAT32_SECTOR_SIZE;
    write_directory(image, layout, data_offset, root, None)?;
    write_node_data(image, layout, data_offset, root);
    Ok(())
}

fn write_node_data(image: &mut [u8], layout: &Fat32Layout, data_offset: usize, node: &Node) {
    if node.is_dir {
        for child in &node.children {
            write_node_data(image, layout, data_offset, child);
        }
    } else if !node.data.is_empty() && node.cluster >= FAT32_ROOT_CLUSTER {
        let offset =
            data_offset + (node.cluster - FAT32_ROOT_CLUSTER) as usize * layout.cluster_size;
        let end = offset.saturating_add(node.data.len()).min(image.len());
        if offset < end {
            image[offset..end].copy_from_slice(&node.data[..end - offset]);
        }
    }
}

fn write_directory(
    image: &mut [u8],
    layout: &Fat32Layout,
    data_offset: usize,
    node: &Node,
    parent_cluster: Option<u32>,
) -> Result<(), UsbError> {
    if node.cluster < FAT32_ROOT_CLUSTER {
        return Err(invalid_fat32("directory has no cluster"));
    }
    let mut entries = Vec::new();
    if node.name.is_empty() {
        let mut entry = [0u8; DIR_ENTRY_SIZE];
        entry[..FAT32_MAX_LABEL].copy_from_slice(&layout.label);
        entry[11] = ATTR_VOLUME_ID;
        entries.extend_from_slice(&entry);
    } else {
        entries.extend_from_slice(&make_directory_entry(
            b".          ",
            ATTR_DIRECTORY,
            node.cluster,
            0,
        ));
        entries.extend_from_slice(&make_directory_entry(
            b"..         ",
            ATTR_DIRECTORY,
            parent_cluster.unwrap_or(0),
            0,
        ));
    }
    for child in &node.children {
        if child.needs_lfn {
            append_lfn_entries(&mut entries, &child.name, &child.short_name);
        }
        entries.extend_from_slice(&make_directory_entry(
            &child.short_name,
            if child.is_dir {
                ATTR_DIRECTORY
            } else {
                ATTR_READ_ONLY
            },
            child.cluster,
            u32::try_from(child.data.len()).unwrap_or(u32::MAX),
        ));
    }
    entries.push(0x00);
    let capacity = node.clusters.max(1) as usize * layout.cluster_size;
    if entries.len() > capacity {
        return Err(invalid_fat32("directory entries exceed allocated clusters"));
    }
    let offset = data_offset + (node.cluster - FAT32_ROOT_CLUSTER) as usize * layout.cluster_size;
    if offset + entries.len() > image.len() {
        return Err(invalid_fat32("directory data exceeds the image"));
    }
    image[offset..offset + entries.len()].copy_from_slice(&entries);
    for child in &node.children {
        if child.is_dir {
            write_directory(image, layout, data_offset, child, Some(node.cluster))?;
        }
    }
    Ok(())
}

fn make_directory_entry(
    name: &[u8; FAT32_MAX_LABEL],
    attr: u8,
    cluster: u32,
    size: u32,
) -> [u8; DIR_ENTRY_SIZE] {
    let mut entry = [0u8; DIR_ENTRY_SIZE];
    entry[..FAT32_MAX_LABEL].copy_from_slice(name);
    entry[11] = attr;
    entry[12] = 0;
    entry[13] = 0;
    put_u16(&mut entry, 14, 0);
    put_u16(&mut entry, 16, 0x6000);
    put_u16(&mut entry, 18, 0x5021);
    put_u16(&mut entry, 20, (cluster >> 16) as u16);
    put_u16(&mut entry, 22, 0);
    put_u16(&mut entry, 24, 0x5021);
    put_u16(&mut entry, 26, (cluster & 0xFFFF) as u16);
    put_u32(&mut entry, 28, size);
    entry
}

fn append_lfn_entries(entries: &mut Vec<u8>, name: &str, short_name: &[u8; FAT32_MAX_LABEL]) {
    let mut units: Vec<u16> = name.encode_utf16().collect();
    units.push(0);
    while units.len() % 13 != 0 {
        units.push(0xFFFF);
    }
    let count = units.len() / 13;
    let checksum = short_name_checksum(short_name);
    for index in (1..=count).rev() {
        let chunk = &units[(index - 1) * 13..index * 13];
        let mut entry = [0u8; DIR_ENTRY_SIZE];
        let mut sequence = index as u8;
        if index == count {
            sequence |= 0x40;
        }
        entry[0] = sequence;
        for offset in 0..5 {
            put_u16(&mut entry, 1 + offset * 2, chunk[offset]);
        }
        entry[11] = ATTR_LFN;
        entry[12] = 0;
        entry[13] = checksum;
        for offset in 0..6 {
            put_u16(&mut entry, 14 + offset * 2, chunk[5 + offset]);
        }
        put_u16(&mut entry, 26, 0);
        put_u16(&mut entry, 28, chunk[11]);
        put_u16(&mut entry, 30, chunk[12]);
        entries.extend_from_slice(&entry);
    }
}

fn short_name_checksum(name: &[u8; FAT32_MAX_LABEL]) -> u8 {
    name.iter().fold(0u8, |checksum, byte| {
        checksum.rotate_right(1).wrapping_add(*byte)
    })
}

fn lfn_entry_count(name: &str) -> usize {
    let units = name.encode_utf16().count() + 1;
    units.div_ceil(13).max(1)
}

fn make_short_name(
    name: &str,
    used: &mut BTreeSet<[u8; FAT32_MAX_LABEL]>,
) -> ([u8; FAT32_MAX_LABEL], bool) {
    let upper = name.to_ascii_uppercase();
    let (base, extension) = match upper.rsplit_once('.') {
        Some((base, extension)) if !base.is_empty() => (base, extension),
        _ => (upper.as_str(), ""),
    };
    let mut base = clean_short_component(base, 8);
    let extension = clean_short_component(extension, 3);
    if base.is_empty() {
        base = "FILE".to_owned();
    }

    let mut candidate = make_83(&base, &extension);
    let mut suffix = 1u32;
    while used.contains(&candidate) {
        let suffix_text = format!("~{suffix}");
        let keep = base.len().min(8usize.saturating_sub(suffix_text.len()));
        let trimmed = &base[..keep];
        candidate = make_83(&format!("{trimmed}{suffix_text}"), &extension);
        suffix += 1;
        if suffix > 999_999 {
            candidate = make_83("FILE~999", &extension);
            break;
        }
    }
    used.insert(candidate);

    let display = display_short_name(&candidate);
    let needs_lfn = name != display;
    (candidate, needs_lfn)
}

fn clean_short_component(value: &str, max: usize) -> String {
    let mut cleaned = String::new();
    for character in value.chars() {
        let character = if character.is_ascii_alphanumeric() || matches!(character, '_' | '-') {
            character
        } else {
            '_'
        };
        cleaned.push(character);
        if cleaned.len() == max {
            break;
        }
    }
    cleaned
}

fn make_83(base: &str, extension: &str) -> [u8; FAT32_MAX_LABEL] {
    let mut name = [b' '; FAT32_MAX_LABEL];
    for (index, byte) in base.bytes().take(8).enumerate() {
        name[index] = byte.to_ascii_uppercase();
    }
    for (index, byte) in extension.bytes().take(3).enumerate() {
        name[8 + index] = byte.to_ascii_uppercase();
    }
    name
}

fn display_short_name(name: &[u8; FAT32_MAX_LABEL]) -> String {
    let base = String::from_utf8_lossy(&name[..8]).trim_end().to_owned();
    let extension = String::from_utf8_lossy(&name[8..]).trim_end().to_owned();
    if extension.is_empty() {
        base
    } else {
        format!("{base}.{extension}")
    }
}

// ---------------------------------------------------------------------------
// Read-only parser helpers
// ---------------------------------------------------------------------------

fn fat_chain(
    fat_entry: impl Fn(u32) -> Option<u32>,
    start: u32,
    total_clusters: u32,
) -> Result<Vec<u32>, UsbError> {
    let mut chain = Vec::new();
    let mut cluster = start;
    let mut seen = BTreeSet::new();
    while cluster >= FAT32_ROOT_CLUSTER
        && cluster < total_clusters.saturating_add(FAT32_ROOT_CLUSTER)
    {
        if !seen.insert(cluster) {
            return Err(invalid_fat32("FAT chain loops"));
        }
        chain.push(cluster);
        if chain.len() > total_clusters as usize {
            return Err(invalid_fat32("FAT chain exceeds volume cluster count"));
        }
        let next = fat_entry(cluster).ok_or_else(|| invalid_fat32("FAT entry outside the FAT"))?;
        if next == FAT_EOC {
            return Ok(chain);
        }
        if next == 0 || next == FAT_BAD || next < FAT32_ROOT_CLUSTER {
            return Err(invalid_fat32("FAT chain contains a free or bad entry"));
        }
        cluster = next;
    }
    Err(invalid_fat32("FAT chain leaves the data region"))
}

fn read_directory(
    bytes: &[u8],
    cluster_size: usize,
    data_start: usize,
    total_clusters: u32,
    chain: &[u32],
    fat_entry: impl Fn(u32) -> Option<u32> + Copy,
) -> Result<Vec<Fat32EntryInfo>, UsbError> {
    let mut entries = Vec::new();
    let mut lfn_parts: Vec<(u8, Vec<u16>)> = Vec::new();

    for cluster in chain {
        let offset = data_start + (*cluster - FAT32_ROOT_CLUSTER) as usize * cluster_size;
        let Some(data) = bytes.get(offset..offset.saturating_add(cluster_size)) else {
            return Err(invalid_fat32("directory cluster is outside the image"));
        };
        for raw in data.chunks_exact(DIR_ENTRY_SIZE) {
            if raw[0] == 0x00 {
                return Ok(entries);
            }
            if raw[0] == 0xE5 {
                continue;
            }
            let attr = raw[11];
            if attr == ATTR_LFN {
                let sequence = raw[0] & 0x1F;
                let mut units = Vec::with_capacity(13);
                for offset in [1, 3, 5, 7, 9] {
                    units.push(u16::from_le_bytes([raw[offset], raw[offset + 1]]));
                }
                for offset in [14, 16, 18, 20, 22, 24] {
                    units.push(u16::from_le_bytes([raw[offset], raw[offset + 1]]));
                }
                for offset in [28, 30] {
                    units.push(u16::from_le_bytes([raw[offset], raw[offset + 1]]));
                }
                lfn_parts.push((sequence, units));
                continue;
            }
            if attr & ATTR_VOLUME_ID != 0 {
                continue;
            }
            let name = decode_short_name(raw);
            if name == "." || name == ".." {
                lfn_parts.clear();
                continue;
            }
            let long_name = assemble_lfn(&lfn_parts);
            lfn_parts.clear();
            let first_cluster = (u32::from(u16::from_le_bytes([raw[20], raw[21]])) << 16)
                | u32::from(u16::from_le_bytes([raw[26], raw[27]]));
            let size_bytes = u32::from_le_bytes([raw[28], raw[29], raw[30], raw[31]]);
            let is_dir = attr & ATTR_DIRECTORY != 0;
            let clusters = if first_cluster >= FAT32_ROOT_CLUSTER {
                fat_chain(fat_entry, first_cluster, total_clusters)?
            } else {
                Vec::new()
            };
            entries.push(Fat32EntryInfo {
                name: long_name.unwrap_or(name),
                is_dir,
                size_bytes,
                first_cluster,
                clusters,
            });
        }
    }
    Ok(entries)
}

fn decode_short_name(raw: &[u8]) -> String {
    let mut base = raw[..8].to_vec();
    let mut extension = raw[8..11].to_vec();
    if base.first() == Some(&0x05) {
        base[0] = 0xE5;
    }
    while base.last() == Some(&b' ') {
        base.pop();
    }
    while extension.last() == Some(&b' ') {
        extension.pop();
    }
    let base = String::from_utf8_lossy(&base).into_owned();
    let extension = String::from_utf8_lossy(&extension).into_owned();
    if extension.is_empty() {
        base
    } else {
        format!("{base}.{extension}")
    }
}

fn assemble_lfn(parts: &[(u8, Vec<u16>)]) -> Option<String> {
    if parts.is_empty() {
        return None;
    }
    let mut sorted = parts.to_vec();
    sorted.sort_by_key(|(sequence, _)| *sequence & 0x1F);
    let mut units = Vec::new();
    for (_, chunk) in sorted {
        units.extend(chunk);
    }
    while units.last() == Some(&0xFFFF) {
        units.pop();
    }
    if units.last() == Some(&0) {
        units.pop();
    }
    if units.is_empty() {
        None
    } else {
        Some(String::from_utf16_lossy(&units))
    }
}

fn collect_entries(node: &Node) -> Vec<Fat32EntryInfo> {
    let mut entries = Vec::new();
    collect_entries_at(node, "", &mut entries);
    entries
}

fn collect_entries_at(node: &Node, prefix: &str, entries: &mut Vec<Fat32EntryInfo>) {
    for child in &node.children {
        let path = if prefix.is_empty() {
            child.name.clone()
        } else {
            format!("{prefix}/{}", child.name)
        };
        let clusters = if child.cluster >= FAT32_ROOT_CLUSTER && child.clusters > 0 {
            (0..child.clusters)
                .map(|offset| child.cluster + offset)
                .collect()
        } else {
            Vec::new()
        };
        entries.push(Fat32EntryInfo {
            name: path.clone(),
            is_dir: child.is_dir,
            size_bytes: child.data.len() as u32,
            first_cluster: child.cluster,
            clusters,
        });
        if child.is_dir {
            collect_entries_at(child, &path, entries);
        }
    }
}

fn read_u16(bytes: &[u8], offset: usize) -> Result<u16, UsbError> {
    let raw = bytes
        .get(offset..offset + 2)
        .ok_or_else(|| invalid_fat32("truncated FAT32 structure"))?;
    Ok(u16::from_le_bytes([raw[0], raw[1]]))
}

fn read_u32(bytes: &[u8], offset: usize) -> Result<u32, UsbError> {
    let raw = bytes
        .get(offset..offset + 4)
        .ok_or_else(|| invalid_fat32("truncated FAT32 structure"))?;
    Ok(u32::from_le_bytes([raw[0], raw[1], raw[2], raw[3]]))
}

fn put_u16(bytes: &mut [u8], offset: usize, value: u16) {
    bytes[offset..offset + 2].copy_from_slice(&value.to_le_bytes());
}

fn put_u32(bytes: &mut [u8], offset: usize, value: u32) {
    bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
}

fn invalid_fat32(reason: impl Into<String>) -> UsbError {
    UsbError::InvalidMedia {
        format: "fat32",
        path: PathBuf::from("<memory>"),
        reason: reason.into(),
    }
}
