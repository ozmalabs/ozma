//! Opt-in evdev device provisioning for per-VM `input-linux` objects.
//!
//! The supervisor's XML injection can already attach operator-supplied evdev
//! paths. This module closes the next gap for Linux: it creates one virtual
//! keyboard and one virtual pointer through `/dev/uinput`, keeps them alive for
//! the VM lifetime, and returns the real `/dev/input/eventN` paths that QEMU
//! should use.
//!
//! Creation is **opt-in** and best-effort. The real implementation is
//! Linux-only; other targets get a typed [`EvdevProvisionError::Unsupported`].
//! Tests and embedders use [`FakeEvdevProvisioner`], which never opens
//! `/dev/uinput`.
//!
//! Honest scope:
//!
//! * Devices use vendor `0x1209` and products `0x0001` (keyboard) / `0x0002`
//!   (pointer), matching the Python soft node.
//! * A marker file per device (default `/run/ozma/evdev/<vm>.kbd` /
//!   `.mouse`) records the real event path, and a best-effort symlink under
//!   `/dev/input/by-id` provides a stable operator-facing name.
//! * Returned paths are always paths the kernel actually exposed. A failed
//!   creation returns an error and the caller must fall back to no evdev
//!   paths; no path is fabricated.
//! * This does not grab devices in the host, verify QEMU has opened them, or
//!   validate input end-to-end.

use std::path::{Path, PathBuf};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::Mutex;

use thiserror::Error;

/// Default marker directory used by the Python soft node.
pub const DEFAULT_EVDEV_MARKER_DIR: &str = "/run/ozma/evdev";
/// Default stable device directory.
pub const DEFAULT_EVDEV_DEVICE_DIR: &str = "/dev/input/by-id";

/// Real evdev paths for one VM. These are event nodes, not symlinks, so QEMU
/// can open exactly what this process created.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvdevPaths {
    pub keyboard: PathBuf,
    pub mouse: PathBuf,
}

/// Operator-selected evdev creation behaviour.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvdevProvisionConfig {
    /// Directory for `<vm>.kbd` / `<vm>.mouse` marker files.
    pub marker_dir: PathBuf,
    /// Directory for best-effort `ozma-kbd-<vm>` / `ozma-mouse-<vm>` symlinks.
    pub device_dir: PathBuf,
    /// Whether the supervisor should create devices. Off by default.
    pub enabled: bool,
}

impl Default for EvdevProvisionConfig {
    fn default() -> Self {
        Self {
            marker_dir: PathBuf::from(DEFAULT_EVDEV_MARKER_DIR),
            device_dir: PathBuf::from(DEFAULT_EVDEV_DEVICE_DIR),
            enabled: false,
        }
    }
}

impl EvdevProvisionConfig {
    /// Validate paths before any VM is touched. Only called when enabled.
    pub fn validate(&self) -> Result<(), EvdevProvisionError> {
        validate_absolute_dir(&self.marker_dir, "evdev marker_dir")?;
        validate_absolute_dir(&self.device_dir, "evdev device_dir")?;
        Ok(())
    }
}

/// Typed evdev provisioning failure. No path is returned on error.
#[derive(Debug, Clone, PartialEq, Eq, Error)]
pub enum EvdevProvisionError {
    #[allow(dead_code)]
    #[error("evdev device provisioning is not supported on {platform}")]
    Unsupported { platform: &'static str },
    #[error("evdev device provisioning is disabled")]
    Disabled,
    #[error("invalid evdev provisioning configuration: {detail}")]
    InvalidConfig { detail: String },
    #[error("VM name {name:?} is not safe for an evdev device")]
    InvalidVmName { name: String },
    #[error("could not create evdev devices for {vm:?}: {detail}")]
    Create { vm: String, detail: String },
    #[error("evdev device for {vm:?} did not expose a /dev/input/event node")]
    NoEventNode { vm: String },
    #[error("could not write evdev marker {path:?}: {detail}")]
    Marker { path: String, detail: String },
}

/// Provisioning seam used by the supervisor.
pub trait EvdevProvisioner: Send + Sync {
    /// Create or reuse the VM's keyboard and pointer. Returned paths must have
    /// existed at the time of this call.
    fn ensure(&self, vm_name: &str) -> Result<EvdevPaths, EvdevProvisionError>;

    /// Close devices and remove markers/symlinks for a stopped or removed VM.
    /// An unknown VM is a no-op.
    fn release(&self, vm_name: &str);
}

fn validate_absolute_dir(path: &Path, field: &str) -> Result<(), EvdevProvisionError> {
    let text = path
        .to_str()
        .ok_or_else(|| EvdevProvisionError::InvalidConfig {
            detail: format!("{field} is not valid UTF-8"),
        })?;
    if text.is_empty() {
        return Err(EvdevProvisionError::InvalidConfig {
            detail: format!("{field} must not be empty"),
        });
    }
    if !path.is_absolute() {
        return Err(EvdevProvisionError::InvalidConfig {
            detail: format!("{field} must be absolute, got {text:?}"),
        });
    }
    if text.contains('\0') || text.contains('\n') || text.contains('\r') {
        return Err(EvdevProvisionError::InvalidConfig {
            detail: format!("{field} contains a control character"),
        });
    }
    Ok(())
}

/// Local mirror of the libvirt-domain safety rule. Device and marker file
/// names embed the VM name, so it must not traverse paths.
fn validate_vm_name(name: &str) -> Result<(), EvdevProvisionError> {
    let ok = !name.is_empty()
        && name.len() <= 200
        && name != "."
        && name != ".."
        && !name.starts_with('-')
        && name
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-'));
    if ok {
        Ok(())
    } else {
        Err(EvdevProvisionError::InvalidVmName {
            name: name.to_owned(),
        })
    }
}

/// Deterministic in-memory provisioner for tests and embedders. It never opens
/// `/dev/uinput`; returned paths are under `base` and are not created on disk.
#[cfg(test)]
pub struct FakeEvdevProvisioner {
    base: PathBuf,
    fail: AtomicBool,
    ensured: Mutex<Vec<String>>,
    released: Mutex<Vec<String>>,
}

#[cfg(test)]
impl FakeEvdevProvisioner {
    pub fn new(base: impl Into<PathBuf>) -> Self {
        Self {
            base: base.into(),
            fail: AtomicBool::new(false),
            ensured: Mutex::new(Vec::new()),
            released: Mutex::new(Vec::new()),
        }
    }

    /// Make every `ensure` fail with a typed creation error.
    pub fn set_fail(&self, fail: bool) {
        self.fail.store(fail, Ordering::SeqCst);
    }

    /// Names passed to `ensure`, in call order.
    pub fn ensured(&self) -> Vec<String> {
        self.ensured.lock().unwrap().clone()
    }

    /// Names passed to `release`, in call order.
    pub fn released(&self) -> Vec<String> {
        self.released.lock().unwrap().clone()
    }

    fn paths_for(&self, vm_name: &str) -> EvdevPaths {
        EvdevPaths {
            keyboard: self.base.join(format!("{vm_name}-kbd")),
            mouse: self.base.join(format!("{vm_name}-mouse")),
        }
    }
}

#[cfg(test)]
impl EvdevProvisioner for FakeEvdevProvisioner {
    fn ensure(&self, vm_name: &str) -> Result<EvdevPaths, EvdevProvisionError> {
        self.ensured.lock().unwrap().push(vm_name.to_owned());
        if self.fail.load(Ordering::SeqCst) {
            return Err(EvdevProvisionError::Create {
                vm: vm_name.to_owned(),
                detail: "fake evdev failure".to_owned(),
            });
        }
        validate_vm_name(vm_name)?;
        Ok(self.paths_for(vm_name))
    }

    fn release(&self, vm_name: &str) {
        self.released.lock().unwrap().push(vm_name.to_owned());
    }
}

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::HashMap;
    use std::os::unix::fs::symlink;
    use std::path::{Path, PathBuf};
    use std::sync::Mutex;
    use std::time::Duration;

    use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
    use evdev::{
        AbsInfo, AbsoluteAxisType, AttributeSet, BusType, InputId, Key, RelativeAxisType,
        UinputAbsSetup,
    };
    use tracing::{debug, warn};

    use super::{
        validate_vm_name, EvdevPaths, EvdevProvisionConfig, EvdevProvisionError, EvdevProvisioner,
    };

    const VENDOR: u16 = 0x1209;
    const KEYBOARD_PRODUCT: u16 = 0x0001;
    const POINTER_PRODUCT: u16 = 0x0002;
    const VERSION: u16 = 1;
    const HID_ABSOLUTE_MAX: i32 = 32767;

    struct ActiveDevice {
        // Held only to keep the uinput devices open for the VM lifetime.
        _keyboard: VirtualDevice,
        _pointer: VirtualDevice,
        paths: EvdevPaths,
        markers: Vec<PathBuf>,
        links: Vec<PathBuf>,
    }

    /// Linux `/dev/uinput` implementation. Devices stay open in this
    /// provisioner until [`EvdevProvisioner::release`] is called.
    pub struct UinputEvdevProvisioner {
        config: EvdevProvisionConfig,
        active: Mutex<HashMap<String, ActiveDevice>>,
    }

    impl UinputEvdevProvisioner {
        pub fn new(config: EvdevProvisionConfig) -> Self {
            Self {
                config,
                active: Mutex::new(HashMap::new()),
            }
        }

        fn create(&self, vm_name: &str) -> Result<ActiveDevice, EvdevProvisionError> {
            let keyboard_name = format!("ozma-kbd-{vm_name}");
            let pointer_name = format!("ozma-mouse-{vm_name}");

            let mut keyboard =
                create_keyboard(&keyboard_name).map_err(|detail| EvdevProvisionError::Create {
                    vm: vm_name.to_owned(),
                    detail: format!("keyboard: {detail}"),
                })?;
            let mut pointer =
                create_pointer(&pointer_name).map_err(|detail| EvdevProvisionError::Create {
                    vm: vm_name.to_owned(),
                    detail: format!("pointer: {detail}"),
                })?;

            let keyboard_path = event_node(&mut keyboard, vm_name)?;
            let mouse_path = event_node(&mut pointer, vm_name)?;

            std::fs::create_dir_all(&self.config.marker_dir).map_err(|error| {
                EvdevProvisionError::Marker {
                    path: self.config.marker_dir.display().to_string(),
                    detail: error.to_string(),
                }
            })?;

            let keyboard_marker = self.config.marker_dir.join(format!("{vm_name}.kbd"));
            let mouse_marker = self.config.marker_dir.join(format!("{vm_name}.mouse"));
            write_marker(&keyboard_marker, &keyboard_path)?;
            if let Err(error) = write_marker(&mouse_marker, &mouse_path) {
                let _ = std::fs::remove_file(&keyboard_marker);
                return Err(error);
            }

            let mut links = Vec::new();
            if let Some(path) = link_device(&self.config.device_dir, &keyboard_name, &keyboard_path)
            {
                links.push(path);
            }
            if let Some(path) = link_device(&self.config.device_dir, &pointer_name, &mouse_path) {
                links.push(path);
            }

            Ok(ActiveDevice {
                _keyboard: keyboard,
                _pointer: pointer,
                paths: EvdevPaths {
                    keyboard: keyboard_path,
                    mouse: mouse_path,
                },
                markers: vec![keyboard_marker, mouse_marker],
                links,
            })
        }
    }

    impl EvdevProvisioner for UinputEvdevProvisioner {
        fn ensure(&self, vm_name: &str) -> Result<EvdevPaths, EvdevProvisionError> {
            if !self.config.enabled {
                return Err(EvdevProvisionError::Disabled);
            }
            validate_vm_name(vm_name)?;
            self.config.validate()?;

            let mut active = self.active.lock().unwrap();
            if let Some(existing) = active.get(vm_name) {
                return Ok(existing.paths.clone());
            }

            let device = self.create(vm_name)?;
            let paths = device.paths.clone();
            active.insert(vm_name.to_owned(), device);
            Ok(paths)
        }

        fn release(&self, vm_name: &str) {
            let Some(device) = self.active.lock().unwrap().remove(vm_name) else {
                return;
            };

            for marker in &device.markers {
                if let Err(error) = std::fs::remove_file(marker) {
                    if error.kind() != std::io::ErrorKind::NotFound {
                        debug!(
                            "VM {}: could not remove evdev marker {}: {}",
                            vm_name,
                            marker.display(),
                            error
                        );
                    }
                }
            }
            for link in &device.links {
                match std::fs::symlink_metadata(link) {
                    Ok(metadata) if metadata.file_type().is_symlink() => {
                        if let Err(error) = std::fs::remove_file(link) {
                            debug!(
                                "VM {}: could not remove evdev symlink {}: {}",
                                vm_name,
                                link.display(),
                                error
                            );
                        }
                    }
                    Ok(_) => debug!(
                        "VM {}: leaving non-symlink evdev path {} untouched",
                        vm_name,
                        link.display()
                    ),
                    Err(_) => {}
                }
            }
            // Dropping the devices closes /dev/uinput and removes the event
            // nodes; QEMU must already be stopped before release.
            drop(device);
        }
    }

    fn create_keyboard(name: &str) -> Result<VirtualDevice, String> {
        let mut keys = AttributeSet::<Key>::new();
        for code in 1..=248u16 {
            keys.insert(Key::new(code));
        }

        VirtualDeviceBuilder::new()
            .map_err(|error| error.to_string())?
            .name(name)
            .input_id(InputId::new(
                BusType::BUS_USB,
                VENDOR,
                KEYBOARD_PRODUCT,
                VERSION,
            ))
            .with_keys(&keys)
            .map_err(|error| error.to_string())?
            .build()
            .map_err(|error| error.to_string())
    }

    fn create_pointer(name: &str) -> Result<VirtualDevice, String> {
        let mut keys = AttributeSet::<Key>::new();
        for code in [0x110u16, 0x111, 0x112, 0x113, 0x114] {
            keys.insert(Key::new(code));
        }

        let mut relative = AttributeSet::<RelativeAxisType>::new();
        relative.insert(RelativeAxisType::REL_X);
        relative.insert(RelativeAxisType::REL_Y);
        relative.insert(RelativeAxisType::REL_WHEEL);
        relative.insert(RelativeAxisType::REL_HWHEEL);

        let x = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_X,
            AbsInfo::new(0, 0, HID_ABSOLUTE_MAX, 0, 0, 0),
        );
        let y = UinputAbsSetup::new(
            AbsoluteAxisType::ABS_Y,
            AbsInfo::new(0, 0, HID_ABSOLUTE_MAX, 0, 0, 0),
        );

        VirtualDeviceBuilder::new()
            .map_err(|error| error.to_string())?
            .name(name)
            .input_id(InputId::new(
                BusType::BUS_USB,
                VENDOR,
                POINTER_PRODUCT,
                VERSION,
            ))
            .with_keys(&keys)
            .map_err(|error| error.to_string())?
            .with_relative_axes(&relative)
            .map_err(|error| error.to_string())?
            .with_absolute_axis(&x)
            .map_err(|error| error.to_string())?
            .with_absolute_axis(&y)
            .map_err(|error| error.to_string())?
            .build()
            .map_err(|error| error.to_string())
    }

    fn event_node(
        device: &mut VirtualDevice,
        vm_name: &str,
    ) -> Result<PathBuf, EvdevProvisionError> {
        // sysfs normally has the event node immediately, but allow a short
        // window for udev/kernel setup before declaring creation failed.
        for _ in 0..50 {
            if let Ok(mut nodes) = device.enumerate_dev_nodes_blocking() {
                if let Some(Ok(path)) = nodes.next() {
                    return Ok(path);
                }
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        Err(EvdevProvisionError::NoEventNode {
            vm: vm_name.to_owned(),
        })
    }

    fn write_marker(path: &Path, event_path: &Path) -> Result<(), EvdevProvisionError> {
        std::fs::write(path, event_path.to_string_lossy().as_bytes()).map_err(|error| {
            EvdevProvisionError::Marker {
                path: path.display().to_string(),
                detail: error.to_string(),
            }
        })
    }

    fn link_device(device_dir: &Path, name: &str, target: &Path) -> Option<PathBuf> {
        let link = device_dir.join(name);
        if let Err(error) = std::fs::create_dir_all(device_dir) {
            debug!(
                "Could not create evdev device directory {}: {}",
                device_dir.display(),
                error
            );
            return None;
        }

        match std::fs::symlink_metadata(&link) {
            Ok(metadata) if metadata.file_type().is_symlink() => {
                let _ = std::fs::remove_file(&link);
            }
            Ok(_) => {
                warn!(
                    "Leaving non-symlink evdev path {} untouched; not creating {}",
                    link.display(),
                    target.display()
                );
                return None;
            }
            Err(_) => {}
        }

        match symlink(target, &link) {
            Ok(()) => Some(link),
            Err(error) => {
                debug!(
                    "Could not create evdev symlink {} -> {}: {}",
                    link.display(),
                    target.display(),
                    error
                );
                None
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod imp {
    use super::{
        validate_vm_name, EvdevPaths, EvdevProvisionConfig, EvdevProvisionError, EvdevProvisioner,
    };

    /// Non-Linux placeholder. Device creation is Linux-specific.
    pub struct UinputEvdevProvisioner {
        _config: EvdevProvisionConfig,
    }

    impl UinputEvdevProvisioner {
        pub fn new(config: EvdevProvisionConfig) -> Self {
            Self { _config: config }
        }
    }

    impl EvdevProvisioner for UinputEvdevProvisioner {
        fn ensure(&self, vm_name: &str) -> Result<EvdevPaths, EvdevProvisionError> {
            validate_vm_name(vm_name)?;
            Err(EvdevProvisionError::Unsupported {
                platform: std::env::consts::OS,
            })
        }

        fn release(&self, _vm_name: &str) {}
    }
}

pub use imp::UinputEvdevProvisioner;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_opt_in() {
        let config = EvdevProvisionConfig::default();
        assert!(!config.enabled);
        assert_eq!(config.marker_dir, PathBuf::from(DEFAULT_EVDEV_MARKER_DIR));
        assert_eq!(config.device_dir, PathBuf::from(DEFAULT_EVDEV_DEVICE_DIR));
    }

    #[test]
    fn config_validate_rejects_relative_directories() {
        let config = EvdevProvisionConfig {
            marker_dir: PathBuf::from("relative/markers"),
            ..EvdevProvisionConfig::default()
        };
        assert!(matches!(
            config.validate(),
            Err(EvdevProvisionError::InvalidConfig { .. })
        ));
    }

    #[test]
    fn real_provisioner_is_disabled_by_default_without_touching_uinput() {
        let provisioner = UinputEvdevProvisioner::new(EvdevProvisionConfig::default());
        assert!(matches!(
            provisioner.ensure("vm-a"),
            Err(EvdevProvisionError::Disabled)
        ));
    }

    #[test]
    fn real_provisioner_rejects_unsafe_names_before_device_creation() {
        let config = EvdevProvisionConfig {
            enabled: true,
            marker_dir: PathBuf::from("/tmp/ozma-evdev-test-markers"),
            device_dir: PathBuf::from("/tmp/ozma-evdev-test-devices"),
        };
        let provisioner = UinputEvdevProvisioner::new(config);
        assert!(matches!(
            provisioner.ensure("../vm"),
            Err(EvdevProvisionError::InvalidVmName { .. })
        ));
    }

    #[test]
    fn fake_provisioner_returns_deterministic_paths_and_reuses() {
        let provisioner = FakeEvdevProvisioner::new("/run/ozma/fake-evdev");
        let first = provisioner.ensure("vm-a").unwrap();
        let second = provisioner.ensure("vm-a").unwrap();

        assert_eq!(
            first.keyboard,
            PathBuf::from("/run/ozma/fake-evdev/vm-a-kbd")
        );
        assert_eq!(
            first.mouse,
            PathBuf::from("/run/ozma/fake-evdev/vm-a-mouse")
        );
        assert_eq!(first, second);
        assert_eq!(provisioner.ensured(), vec!["vm-a", "vm-a"]);
    }

    #[test]
    fn fake_provisioner_release_is_recorded() {
        let provisioner = FakeEvdevProvisioner::new("/run/ozma/fake-evdev");
        provisioner.ensure("vm-a").unwrap();
        provisioner.release("vm-a");
        provisioner.release("unknown");
        assert_eq!(provisioner.released(), vec!["vm-a", "unknown"]);
    }

    #[test]
    fn fake_provisioner_failure_is_typed_and_does_not_return_paths() {
        let provisioner = FakeEvdevProvisioner::new("/run/ozma/fake-evdev");
        provisioner.set_fail(true);
        assert!(matches!(
            provisioner.ensure("vm-a"),
            Err(EvdevProvisionError::Create { .. })
        ));
    }
}
