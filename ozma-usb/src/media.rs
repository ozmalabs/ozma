// SPDX-License-Identifier: AGPL-3.0-only
//! Virtual media and USB mass-storage LUN models.
//!
//! This is the safe model layer for `softnode/virtual_media.py` and
//! `softnode/usb_mass_storage.py`. It describes a backing image, whether the
//! LUN is a disk or a CD-ROM, whether it is writable, and whether the image is
//! currently attached. It performs **no** mounts, block-device writes, image
//! creation, FAT32 synthesis, or SCSI I/O.

use std::fs;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::config::is_safe_identifier;
use crate::error::UsbError;

/// Backing format declared for a mass-storage image.
///
/// `Raw` is the historical default and imposes no content validation. The
/// other variants add read-only or read-only-media policy checks and, where
/// practical, a bounded content probe immediately before attach.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MediaFormat {
    /// An opaque disk image (the existing behaviour).
    #[default]
    Raw,
    /// An ISO 9660 image; CD-ROM and read-only.
    Iso9660,
    /// A FAT32 image produced by this crate or another tool.
    Fat32,
    /// A local file served through an NBD export.
    Nbd,
}

impl MediaFormat {
    /// Stable lower-case name used in JSON.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Raw => "raw",
            Self::Iso9660 => "iso9660",
            Self::Fat32 => "fat32",
            Self::Nbd => "nbd",
        }
    }

    /// Whether this is the default opaque image format.
    pub fn is_raw(&self) -> bool {
        matches!(self, Self::Raw)
    }

    /// Whether the format requires CD-ROM presentation.
    pub fn requires_cdrom(self) -> bool {
        matches!(self, Self::Iso9660)
    }

    /// Whether the format is intrinsically read-only.
    pub fn is_read_only(self) -> bool {
        matches!(self, Self::Iso9660)
    }

    /// Whether a ConfigFS LUN can use this format directly.
    pub fn configfs_attachable(self) -> bool {
        !matches!(self, Self::Nbd)
    }
}

/// How the kernel should present a mass-storage LUN.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum MediaMode {
    /// A regular removable disk.
    #[default]
    Disk,
    /// A CD-ROM device. ConfigFS requires `cdrom=1` and read-only media.
    CdRom,
}

impl MediaMode {
    /// Value written to the ConfigFS `lun.N/cdrom` attribute.
    pub fn configfs_value(self) -> &'static str {
        match self {
            Self::Disk => "0",
            Self::CdRom => "1",
        }
    }

    /// Whether this mode is a CD-ROM.
    pub fn is_cdrom(self) -> bool {
        matches!(self, Self::CdRom)
    }
}

/// A backing file for one USB mass-storage LUN.
///
/// The path is recorded as supplied and is only checked when the image is
/// attached. Attaching never opens the image for writing, even when
/// `read_only` is false: the kernel gadget driver owns the data path.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MassStorageImage {
    /// Backing image path (normally an absolute path).
    pub path: PathBuf,
    /// Disk or CD-ROM presentation.
    pub mode: MediaMode,
    /// Whether the LUN is exposed read-only.
    pub read_only: bool,
    /// Declared backing format.
    #[serde(default, skip_serializing_if = "MediaFormat::is_raw")]
    pub format: MediaFormat,
    /// Known image size, if the caller already knows it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub size_bytes: Option<u64>,
}

impl MassStorageImage {
    /// A writable removable-disk image.
    pub fn disk(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: MediaMode::Disk,
            read_only: false,
            format: MediaFormat::Raw,
            size_bytes: None,
        }
    }

    /// A read-only CD-ROM image.
    pub fn cdrom(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: MediaMode::CdRom,
            read_only: true,
            format: MediaFormat::Raw,
            size_bytes: None,
        }
    }

    /// A read-only ISO 9660 CD-ROM image.
    pub fn iso(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: MediaMode::CdRom,
            read_only: true,
            format: MediaFormat::Iso9660,
            size_bytes: None,
        }
    }

    /// A read-only FAT32 image presented as a removable disk.
    pub fn fat32(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: MediaMode::Disk,
            read_only: true,
            format: MediaFormat::Fat32,
            size_bytes: None,
        }
    }

    /// A local file exposed through NBD (not attached directly to ConfigFS).
    pub fn nbd(path: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            mode: MediaMode::Disk,
            read_only: true,
            format: MediaFormat::Nbd,
            size_bytes: None,
        }
    }

    /// Set the declared backing format.
    ///
    /// Consistency (for example ISO requires CD-ROM/read-only) is checked by
    /// [`Self::validate`] and by [`Self::with_format`].
    pub fn with_format(mut self, format: MediaFormat) -> Result<Self, UsbError> {
        self.format = format;
        self.validate()?;
        Ok(self)
    }

    /// Set the read-only flag.
    ///
    /// CD-ROM media cannot be made writable; doing so returns
    /// [`UsbError::InvalidConfig`].
    pub fn with_read_only(mut self, read_only: bool) -> Result<Self, UsbError> {
        if self.mode.is_cdrom() && !read_only {
            return Err(UsbError::InvalidConfig(
                "CD-ROM media must be read-only".to_owned(),
            ));
        }
        self.read_only = read_only;
        Ok(self)
    }

    /// Record the known image size.
    pub fn with_size(mut self, size_bytes: u64) -> Result<Self, UsbError> {
        if size_bytes == 0 {
            return Err(UsbError::InvalidConfig(
                "media image size must be greater than zero".to_owned(),
            ));
        }
        self.size_bytes = Some(size_bytes);
        Ok(self)
    }

    /// Validate the shape of the image model without touching the filesystem.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.path.as_os_str().is_empty() {
            return Err(UsbError::InvalidField {
                field: "media.path",
                reason: "image path must not be empty".to_owned(),
            });
        }
        if self.path.as_os_str().to_string_lossy().contains('\0') {
            return Err(UsbError::InvalidField {
                field: "media.path",
                reason: "image path must not contain NUL".to_owned(),
            });
        }
        if self.mode.is_cdrom() && !self.read_only {
            return Err(UsbError::InvalidConfig(
                "CD-ROM media must be read-only".to_owned(),
            ));
        }
        if matches!(self.size_bytes, Some(0)) {
            return Err(UsbError::InvalidConfig(
                "media image size must be greater than zero".to_owned(),
            ));
        }
        match self.format {
            MediaFormat::Raw => {}
            MediaFormat::Iso9660 => {
                if !self.mode.is_cdrom() {
                    return Err(UsbError::UnsupportedMediaFormat {
                        format: self.format.as_str(),
                        reason: "ISO 9660 media must use CD-ROM presentation".to_owned(),
                    });
                }
                if !self.read_only {
                    return Err(UsbError::UnsupportedMediaFormat {
                        format: self.format.as_str(),
                        reason: "ISO 9660 media must be read-only".to_owned(),
                    });
                }
            }
            MediaFormat::Fat32 => {
                if self.mode.is_cdrom() {
                    return Err(UsbError::UnsupportedMediaFormat {
                        format: self.format.as_str(),
                        reason: "FAT32 media must use disk presentation".to_owned(),
                    });
                }
            }
            MediaFormat::Nbd => {
                if self.mode.is_cdrom() {
                    return Err(UsbError::UnsupportedMediaFormat {
                        format: self.format.as_str(),
                        reason: "NBD media must use disk presentation".to_owned(),
                    });
                }
            }
        }
        Ok(())
    }

    /// Validate the model and require the backing file to exist.
    ///
    /// This is called immediately before a ConfigFS `file` attribute is
    /// written. It never opens the image for writing or mounts it; ISO/FAT32
    /// format probes read a small bounded header.
    pub fn validate_for_attach(&self) -> Result<u64, UsbError> {
        self.validate()?;
        if !self.format.configfs_attachable() {
            return Err(UsbError::UnsupportedMediaFormat {
                format: self.format.as_str(),
                reason: "NBD exports are served by the NBD layer and are not a ConfigFS LUN file"
                    .to_owned(),
            });
        }
        let metadata = fs::metadata(&self.path).map_err(|source| {
            if source.kind() == std::io::ErrorKind::NotFound {
                UsbError::MediaNotFound(self.path.clone())
            } else {
                crate::error::io_error("stat media image", self.path.clone(), source)
            }
        })?;
        if !metadata.is_file() {
            return Err(UsbError::MediaNotFile(self.path.clone()));
        }
        let size = metadata.len();
        if size == 0 {
            return Err(UsbError::InvalidConfig(format!(
                "media image {} is empty",
                self.path.display()
            )));
        }
        match self.format {
            MediaFormat::Iso9660 => {
                crate::iso::validate_iso_image(&self.path)?;
            }
            MediaFormat::Fat32 => {
                crate::fat32::validate_fat32_boot_sector(&self.path)?;
            }
            MediaFormat::Raw | MediaFormat::Nbd => {}
        }
        Ok(size)
    }

    /// ConfigFS `ro` value.
    pub fn configfs_ro(&self) -> &'static str {
        if self.read_only {
            "1"
        } else {
            "0"
        }
    }

    /// ConfigFS `cdrom` value.
    pub fn configfs_cdrom(&self) -> &'static str {
        self.mode.configfs_value()
    }

    /// Size to report in status, preferring an explicit model value.
    pub fn effective_size(&self) -> Option<u64> {
        self.size_bytes.or_else(|| {
            fs::metadata(&self.path)
                .ok()
                .filter(|metadata| metadata.is_file())
                .map(|metadata| metadata.len())
        })
    }

    /// Build an attached state snapshot from this image.
    pub fn attached_state(&self) -> MediaState {
        MediaState::Attached {
            path: self.path.clone(),
            mode: self.mode,
            read_only: self.read_only,
            size_bytes: self.effective_size(),
        }
    }
}

/// Attached/detached state of one virtual-media LUN.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum MediaState {
    /// No image is attached; the LUN is empty.
    Detached,
    /// An image is attached.
    Attached {
        /// Backing image path reported by the gadget.
        path: PathBuf,
        /// Disk or CD-ROM.
        mode: MediaMode,
        /// Read-only flag reported by the gadget.
        read_only: bool,
        /// Size, when known.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size_bytes: Option<u64>,
    },
}

impl MediaState {
    /// Whether an image is currently attached.
    pub fn is_attached(&self) -> bool {
        matches!(self, Self::Attached { .. })
    }
}

/// A runtime mass-storage function plus its media attachment state.
///
/// The `function` is the ConfigFS instance name (`usb0`) or the full
/// `mass_storage.usb0` name; both are accepted by the controllers.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VirtualMedia {
    /// Mass-storage function instance.
    pub function: String,
    /// Backing image model.
    pub image: MassStorageImage,
    /// Last observed attach state.
    pub state: MediaState,
}

impl VirtualMedia {
    /// An initially detached writable disk.
    pub fn disk(function: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            function: function.into(),
            image: MassStorageImage::disk(path),
            state: MediaState::Detached,
        }
    }

    /// An initially detached read-only CD-ROM.
    pub fn cdrom(function: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            function: function.into(),
            image: MassStorageImage::cdrom(path),
            state: MediaState::Detached,
        }
    }

    /// An initially detached read-only ISO CD-ROM.
    pub fn iso(function: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            function: function.into(),
            image: MassStorageImage::iso(path),
            state: MediaState::Detached,
        }
    }

    /// An initially detached read-only FAT32 disk.
    pub fn fat32(function: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            function: function.into(),
            image: MassStorageImage::fat32(path),
            state: MediaState::Detached,
        }
    }

    /// An initially detached NBD-backed media descriptor.
    pub fn nbd(function: impl Into<String>, path: impl Into<PathBuf>) -> Self {
        Self {
            function: function.into(),
            image: MassStorageImage::nbd(path),
            state: MediaState::Detached,
        }
    }

    /// Set the image read-only flag.
    pub fn with_read_only(mut self, read_only: bool) -> Result<Self, UsbError> {
        self.image = self.image.with_read_only(read_only)?;
        Ok(self)
    }

    /// Set the declared image format.
    pub fn with_format(mut self, format: MediaFormat) -> Result<Self, UsbError> {
        self.image = self.image.with_format(format)?;
        Ok(self)
    }

    /// Record the image size.
    pub fn with_size(mut self, size_bytes: u64) -> Result<Self, UsbError> {
        self.image = self.image.with_size(size_bytes)?;
        Ok(self)
    }

    /// Validate the function name and image model.
    pub fn validate(&self) -> Result<(), UsbError> {
        if !is_safe_identifier(&self.function) {
            return Err(UsbError::InvalidField {
                field: "media.function",
                reason: format!(
                    "{:?} must be a non-empty ASCII name containing only letters, digits, '-' or '_'",
                    self.function
                ),
            });
        }
        self.image.validate()
    }

    /// Whether media is currently attached.
    pub fn is_attached(&self) -> bool {
        self.state.is_attached()
    }

    /// Validate the backing file and mark this model attached.
    ///
    /// This does not perform the gadget write; controllers call
    /// [`UsbGadgetController::attach_media`](crate::UsbGadgetController::attach_media)
    /// and then mirror the returned state.
    pub fn attach(&mut self) -> Result<MediaState, UsbError> {
        self.validate()?;
        self.image.validate_for_attach()?;
        let state = self.image.attached_state();
        self.state = state.clone();
        Ok(state)
    }

    /// Mark this model detached.
    pub fn detach(&mut self) -> MediaState {
        self.state = MediaState::Detached;
        self.state.clone()
    }

    /// Build the matching [`UsbFunction::MassStorage`](crate::UsbFunction) entry.
    pub fn as_function(&self) -> crate::config::UsbFunction {
        crate::config::UsbFunction::MassStorage {
            instance: self.function.clone(),
            image: Some(self.image.clone()),
            removable: true,
        }
    }

    /// Full ConfigFS function directory name (`mass_storage.<instance>`).
    pub fn configfs_function_name(&self) -> String {
        format!("mass_storage.{}", self.function)
    }
}

/// Resolve a mass-storage function argument to its full ConfigFS name.
///
/// Accepts either `usb0` or `mass_storage.usb0`. The instance must be a safe
/// identifier, which prevents path traversal and accidental function types.
pub(crate) fn resolve_mass_storage_function(function: &str) -> Result<String, UsbError> {
    let function = function.trim();
    if let Some(instance) = function.strip_prefix("mass_storage.") {
        if is_safe_identifier(instance) {
            return Ok(function.to_owned());
        }
    } else if is_safe_identifier(function) {
        return Ok(format!("mass_storage.{function}"));
    }
    Err(UsbError::InvalidField {
        field: "function",
        reason: format!(
            "{function:?} must be a mass-storage instance name containing only letters, digits, '-' or '_'"
        ),
    })
}

impl From<&VirtualMedia> for PathBuf {
    fn from(media: &VirtualMedia) -> Self {
        media.image.path.clone()
    }
}

impl AsRef<Path> for VirtualMedia {
    fn as_ref(&self) -> &Path {
        &self.image.path
    }
}
