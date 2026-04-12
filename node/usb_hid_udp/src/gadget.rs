//! Async HID gadget writer.
//!
//! Wraps the `hidg` crate's async file handles for `/dev/hidg0` (keyboard)
//! and `/dev/hidg1` (mouse).  Each write is non-blocking inside the Tokio
//! runtime via `hidg`'s Tokio feature.

use std::path::Path;

use hidg::{Class, Device};
use tracing::{error, info, warn};

use crate::report::{KeyboardReport, MouseReport, KBD_REPORT_LEN, MOUSE_REPORT_LEN};

pub struct HidGadget {
    kbd:   Option<hidg::tokio::Device>,
    mouse: Option<hidg::tokio::Device>,
}

impl HidGadget {
    /// Open both gadget devices.
    ///
    /// If a device cannot be opened, a warning is logged and that device is
    /// disabled (writes to it become no-ops), mirroring the Python behaviour.
    pub async fn open(
        kbd_path:   impl AsRef<Path>,
        mouse_path: impl AsRef<Path>,
    ) -> Self {
        let kbd_path   = kbd_path.as_ref();
        let mouse_path = mouse_path.as_ref();

        let kbd = match hidg::tokio::Device::open(kbd_path, Class::Keyboard) {
            Ok(d)  => { info!("Opened keyboard gadget: {}", kbd_path.display()); Some(d) }
            Err(e) => { warn!("Cannot open keyboard gadget {}: {e}", kbd_path.display()); None }
        };

        let mouse = match hidg::tokio::Device::open(mouse_path, Class::Mouse) {
            Ok(d)  => { info!("Opened mouse gadget: {}", mouse_path.display()); Some(d) }
            Err(e) => { warn!("Cannot open mouse gadget {}: {e}", mouse_path.display()); None }
        };

        Self { kbd, mouse }
    }

    /// Write a keyboard report.  No-op if the device failed to open.
    pub async fn write_keyboard(&mut self, report: &KeyboardReport) {
        let bytes = report.to_bytes();
        debug_assert_eq!(bytes.len(), KBD_REPORT_LEN);
        if let Some(dev) = &mut self.kbd {
            if let Err(e) = dev.write(&bytes).await {
                error!("Keyboard write error: {e}");
            }
        }
    }

    /// Write a mouse report.  No-op if the device failed to open.
    pub async fn write_mouse(&mut self, report: &MouseReport) {
        let bytes = report.to_bytes();
        debug_assert_eq!(bytes.len(), MOUSE_REPORT_LEN);
        if let Some(dev) = &mut self.mouse {
            if let Err(e) = dev.write(&bytes).await {
                error!("Mouse write error: {e}");
            }
        }
    }
}
