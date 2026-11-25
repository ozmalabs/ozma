// SPDX-License-Identifier: AGPL-3.0-only
//! Linux ConfigFS composite-gadget controller.
//!
//! The controller performs ordinary `std::fs` operations against an injectable
//! root (default `/sys/kernel/config/usb_gadget`). Tests point it at a
//! temporary directory, so the default suite never creates a real gadget,
//! binds a UDC, mounts a filesystem, or writes a block device.
//!
//! ConfigFS layout created for a gadget:
//!
//! ```text
//! <root>/<name>/
//!   idVendor  idProduct  bcdDevice  bcdUSB  UDC
//!   strings/0x409/{manufacturer,product,serialnumber}
//!   configs/c.1/
//!     MaxPower
//!     strings/0x409/configuration
//!     <function-name> -> ../../functions/<function-name>
//!   functions/
//!     hid.kbd/{protocol,subclass,report_length,report_desc}
//!     mass_storage.usb0/{stall,lun.0/{file,ro,cdrom,removable}}
//!     uac2.usb0/{p_chmask,c_chmask,p_srate,c_srate,p_ssize,c_ssize,req_number}
//!     ffs.vendor0/            (FunctionFS descriptors come from userspace)
//! ```

use std::fs;
use std::io::ErrorKind;
use std::path::{Path, PathBuf};

use crate::config::{
    is_safe_identifier, is_safe_udc, UsbFunction, UsbFunctionKind, UsbGadgetConfig,
};
use crate::controller::UsbGadgetController;
use crate::error::{configfs_error, UsbError};
use crate::functionfs::{FunctionFsConfig, FunctionFsStatus};
use crate::media::{
    resolve_mass_storage_function, MassStorageImage, MediaMode, MediaState, VirtualMedia,
};
use crate::status::{FunctionStatus, GadgetStatus, UsbGadgetCapabilities};
use crate::uac2::{Uac2Config, Uac2Status};

/// Default ConfigFS composite-gadget root.
pub const DEFAULT_CONFIGFS_ROOT: &str = "/sys/kernel/config/usb_gadget";

const CONFIG_NAME: &str = "c.1";
const LANGUAGE_ID: &str = "0x409";

/// A safe Linux ConfigFS gadget controller.
#[derive(Debug, Clone)]
pub struct ConfigFsUsbController {
    root: PathBuf,
}

impl Default for ConfigFsUsbController {
    fn default() -> Self {
        Self::with_default_root()
    }
}

impl ConfigFsUsbController {
    /// Create a controller rooted at `root`.
    ///
    /// Relative roots are made absolute against the current directory so that
    /// configuration symlink targets are unambiguous to ConfigFS.
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let root = root.into();
        let root = if root.is_absolute() {
            root
        } else {
            std::env::current_dir()
                .map(|cwd| cwd.join(&root))
                .unwrap_or(root)
        };
        Self { root }
    }

    /// Use `/sys/kernel/config/usb_gadget`.
    pub fn with_default_root() -> Self {
        Self::new(DEFAULT_CONFIGFS_ROOT)
    }

    /// The injected root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether `root` currently exists as a directory.
    pub fn probe(root: impl AsRef<Path>) -> bool {
        root.as_ref().is_dir()
    }

    /// Path to one gadget directory.
    pub fn gadget_path(&self, name: &str) -> PathBuf {
        self.root.join(name)
    }

    fn ensure_root(&self) -> Result<(), UsbError> {
        match fs::metadata(&self.root) {
            Ok(metadata) if metadata.is_dir() => Ok(()),
            Ok(_) => Err(UsbError::RootNotDirectory(self.root.clone())),
            Err(source) if source.kind() == ErrorKind::NotFound => {
                Err(UsbError::MissingRoot(self.root.clone()))
            }
            Err(source) => Err(configfs_error(
                "stat ConfigFS root",
                self.root.clone(),
                source,
            )),
        }
    }

    fn existing_gadget(&self, name: &str) -> Result<PathBuf, UsbError> {
        validate_gadget_name(name)?;
        let dir = self.gadget_path(name);
        if dir.is_dir() {
            Ok(dir)
        } else {
            Err(UsbError::MissingGadget(name.to_owned()))
        }
    }

    fn create_directory(&self, path: &Path) -> Result<(), UsbError> {
        match fs::create_dir(path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == ErrorKind::AlreadyExists && path.is_dir() => Ok(()),
            Err(source) => Err(configfs_error(
                "create directory",
                path.to_path_buf(),
                source,
            )),
        }
    }

    fn write_attr(&self, path: &Path, value: impl AsRef<[u8]>) -> Result<(), UsbError> {
        fs::write(path, value)
            .map_err(|source| configfs_error("write attribute", path.to_path_buf(), source))
    }

    fn read_attr(&self, path: &Path) -> Result<String, UsbError> {
        fs::read_to_string(path)
            .map(|value| value.trim().to_owned())
            .map_err(|source| configfs_error("read attribute", path.to_path_buf(), source))
    }

    fn read_attr_opt(&self, path: &Path) -> Option<String> {
        fs::read_to_string(path)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
    }

    fn create_config_link(&self, gadget: &str, function_name: &str) -> Result<(), UsbError> {
        let gadget_dir = self.gadget_path(gadget);
        let target = gadget_dir.join("functions").join(function_name);
        let link = gadget_dir
            .join("configs")
            .join(CONFIG_NAME)
            .join(function_name);
        create_symlink(&target, &link)
            .map_err(|source| configfs_error("create function link", link.clone(), source))
    }

    fn populate_gadget(&self, config: &UsbGadgetConfig) -> Result<(), UsbError> {
        let dir = self.gadget_path(&config.name);
        self.create_directory(&dir)?;

        self.write_attr(&dir.join("idVendor"), format!("0x{:04x}", config.id_vendor))?;
        self.write_attr(
            &dir.join("idProduct"),
            format!("0x{:04x}", config.id_product),
        )?;
        self.write_attr(
            &dir.join("bcdDevice"),
            format!("0x{:04x}", config.bcd_device),
        )?;
        self.write_attr(&dir.join("bcdUSB"), format!("0x{:04x}", config.bcd_usb))?;

        let strings = dir.join("strings");
        self.create_directory(&strings)?;
        let language = strings.join(LANGUAGE_ID);
        self.create_directory(&language)?;
        self.write_attr(&language.join("manufacturer"), &config.manufacturer)?;
        self.write_attr(&language.join("product"), &config.product)?;
        self.write_attr(&language.join("serialnumber"), &config.serial_number)?;

        let configs = dir.join("configs");
        self.create_directory(&configs)?;
        let config_dir = configs.join(CONFIG_NAME);
        self.create_directory(&config_dir)?;
        let config_strings = config_dir.join("strings");
        self.create_directory(&config_strings)?;
        let config_language = config_strings.join(LANGUAGE_ID);
        self.create_directory(&config_language)?;
        self.write_attr(
            &config_language.join("configuration"),
            &config.configuration,
        )?;
        self.write_attr(
            &config_dir.join("MaxPower"),
            config.max_power_ma.to_string(),
        )?;

        let functions = dir.join("functions");
        self.create_directory(&functions)?;
        for function in &config.functions {
            self.create_function(&functions, function)?;
            self.create_config_link(&config.name, &function.configfs_name())?;
        }

        Ok(())
    }

    fn create_function(
        &self,
        functions_root: &Path,
        function: &UsbFunction,
    ) -> Result<(), UsbError> {
        let dir = functions_root.join(function.configfs_name());
        self.create_directory(&dir)?;

        match function {
            UsbFunction::Hid {
                protocol,
                subclass,
                report_length,
                report_descriptor,
                ..
            } => {
                self.write_attr(&dir.join("protocol"), protocol.configfs_value().to_string())?;
                self.write_attr(&dir.join("subclass"), subclass.to_string())?;
                self.write_attr(&dir.join("report_length"), report_length.to_string())?;
                self.write_attr(&dir.join("report_desc"), report_descriptor)?;
            }
            UsbFunction::MassStorage {
                image, removable, ..
            } => {
                self.write_attr(&dir.join("stall"), "1")?;
                let lun = dir.join("lun.0");
                self.create_directory(&lun)?;
                self.write_attr(&lun.join("removable"), if *removable { "1" } else { "0" })?;
                if let Some(image) = image {
                    self.configure_lun(&lun, image)?;
                } else {
                    self.write_attr(&lun.join("ro"), "0")?;
                    self.write_attr(&lun.join("cdrom"), "0")?;
                    self.write_attr(&lun.join("file"), "")?;
                }
            }
            UsbFunction::Uac2 { config, .. } => {
                self.write_attr(
                    &dir.join("p_chmask"),
                    Uac2Config::channel_mask(config.playback_channels).to_string(),
                )?;
                self.write_attr(
                    &dir.join("c_chmask"),
                    Uac2Config::channel_mask(config.capture_channels).to_string(),
                )?;
                self.write_attr(&dir.join("p_srate"), config.sample_rate_hz.to_string())?;
                self.write_attr(&dir.join("c_srate"), config.sample_rate_hz.to_string())?;
                let sample_size = config.sample_size_bytes();
                self.write_attr(&dir.join("p_ssize"), sample_size.to_string())?;
                self.write_attr(&dir.join("c_ssize"), sample_size.to_string())?;
                self.write_attr(&dir.join("req_number"), "2")?;
            }
            UsbFunction::Vendor { .. } => {
                // ffs.<instance> is a FunctionFS mount point. Descriptors and
                // endpoints are supplied by the userspace daemon, not ConfigFS.
            }
        }

        Ok(())
    }

    fn configure_lun(&self, lun: &Path, image: &MassStorageImage) -> Result<(), UsbError> {
        image.validate_for_attach()?;
        self.write_attr(&lun.join("ro"), image.configfs_ro())?;
        self.write_attr(&lun.join("cdrom"), image.configfs_cdrom())?;
        self.write_attr(&lun.join("file"), image.path.to_string_lossy().as_bytes())?;
        Ok(())
    }

    fn read_lun_state(&self, lun: &Path) -> Result<MediaState, UsbError> {
        let Some(file) = self.read_attr_opt(&lun.join("file")) else {
            return Ok(MediaState::Detached);
        };
        let read_only = self
            .read_attr(&lun.join("ro"))
            .is_ok_and(|value| value == "1");
        let cdrom = self
            .read_attr(&lun.join("cdrom"))
            .is_ok_and(|value| value == "1");
        let path = PathBuf::from(file);
        let size_bytes = fs::metadata(&path)
            .ok()
            .filter(|metadata| metadata.is_file())
            .map(|metadata| metadata.len());
        Ok(MediaState::Attached {
            path,
            mode: if cdrom {
                MediaMode::CdRom
            } else {
                MediaMode::Disk
            },
            read_only,
            size_bytes,
        })
    }

    fn read_uac2_status(&self, function_dir: &Path, instance: &str) -> Option<Uac2Status> {
        let playback_channels = self
            .read_attr_opt(&function_dir.join("p_chmask"))
            .and_then(|value| value.parse::<u32>().ok())
            .map(|mask| mask.count_ones() as u8)?;
        let capture_channels = self
            .read_attr_opt(&function_dir.join("c_chmask"))
            .and_then(|value| value.parse::<u32>().ok())
            .map(|mask| mask.count_ones() as u8)?;
        if playback_channels == 0 && capture_channels == 0 {
            return None;
        }
        let (rate_key, size_key) = if playback_channels > 0 {
            ("p_srate", "p_ssize")
        } else {
            ("c_srate", "c_ssize")
        };
        let sample_rate_hz = self
            .read_attr_opt(&function_dir.join(rate_key))
            .and_then(|value| value.parse::<u32>().ok())?;
        let bit_depth = self
            .read_attr_opt(&function_dir.join(size_key))
            .and_then(|value| value.parse::<u8>().ok())
            .and_then(|bytes| bytes.checked_mul(8))?;
        let config = Uac2Config::new(
            playback_channels,
            capture_channels,
            sample_rate_hz,
            bit_depth,
        );
        if config.validate().is_err() {
            return None;
        }
        Some(Uac2Status::from_config(instance, &config))
    }

    /// Status for one FunctionFS function, using the model's daemon path and
    /// readiness marker without executing the daemon.
    pub fn function_fs_status(
        &self,
        gadget: &str,
        config: &FunctionFsConfig,
    ) -> Result<FunctionFsStatus, UsbError> {
        config.validate()?;
        let gadget_dir = self.existing_gadget(gadget)?;
        let function = config.configfs_name();
        let dir = gadget_dir.join("functions").join(&function);
        if !dir.is_dir() {
            return Err(UsbError::MissingFunction {
                gadget: gadget.to_owned(),
                function,
            });
        }
        Ok(config.status())
    }

    /// Observed UAC2 ConfigFS attributes for one instance or full function
    /// name (`audio` or `uac2.audio`).
    pub fn uac2_status(&self, gadget: &str, function: &str) -> Result<Uac2Status, UsbError> {
        let name = resolve_function_name(function, "uac2")?;
        let gadget_dir = self.existing_gadget(gadget)?;
        let dir = gadget_dir.join("functions").join(&name);
        if !dir.is_dir() {
            return Err(UsbError::MissingFunction {
                gadget: gadget.to_owned(),
                function: name,
            });
        }
        let instance = name
            .split_once('.')
            .map(|(_, instance)| instance)
            .unwrap_or(name.as_str());
        self.read_uac2_status(&dir, instance)
            .ok_or_else(|| UsbError::InvalidField {
                field: "function.uac2",
                reason: format!("ConfigFS function {name:?} has no usable UAC2 attributes"),
            })
    }

    fn remove_file(&self, path: &Path) -> Result<(), UsbError> {
        match fs::remove_file(path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) => Err(configfs_error("remove file", path.to_path_buf(), source)),
        }
    }

    fn remove_default_group(&self, path: &Path) -> Result<(), UsbError> {
        match self.remove_tree(path) {
            // ConfigFS owns the `strings` default group and removes it with its
            // gadget/configuration; an explicit rmdir returns EPERM. The group
            // is gone once the parent directory is removed, so this is benign.
            Err(UsbError::ConfigFs { source, .. })
                if source.kind() == ErrorKind::PermissionDenied =>
            {
                Ok(())
            }
            other => other,
        }
    }

    /// Remove a directory with a regular-filesystem fallback.
    ///
    /// On ConfigFS, `rmdir` removes a function/configuration and its virtual
    /// children directly. On a test root the directory is real, so an
    /// `ENOTEMPTY` result descends and removes regular children. This fallback
    /// is never used for a ConfigFS directory that `rmdir` accepts.
    fn remove_tree(&self, path: &Path) -> Result<(), UsbError> {
        match fs::remove_dir(path) {
            Ok(()) => Ok(()),
            Err(source) if source.kind() == ErrorKind::NotFound => Ok(()),
            Err(source) if source.kind() == ErrorKind::DirectoryNotEmpty => {
                let entries = fs::read_dir(path).map_err(|source| {
                    configfs_error("list directory", path.to_path_buf(), source)
                })?;
                for entry in entries {
                    let entry = entry.map_err(|source| {
                        configfs_error("list directory entry", path.to_path_buf(), source)
                    })?;
                    let child = entry.path();
                    let file_type = entry.file_type().map_err(|source| {
                        configfs_error("stat directory entry", child.clone(), source)
                    })?;
                    if file_type.is_dir() {
                        self.remove_tree(&child)?;
                    } else {
                        self.remove_file(&child)?;
                    }
                }
                fs::remove_dir(path).map_err(|source| {
                    configfs_error("remove directory", path.to_path_buf(), source)
                })
            }
            Err(source) => Err(configfs_error(
                "remove directory",
                path.to_path_buf(),
                source,
            )),
        }
    }

    fn function_path(&self, gadget: &str, function: &str) -> Result<PathBuf, UsbError> {
        let gadget_dir = self.existing_gadget(gadget)?;
        let name = resolve_mass_storage_function(function)?;
        let path = gadget_dir.join("functions").join(&name);
        if path.is_dir() {
            Ok(path)
        } else {
            Err(UsbError::MissingFunction {
                gadget: gadget.to_owned(),
                function: name,
            })
        }
    }
}

impl UsbGadgetController for ConfigFsUsbController {
    fn create_gadget(&self, config: &UsbGadgetConfig) -> Result<GadgetStatus, UsbError> {
        config.validate()?;
        self.ensure_root()?;
        let dir = self.gadget_path(&config.name);
        if dir.exists() {
            return Err(UsbError::GadgetExists(config.name.clone()));
        }
        if let Err(error) = self.populate_gadget(config) {
            let _ = self.remove_gadget(&config.name);
            return Err(error);
        }
        self.status(&config.name)
    }

    fn remove_gadget(&self, name: &str) -> Result<(), UsbError> {
        let dir = self.existing_gadget(name)?;
        let mut first_error: Option<UsbError> = None;

        if let Ok(udc) = self.read_attr(&dir.join("UDC")) {
            if !udc.is_empty() {
                // ConfigFS ignores a zero-byte write; clearing the UDC needs a
                // newline, exactly like `echo '' > UDC`.
                if let Err(error) = self.write_attr(&dir.join("UDC"), "\n") {
                    first_error.get_or_insert(error);
                }
            }
        }

        let configs = dir.join("configs");
        if configs.is_dir() {
            if let Ok(entries) = fs::read_dir(&configs) {
                for entry in entries.flatten() {
                    let config = entry.path();
                    if !config.is_dir() {
                        continue;
                    }
                    if let Ok(children) = fs::read_dir(&config) {
                        for child in children.flatten() {
                            if child
                                .file_type()
                                .map(|file_type| file_type.is_symlink())
                                .unwrap_or(false)
                            {
                                if let Err(error) = self.remove_file(&child.path()) {
                                    first_error.get_or_insert(error);
                                }
                            }
                        }
                    }
                    if let Err(error) = self.remove_tree(&config.join("strings").join(LANGUAGE_ID))
                    {
                        first_error.get_or_insert(error);
                    }
                    if let Err(error) = self.remove_default_group(&config.join("strings")) {
                        first_error.get_or_insert(error);
                    }
                    if let Err(error) = self.remove_tree(&config) {
                        first_error.get_or_insert(error);
                    }
                }
            }
        }

        let functions = dir.join("functions");
        if functions.is_dir() {
            if let Ok(entries) = fs::read_dir(&functions) {
                for entry in entries.flatten() {
                    if entry
                        .file_type()
                        .map(|file_type| file_type.is_dir())
                        .unwrap_or(false)
                    {
                        if let Err(error) = self.remove_tree(&entry.path()) {
                            first_error.get_or_insert(error);
                        }
                    }
                }
            }
        }

        if let Err(error) = self.remove_tree(&dir.join("strings").join(LANGUAGE_ID)) {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.remove_default_group(&dir.join("strings")) {
            first_error.get_or_insert(error);
        }
        if let Err(error) = self.remove_tree(&dir) {
            first_error.get_or_insert(error);
        }

        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn bind(&self, name: &str, udc: &str) -> Result<GadgetStatus, UsbError> {
        validate_gadget_name(name)?;
        if !is_safe_udc(udc) {
            return Err(UsbError::InvalidField {
                field: "udc",
                reason: format!("{udc:?} is not a valid UDC name"),
            });
        }
        let dir = self.existing_gadget(name)?;
        let current = self.read_attr_opt(&dir.join("UDC"));
        if let Some(current) = current {
            if current == udc {
                return self.status(name);
            }
            return Err(UsbError::AlreadyBound {
                gadget: name.to_owned(),
                udc: current,
            });
        }
        self.write_attr(&dir.join("UDC"), udc)?;
        self.status(name)
    }

    fn unbind(&self, name: &str) -> Result<GadgetStatus, UsbError> {
        let dir = self.existing_gadget(name)?;
        if self.read_attr_opt(&dir.join("UDC")).is_some() {
            // A zero-byte write is a no-op for the ConfigFS UDC attribute; the
            // conventional newline write is what actually unbinds and wakes
            // pending FunctionFS endpoint I/O.
            self.write_attr(&dir.join("UDC"), "\n")?;
        }
        self.status(name)
    }

    fn status(&self, name: &str) -> Result<GadgetStatus, UsbError> {
        let dir = self.existing_gadget(name)?;
        let udc = self.read_attr_opt(&dir.join("UDC"));
        let mut functions = Vec::new();
        let functions_root = dir.join("functions");
        if functions_root.is_dir() {
            let mut entries: Vec<PathBuf> = fs::read_dir(&functions_root)
                .map_err(|source| configfs_error("list functions", functions_root.clone(), source))?
                .filter_map(|entry| entry.ok().map(|entry| entry.path()))
                .filter(|path| path.is_dir())
                .collect();
            entries.sort();
            for path in entries {
                let name = path
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_default();
                let Some(kind) = UsbFunctionKind::from_configfs_name(&name) else {
                    continue;
                };
                let linked =
                    fs::symlink_metadata(dir.join("configs").join(CONFIG_NAME).join(&name))
                        .map(|metadata| metadata.file_type().is_symlink())
                        .unwrap_or(false);
                let instance = name
                    .split_once('.')
                    .map(|(_, instance)| instance)
                    .unwrap_or(name.as_str());
                let media = if kind == UsbFunctionKind::MassStorage {
                    Some(self.read_lun_state(&path.join("lun.0"))?)
                } else {
                    None
                };
                let uac2 = if kind == UsbFunctionKind::Uac2 {
                    self.read_uac2_status(&path, instance)
                } else {
                    None
                };
                let function_fs = (kind == UsbFunctionKind::Vendor)
                    .then(|| FunctionFsStatus::unobserved(instance));
                functions.push(FunctionStatus {
                    name,
                    kind,
                    linked,
                    requires_userspace: kind == UsbFunctionKind::Vendor,
                    media,
                    uac2,
                    function_fs,
                });
            }
        }

        Ok(GadgetStatus {
            name: name.to_owned(),
            present: true,
            bound: udc.is_some(),
            udc,
            functions,
        })
    }

    fn list_gadgets(&self) -> Result<Vec<String>, UsbError> {
        self.ensure_root()?;
        let mut names: Vec<String> = fs::read_dir(&self.root)
            .map_err(|source| configfs_error("list gadgets", self.root.clone(), source))?
            .filter_map(|entry| entry.ok())
            .filter(|entry| entry.path().is_dir())
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .collect();
        names.sort();
        Ok(names)
    }

    fn attach_media(&self, gadget: &str, media: &VirtualMedia) -> Result<MediaState, UsbError> {
        media.validate()?;
        let function_dir = self.function_path(gadget, &media.function)?;
        let size = media.image.validate_for_attach()?;
        let lun = function_dir.join("lun.0");

        // The kernel requires the LUN to be empty while ro/cdrom change.
        if self.read_attr_opt(&lun.join("file")).is_some() {
            self.write_attr(&lun.join("file"), "")?;
        }
        self.write_attr(&lun.join("ro"), media.image.configfs_ro())?;
        self.write_attr(&lun.join("cdrom"), media.image.configfs_cdrom())?;
        self.write_attr(&lun.join("removable"), "1")?;
        self.write_attr(
            &lun.join("file"),
            media.image.path.to_string_lossy().as_bytes(),
        )?;

        Ok(MediaState::Attached {
            path: media.image.path.clone(),
            mode: media.image.mode,
            read_only: media.image.read_only,
            size_bytes: media.image.size_bytes.or(Some(size)),
        })
    }

    fn detach_media(&self, gadget: &str, function: &str) -> Result<MediaState, UsbError> {
        let function_dir = self.function_path(gadget, function)?;
        self.write_attr(&function_dir.join("lun.0").join("file"), "")?;
        Ok(MediaState::Detached)
    }

    fn media_state(&self, gadget: &str, function: &str) -> Result<MediaState, UsbError> {
        let function_dir = self.function_path(gadget, function)?;
        self.read_lun_state(&function_dir.join("lun.0"))
    }

    fn capabilities(&self) -> UsbGadgetCapabilities {
        UsbGadgetCapabilities::at(&self.root)
    }
}

fn resolve_function_name(function: &str, prefix: &str) -> Result<String, UsbError> {
    let function = function.trim();
    if let Some(instance) = function
        .strip_prefix(prefix)
        .and_then(|rest| rest.strip_prefix('.'))
    {
        if is_safe_identifier(instance) {
            return Ok(format!("{prefix}.{instance}"));
        }
    } else if is_safe_identifier(function) {
        return Ok(format!("{prefix}.{function}"));
    }
    Err(UsbError::InvalidField {
        field: "function",
        reason: format!(
            "{function:?} must be a {prefix} instance name containing only letters, digits, '-' or '_'"
        ),
    })
}

fn validate_gadget_name(name: &str) -> Result<(), UsbError> {
    if is_safe_identifier(name) {
        Ok(())
    } else {
        Err(UsbError::InvalidField {
            field: "name",
            reason: format!(
                "{name:?} must be a non-empty ASCII name containing only letters, digits, '-' or '_'"
            ),
        })
    }
}

#[cfg(unix)]
fn create_symlink(target: &Path, link: &Path) -> std::io::Result<()> {
    std::os::unix::fs::symlink(target, link)
}

#[cfg(not(unix))]
fn create_symlink(_target: &Path, _link: &Path) -> std::io::Result<()> {
    Err(std::io::Error::new(
        ErrorKind::Unsupported,
        "configuration symlinks require a Unix host",
    ))
}
