//! Async HID gadget device wrapper.
//!
//! Opens /dev/hidg0 (keyboard) and /dev/hidg1 (mouse) via the `hidg` crate
//! and exposes async write methods that mirror USBHIDGadget in usb_hid.py.

use std::path::{Path, PathBuf};

use hidg::{Class, Device};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use crate::report::{KeyboardReport, MouseReport, KBD_REPORT_LEN, MOUSE_REPORT_LEN};

pub struct UsbHidGadget {
    kbd:   Mutex<Option<Device>>,
    mouse: Mutex<Option<Device>>,
    kbd_path:   PathBuf,
    mouse_path: PathBuf,
}

impl UsbHidGadget {
    /// Open both gadget devices.
    ///
    /// If a device cannot be opened, a warning is logged and that device is
    /// disabled (writes to it become no-ops), mirroring the Python behaviour.
    pub async fn open(
        kbd_path: impl AsRef<Path>,
        mouse_path: impl AsRef<Path>,
    ) -> Self {
        let kbd_path   = kbd_path.as_ref().to_path_buf();
        let mouse_path = mouse_path.as_ref().to_path_buf();

        let kbd = match Device::open(&kbd_path, Class::Keyboard) {
            Ok(d) => {
                info!("Opened keyboard gadget: {}", kbd_path.display());
                Some(d)
            }
            Err(e) => {
                error!("Cannot open keyboard gadget {}: {}", kbd_path.display(), e);
                None
            }
        };

        let mouse = match Device::open(&mouse_path, Class::Mouse) {
            Ok(d) => {
                info!("Opened mouse gadget: {}", mouse_path.display());
                Some(d)
            }
            Err(e) => {
                error!("Cannot open mouse gadget {}: {}", mouse_path.display(), e);
                None
            }
        };

        Self {
            kbd:   Mutex::new(kbd),
            mouse: Mutex::new(mouse),
            kbd_path,
            mouse_path,
        }
    }

    /// Write a keyboard report.  No-op if the device is unavailable.
    pub async fn write_keyboard(&self, report: &KeyboardReport) {
        let buf: [u8; KBD_REPORT_LEN] = report.to_bytes();
        let mut guard = self.kbd.lock().await;
        match guard.as_mut() {
            Some(dev) => {
                if let Err(e) = dev.write(&buf) {
                    warn!("keyboard write error: {}", e);
                }
            }
            None => {
                warn!("keyboard gadget unavailable ({})", self.kbd_path.display());
            }
        }
    }

    /// Write a mouse report.  No-op if the device is unavailable.
    pub async fn write_mouse(&self, report: &MouseReport) {
        let buf: [u8; MOUSE_REPORT_LEN] = report.to_bytes();
        let mut guard = self.mouse.lock().await;
        match guard.as_mut() {
            Some(dev) => {
                if let Err(e) = dev.write(&buf) {
                    warn!("mouse write error: {}", e);
                }
            }
            None => {
                warn!("mouse gadget unavailable ({})", self.mouse_path.display());
            }
        }
    }
}
