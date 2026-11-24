// SPDX-License-Identifier: AGPL-3.0-only
//! Input device enumeration and USB-hub grouping.
//!
//! A seat's input is grouped by USB topology: devices sharing the same parent
//! hub are assumed to belong to the same user. This mirrors the Python
//! `agent/multiseat` baseline, including the `internal` group for devices with
//! no USB parent and the gamepad-only group split in the planner.
//!
//! [`SysfsInputBackend`] reads `/sys/class/input` and `/dev/input`. It never
//! grabs an evdev node and never injects input. [`crate::fake`] contains the
//! test-only backends.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::display::BackendDetection;
use crate::error::MultiseatError;

/// Default evdev device directory.
pub const DEFAULT_INPUT_DEV_ROOT: &str = "/dev/input";

/// Default sysfs input class directory.
pub const DEFAULT_SYSFS_INPUT_ROOT: &str = "/sys/class/input";

/// Group key for devices with no USB parent (PS/2, laptop touchpad, ...).
pub const INTERNAL_HUB: &str = "internal";

/// Crude input device class derived from evdev capability bits.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InputDeviceKind {
    /// Keyboard-like device with many key bits.
    Keyboard,
    /// Mouse/pointer-like device with relative movement and buttons.
    Mouse,
    /// Joystick/gamepad-like device with absolute axes and few buttons.
    Gamepad,
    /// A device that did not match a keyboard, mouse, or gamepad shape.
    Other,
}

impl InputDeviceKind {
    /// Stable wire/status name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Keyboard => "keyboard",
            Self::Mouse => "mouse",
            Self::Gamepad => "gamepad",
            Self::Other => "other",
        }
    }
}

/// One evdev input device with its USB grouping hint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputDevice {
    /// Absolute evdev path, for example `/dev/input/event5`.
    pub path: String,
    /// Kernel device name from sysfs, or the event node name as a fallback.
    pub name: String,
    /// Capability-derived class.
    pub kind: InputDeviceKind,
    /// USB topology hub key, for example `1-1`. `None` means non-USB/internal.
    #[serde(default)]
    pub usb_hub: Option<String>,
    /// Canonical sysfs device path when it could be resolved.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sysfs_path: Option<String>,
}

/// Input devices grouped by shared USB hub.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct InputGroup {
    /// `internal` or a USB topology path such as `1-1` / `2-3.1`.
    pub hub_path: String,
    /// Keyboard evdev paths.
    #[serde(default)]
    pub keyboards: Vec<String>,
    /// Mouse/pointer evdev paths.
    #[serde(default)]
    pub mice: Vec<String>,
    /// Gamepad evdev paths.
    #[serde(default)]
    pub gamepads: Vec<String>,
    /// Other classified evdev paths.
    #[serde(default)]
    pub other: Vec<String>,
}

impl InputGroup {
    /// An empty group for a hub path.
    pub fn new(hub_path: impl Into<String>) -> Self {
        Self {
            hub_path: hub_path.into(),
            ..Self::default()
        }
    }

    /// Number of devices in the group.
    pub fn device_count(&self) -> usize {
        self.keyboards.len() + self.mice.len() + self.gamepads.len() + self.other.len()
    }

    /// Whether the group has a keyboard or mouse, matching the Python
    /// "full input group" test.
    pub fn has_input(&self) -> bool {
        !self.keyboards.is_empty() || !self.mice.is_empty()
    }

    /// Every device path in deterministic type order.
    pub fn all_devices(&self) -> Vec<String> {
        let mut devices = Vec::with_capacity(self.device_count());
        devices.extend(self.keyboards.iter().cloned());
        devices.extend(self.mice.iter().cloned());
        devices.extend(self.gamepads.iter().cloned());
        devices.extend(self.other.iter().cloned());
        devices
    }

    /// Add one device to the matching list.
    pub fn push(&mut self, device: &InputDevice) {
        match device.kind {
            InputDeviceKind::Keyboard => self.keyboards.push(device.path.clone()),
            InputDeviceKind::Mouse => self.mice.push(device.path.clone()),
            InputDeviceKind::Gamepad => self.gamepads.push(device.path.clone()),
            InputDeviceKind::Other => self.other.push(device.path.clone()),
        }
    }

    fn sort(&mut self) {
        self.keyboards.sort();
        self.mice.sort();
        self.gamepads.sort();
        self.other.sort();
    }
}

/// Group devices by USB hub. The `internal` group is first; USB hubs follow in
/// lexicographic order. Device paths are sorted inside each group.
pub fn group_inputs(devices: &[InputDevice]) -> Vec<InputGroup> {
    let mut groups: BTreeMap<String, InputGroup> = BTreeMap::new();

    for device in devices {
        let hub = device
            .usb_hub
            .as_deref()
            .filter(|hub| !hub.trim().is_empty())
            .unwrap_or(INTERNAL_HUB);
        groups
            .entry(hub.to_owned())
            .or_insert_with(|| InputGroup::new(hub))
            .push(device);
    }

    let mut result = Vec::new();
    if let Some(mut internal) = groups.remove(INTERNAL_HUB) {
        if internal.device_count() > 0 {
            internal.sort();
            result.push(internal);
        }
    }
    for mut group in groups.into_values() {
        if group.device_count() > 0 {
            group.sort();
            result.push(group);
        }
    }
    result
}

/// Enumerates evdev devices and groups them by USB topology.
pub trait InputBackend {
    /// Stable backend name for status and diagnostics.
    fn backend_name(&self) -> &'static str;

    /// Enumerate input devices. Grouping remains a pure planner step.
    fn enumerate(&self) -> Result<Vec<InputDevice>, MultiseatError>;
}

/// Linux sysfs/evdev input backend.
///
/// This backend is read-only: it opens no `/dev/input/event*` node, issues no
/// ioctl, and never takes an `EVIOCGRAB`. Physical input routing to a seat is
/// not implemented by this foundation.
#[derive(Clone, Debug)]
pub struct SysfsInputBackend {
    sysfs_input_root: PathBuf,
    dev_input_root: PathBuf,
}

impl Default for SysfsInputBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl SysfsInputBackend {
    /// Use the default `/sys/class/input` and `/dev/input` roots.
    pub fn new() -> Self {
        Self {
            sysfs_input_root: PathBuf::from(DEFAULT_SYSFS_INPUT_ROOT),
            dev_input_root: PathBuf::from(DEFAULT_INPUT_DEV_ROOT),
        }
    }

    /// Use explicit roots. This is what fixture tests use; production callers
    /// should normally use [`Self::new`].
    pub fn with_roots(
        sysfs_input_root: impl Into<PathBuf>,
        dev_input_root: impl Into<PathBuf>,
    ) -> Self {
        Self {
            sysfs_input_root: sysfs_input_root.into(),
            dev_input_root: dev_input_root.into(),
        }
    }

    /// Whether `/dev/input` exists and contains event nodes.
    ///
    /// This is a detection check only; it does not prove read permission.
    pub fn detect() -> BackendDetection {
        Self::detect_at(Path::new(DEFAULT_INPUT_DEV_ROOT))
    }

    /// Detect an explicit `/dev/input` root.
    pub fn detect_at(dev_input_root: &Path) -> BackendDetection {
        match std::fs::read_dir(dev_input_root) {
            Ok(entries) => {
                let count = entries
                    .filter_map(Result::ok)
                    .filter(|entry| {
                        entry
                            .file_name()
                            .to_str()
                            .is_some_and(|name| name.starts_with("event"))
                    })
                    .count();
                if count == 0 {
                    BackendDetection::unavailable(
                        "sysfs",
                        format!("{} contains no event nodes", dev_input_root.display()),
                    )
                } else {
                    BackendDetection::available(
                        "sysfs",
                        Some(format!(
                            "{count} evdev event node(s) under {}",
                            dev_input_root.display()
                        )),
                    )
                }
            }
            Err(error) => BackendDetection::unavailable(
                "sysfs",
                format!("cannot read {}: {error}", dev_input_root.display()),
            ),
        }
    }

    /// Read and classify all event nodes.
    pub fn enumerate(&self) -> Result<Vec<InputDevice>, MultiseatError> {
        let entries = std::fs::read_dir(&self.dev_input_root).map_err(|error| {
            MultiseatError::InputBackend(format!(
                "cannot read {}: {error}",
                self.dev_input_root.display()
            ))
        })?;

        let mut event_names: Vec<String> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| entry.file_name().to_str().map(str::to_owned))
            .filter(|name| name.starts_with("event"))
            .collect();
        event_names.sort();
        event_names.dedup();

        let mut devices = Vec::new();
        for event_name in event_names {
            if let Some(device) = self.read_device(&event_name)? {
                devices.push(device);
            }
        }
        devices.sort_by(|left, right| left.path.cmp(&right.path));
        Ok(devices)
    }

    /// Read one event node. Missing capability/sysfs data means "not an input
    /// device we classify" and is skipped, matching the Python behavior.
    fn read_device(&self, event_name: &str) -> Result<Option<InputDevice>, MultiseatError> {
        let sysfs_event = self.sysfs_input_root.join(event_name);
        let device_dir = sysfs_event.join("device");
        if !device_dir.is_dir() {
            return Ok(None);
        }

        let capabilities_dir = device_dir.join("capabilities");
        let ev_path = capabilities_dir.join("ev");
        let key_path = capabilities_dir.join("key");
        let ev_bits = match std::fs::read_to_string(&ev_path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(MultiseatError::io(&ev_path, error)),
        };
        let key_bits = match std::fs::read_to_string(&key_path) {
            Ok(value) => value,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => String::new(),
            Err(error) => return Err(MultiseatError::io(&key_path, error)),
        };

        let Some(kind) = classify_capabilities(&ev_bits, &key_bits) else {
            return Ok(None);
        };

        let name_path = device_dir.join("name");
        let name = std::fs::read_to_string(&name_path)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| event_name.to_owned());

        let canonical = std::fs::canonicalize(&device_dir).ok();
        let sysfs_path = canonical.as_ref().map(|path| path.display().to_string());
        let usb_hub = canonical.as_deref().and_then(usb_hub_path_from_sysfs);

        Ok(Some(InputDevice {
            path: self.dev_input_root.join(event_name).display().to_string(),
            name,
            kind,
            usb_hub,
            sysfs_path,
        }))
    }
}

impl InputBackend for SysfsInputBackend {
    fn backend_name(&self) -> &'static str {
        "sysfs"
    }

    fn enumerate(&self) -> Result<Vec<InputDevice>, MultiseatError> {
        SysfsInputBackend::enumerate(self)
    }
}

/// Classify a device from its sysfs `ev` and `key` capability masks.
///
/// Mirrors the Python baseline:
///
/// * absolute axes + keys + fewer than 20 key bits -> gamepad
/// * at least 30 key bits -> keyboard
/// * relative movement + keys -> mouse
/// * absolute axes + keys -> mouse (touchpad/tablet)
/// * otherwise no classified device
pub fn classify_capabilities(ev_bits: &str, key_bits: &str) -> Option<InputDeviceKind> {
    let ev = parse_hex_mask(ev_bits)?;
    let has_key = ev & (1 << 1) != 0;
    let has_rel = ev & (1 << 2) != 0;
    let has_abs = ev & (1 << 3) != 0;

    let key_count: u32 = key_bits
        .split_whitespace()
        .filter_map(|word| u64::from_str_radix(word, 16).ok())
        .map(|word| word.count_ones())
        .sum();

    if has_abs && has_key && key_count < 20 {
        Some(InputDeviceKind::Gamepad)
    } else if has_key && key_count >= 30 {
        Some(InputDeviceKind::Keyboard)
    } else if has_rel && has_key {
        Some(InputDeviceKind::Mouse)
    } else if has_abs && has_key {
        Some(InputDeviceKind::Mouse)
    } else {
        None
    }
}

/// Trace a canonical sysfs device path to its parent USB hub.
///
/// The last path segment shaped like `1-1.2` is the USB device; the hub is one
/// port up (`1-1`). A device directly on a root-hub port keeps that port as its
/// group key, matching the Python scanner. Interface segments such as
/// `1-1.2:1.0` are ignored by the shape check.
pub fn usb_hub_path_from_sysfs(path: &Path) -> Option<String> {
    let mut usb_segment = None;
    for part in path.iter().filter_map(|part| part.to_str()) {
        if is_usb_device_segment(part) {
            usb_segment = Some(part);
        }
    }
    let segment = usb_segment?;
    match segment.rfind('.') {
        Some(index) if index > 0 => Some(segment[..index].to_owned()),
        _ => Some(segment.to_owned()),
    }
}

fn is_usb_device_segment(part: &str) -> bool {
    let Some((bus, rest)) = part.split_once('-') else {
        return false;
    };
    !bus.is_empty()
        && bus.chars().all(|character| character.is_ascii_digit())
        && !rest.is_empty()
        && rest.split('.').all(|component| {
            !component.is_empty()
                && component
                    .chars()
                    .all(|character| character.is_ascii_digit())
        })
}

fn parse_hex_mask(value: &str) -> Option<u64> {
    value
        .split_whitespace()
        .next()
        .and_then(|word| u64::from_str_radix(word, 16).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(path: &str, kind: InputDeviceKind, hub: Option<&str>) -> InputDevice {
        InputDevice {
            path: path.to_owned(),
            name: path.to_owned(),
            kind,
            usb_hub: hub.map(str::to_owned),
            sysfs_path: None,
        }
    }

    #[test]
    fn groups_internal_first_then_hubs() {
        let devices = vec![
            device("/dev/input/event2", InputDeviceKind::Keyboard, Some("2-1")),
            device("/dev/input/event1", InputDeviceKind::Keyboard, Some("1-1")),
            device("/dev/input/event3", InputDeviceKind::Keyboard, None),
            device("/dev/input/event0", InputDeviceKind::Mouse, Some("1-1")),
        ];
        let groups = group_inputs(&devices);
        assert_eq!(groups.len(), 3);
        assert_eq!(groups[0].hub_path, INTERNAL_HUB);
        assert_eq!(groups[1].hub_path, "1-1");
        assert_eq!(groups[1].mice, vec!["/dev/input/event0"]);
        assert_eq!(groups[1].keyboards, vec!["/dev/input/event1"]);
        assert_eq!(groups[2].hub_path, "2-1");
    }

    #[test]
    fn usb_hub_is_parent_of_device_port() {
        let path = Path::new(
            "/sys/devices/pci0000:00/0000:00:14.0/usb1/1-1/1-1.2/1-1.2:1.0/0003:046D:C52B/input/input8",
        );
        assert_eq!(usb_hub_path_from_sysfs(path).as_deref(), Some("1-1"));
    }

    #[test]
    fn root_port_device_uses_its_own_port_as_group() {
        let path = Path::new("/sys/devices/usb1/1-1/1-1:1.0/input/input3");
        assert_eq!(usb_hub_path_from_sysfs(path).as_deref(), Some("1-1"));
    }

    #[test]
    fn non_usb_path_has_no_group() {
        assert_eq!(
            usb_hub_path_from_sysfs(Path::new("/sys/devices/platform/i8042/serio0")),
            None
        );
    }

    #[test]
    fn classifies_common_capability_shapes() {
        assert_eq!(
            classify_capabilities("3", "ffffffffffffffff"),
            Some(InputDeviceKind::Keyboard)
        );
        assert_eq!(
            classify_capabilities("7", "70000 0 0 0"),
            Some(InputDeviceKind::Mouse)
        );
        assert_eq!(
            classify_capabilities("b", "30000 0 0 0"),
            Some(InputDeviceKind::Gamepad)
        );
        assert_eq!(classify_capabilities("1", "ffffffff"), None);
    }
}
