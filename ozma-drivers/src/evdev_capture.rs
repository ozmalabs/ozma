// SPDX-License-Identifier: AGPL-3.0-only
//! Async evdev device capture with hotplug scanning.
//!
//! Mirrors the device-discovery and event-reading logic in `controller/hid.py`.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;

use evdev::{AbsoluteAxisType, Device, InputEventKind, Key, RelativeAxisType};
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

/// Prefix used to identify ozma virtual devices (mirrors Python constant).
pub const VIRTUAL_DEVICE_PREFIX: &str = "ozma-virtual-";

/// A normalised input event emitted by [`EvdevCapture`].
#[derive(Debug, Clone)]
pub enum InputEvent {
    KeyDown(Key),
    KeyUp(Key),
    RelX(i32),
    RelY(i32),
    RelWheel(i32),
    AbsX { value: i32, max: i32 },
    AbsY { value: i32, max: i32 },
    Sync,
}

/// Returns `true` if the device looks like a keyboard (has EV_KEY + KEY_A).
///
/// Mirrors `find_keyboard_devices()` in `controller/hid.py`.
pub fn is_keyboard(dev: &Device) -> bool {
    dev.supported_keys()
        .map(|keys| keys.contains(Key::KEY_A))
        .unwrap_or(false)
}

/// Returns `true` if the device looks like a mouse (has buttons + relative/absolute axes).
///
/// Mirrors `find_mouse_devices()` in `controller/hid.py`.
pub fn is_mouse(dev: &Device) -> bool {
    let has_btn = dev
        .supported_keys()
        .map(|k| k.contains(Key::BTN_LEFT))
        .unwrap_or(false);
    let has_rel = dev
        .supported_relative_axes()
        .map(|r| r.contains(RelativeAxisType::REL_X))
        .unwrap_or(false);
    let has_abs = dev
        .supported_absolute_axes()
        .map(|a| a.contains(AbsoluteAxisType::ABS_X))
        .unwrap_or(false);
    has_btn && (has_rel || has_abs)
}

/// Enumerate all `/dev/input/event*` devices matching a predicate.
pub fn list_devices<F>(filter: F) -> Vec<PathBuf>
where
    F: Fn(&Device) -> bool,
{
    let mut result = Vec::new();
    let Ok(entries) = std::fs::read_dir("/dev/input") else {
        return result;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let is_event = path
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.starts_with("event"))
            .unwrap_or(false);
        if !is_event {
            continue;
        }
        if let Ok(dev) = Device::open(&path) {
            if filter(&dev) {
                result.push(path);
            }
        }
    }
    result
}

/// Async evdev capture for a single device.
///
/// Opens the device, optionally grabs it exclusively, and streams
/// [`InputEvent`]s over a tokio mpsc channel.
///
/// Mirrors `HIDForwarder._kbd_loop()` / `_mouse_loop()` in `controller/hid.py`.
pub struct EvdevCapture {
    path: PathBuf,
    grab: bool,
    /// Receive end of the event channel.  Consume via `rx.recv().await`.
    pub rx: mpsc::Receiver<InputEvent>,
    tx: mpsc::Sender<InputEvent>,
}

impl EvdevCapture {
    /// Create a new capture for the device at `path`.
    ///
    /// Set `grab = true` to exclusively grab the device so the compositor
    /// does not also receive its events (mirrors `HIDForwarder._grab()`).
    pub fn new(path: impl AsRef<Path>, grab: bool) -> Self {
        let (tx, rx) = mpsc::channel(256);
        Self {
            path: path.as_ref().to_owned(),
            grab,
            tx,
            rx,
        }
    }

    /// Spawn a tokio task that reads events and sends them to the channel.
    ///
    /// Returns the task handle.  The task exits when the device is
    /// disconnected or the receiver is dropped.
    pub fn spawn(self) -> tokio::task::JoinHandle<()> {
        let path = self.path.clone();
        tokio::spawn(async move {
            if let Err(e) = run_capture(self.path.clone(), self.grab, self.tx).await {
                warn!("evdev capture error on {:?}: {}", path, e);
            }
        })
    }
}

async fn run_capture(
    path: PathBuf,
    grab: bool,
    tx: mpsc::Sender<InputEvent>,
) -> anyhow::Result<()> {
    let mut dev = Device::open(&path)?;

    if grab {
        dev.grab()?;
        debug!("Grabbed {:?}", path);
    }

    // Cache ABS axis maximums for normalisation
    let abs_x_max = abs_axis_max(&dev, AbsoluteAxisType::ABS_X).unwrap_or(32767);
    let abs_y_max = abs_axis_max(&dev, AbsoluteAxisType::ABS_Y).unwrap_or(32767);

    info!("Capturing: {} ({:?})", dev.name().unwrap_or("?"), path);

    let mut stream = dev.into_event_stream()?;
    loop {
        let raw = match stream.next_event().await {
            Ok(e) => e,
            Err(e) => {
                warn!("Read error on {:?}: {}", path, e);
                break;
            }
        };

        let evt = match raw.kind() {
            InputEventKind::Key(key) => match raw.value() {
                1 => InputEvent::KeyDown(key),
                0 => InputEvent::KeyUp(key),
                _ => continue, // ignore auto-repeat
            },
            InputEventKind::RelAxis(axis) => match axis {
                RelativeAxisType::REL_X => InputEvent::RelX(raw.value()),
                RelativeAxisType::REL_Y => InputEvent::RelY(raw.value()),
                RelativeAxisType::REL_WHEEL => InputEvent::RelWheel(raw.value()),
                _ => continue,
            },
            InputEventKind::AbsAxis(axis) => match axis {
                AbsoluteAxisType::ABS_X => InputEvent::AbsX {
                    value: raw.value(),
                    max: abs_x_max,
                },
                AbsoluteAxisType::ABS_Y => InputEvent::AbsY {
                    value: raw.value(),
                    max: abs_y_max,
                },
                _ => continue,
            },
            InputEventKind::Synchronization(_) => InputEvent::Sync,
            _ => continue,
        };

        if tx.send(evt).await.is_err() {
            break; // receiver dropped — surface stopped
        }
    }
    Ok(())
}

fn abs_axis_max(dev: &Device, axis: AbsoluteAxisType) -> Option<i32> {
    dev.get_absinfo()
        .ok()?
        .get(axis.0 as usize)
        .copied()
        .map(|a| a.maximum())
}

/// Hotplug scanner: periodically scans `/dev/input` and notifies of newly
/// discovered device paths matching the given filter.
///
/// Mirrors `HIDForwarder._hotplug_loop()` in `controller/hid.py`.
pub struct HotplugScanner {
    known: HashSet<PathBuf>,
    interval: Duration,
}

impl HotplugScanner {
    pub fn new(interval: Duration) -> Self {
        Self {
            known: HashSet::new(),
            interval,
        }
    }

    /// Run forever, sending newly-appeared device paths via `tx`.
    ///
    /// Exits when `tx` is closed.
    pub async fn run<F>(mut self, filter: F, tx: mpsc::Sender<PathBuf>)
    where
        F: Fn(&Device) -> bool,
    {
        loop {
            tokio::time::sleep(self.interval).await;
            let current: HashSet<PathBuf> = list_devices(&filter).into_iter().collect();
            for path in current.difference(&self.known) {
                if tx.send(path.clone()).await.is_err() {
                    return;
                }
            }
            self.known = current;
        }
    }
}
