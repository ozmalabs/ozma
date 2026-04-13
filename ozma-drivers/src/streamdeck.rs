//! Stream Deck driver for elgato-streamdeck 0.5+ API.
//!
//! This module provides a wrapper around elgato-streamdeck devices with a
//! consistent interface. It uses the 0.5 API which removed DeviceManager and
//! DeviceType in favor of direct device enumeration via HidApi.

use anyhow::{anyhow, Context, Result};
use elgato_streamdeck::{hidapi::HidApi, list_devices, StreamDeck};
use std::sync::{Arc, Mutex};

/// Wrapper around an elgato-streamdeck device with a consistent interface.
pub struct StreamDeckDevice {
    /// The underlying stream deck device.
    device: Arc<Mutex<StreamDeck>>,
    /// Number of keys on this device (used to identify model).
    key_count: u8,
}

impl StreamDeckDevice {
    /// Opens the first available Stream Deck device.
    ///
    /// Uses elgato-streamdeck 0.5 API: `list_devices()` + `StreamDeck::open()`.
    pub fn open_first() -> Result<Self> {
        let hidapi = HidApi::new()
            .context("Failed to initialize HID API")?;

        let devices = list_devices(&hidapi);
        if devices.is_empty() {
            return Err(anyhow!("No Stream Deck devices found"));
        }

        let info = &devices[0];
        let device = StreamDeck::open(&hidapi, &info.path)
            .with_context(|| {
                format!(
                    "Failed to open Stream Deck device: {}",
                    info.product_string.as_deref().unwrap_or("unknown")
                )
            })?;

        let key_count = device.key_count();
        let device = Arc::new(Mutex::new(device));

        Ok(Self { device, key_count })
    }

    /// Returns the number of keys on this device.
    pub fn key_count(&self) -> u8 {
        self.key_count
    }

    /// Returns a device identifier string based on key count.
    pub fn device_id(&self) -> &'static str {
        match self.key_count {
            6 => "streamdeck_original",
            15 => "streamdeck_mini",
            32 => "streamdeckxl",
            3 => "streamdeck_pedal",
            _ => "streamdeck_unknown",
        }
    }

    /// Returns true if this device has a visual display.
    pub fn has_display(&self) -> bool {
        // The Pedal (3 keys) has no visual display
        self.key_count != 3
    }

    /// Fills the key at `index` with the given RGB color.
    pub fn fill_key_color(&self, index: u8, r: u8, g: u8, b: u8) -> Result<()> {
        let mut device = self.device.lock().unwrap();
        device
            .set_key_color(index, r, g, b)
            .with_context(|| format!("Failed to set color for key {}", index))?;
        Ok(())
    }

    /// Clears all keys.
    pub fn clear_keys(&self) -> Result<()> {
        let mut device = self.device.lock().unwrap();
        device.reset().context("Failed to reset device")?;
        Ok(())
    }

    /// Sets the brightness (0-100).
    pub fn set_brightness(&self, percentage: u8) -> Result<()> {
        let mut device = self.device.lock().unwrap();
        device
            .set_brightness(percentage)
            .context("Failed to set brightness")?;
        Ok(())
    }
}
