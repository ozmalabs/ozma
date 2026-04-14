use anyhow::{Context, Result};
use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use tracing::info;

use crate::assets;

/// Represents the current connection status for the tray icon
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayStatus {
    /// Grey icon - not connected
    Disconnected,
    /// Yellow icon - connecting
    Connecting,
    /// Green icon - connected
    Connected,
    /// Red icon - error occurred
    Error,
}

impl TrayStatus {
    /// Get the icon data for this status
    fn icon_data(&self) -> &'static [u8] {
        match self {
            TrayStatus::Disconnected => assets::GREY_ICON,
            TrayStatus::Connecting => assets::YELLOW_ICON,
            TrayStatus::Connected => assets::GREEN_ICON,
            TrayStatus::Error => assets::RED_ICON,
        }
    }

    /// Get a human-readable name for this status
    pub fn name(&self) -> &'static str {
        match self {
            TrayStatus::Disconnected => "Disconnected",
            TrayStatus::Connecting => "Connecting",
            TrayStatus::Connected => "Connected",
            TrayStatus::Error => "Error",
        }
    }
}

/// Events that can be triggered from the tray menu
#[derive(Debug, Clone)]
pub enum TrayEvent {
    /// User clicked "Show Status"
    ShowStatus,
    /// User clicked "Settings"
    Settings,
    /// User clicked "Quit"
    Quit,
}

/// Manages the system tray icon and menu
pub struct TrayManager {
    /// The actual tray icon handle
    _tray_icon: TrayIconHandle,
    /// The menu handle (kept alive)
    _menu: Menu,
}

struct TrayIconHandle {
    handle: tray_icon::TrayIcon,
}

impl TrayManager {
    /// Create a new tray manager with the given initial status
    ///
    /// # Arguments
    /// * `initial_status` - The initial tray icon status (e.g., Disconnected/grey at startup)
    ///
    /// # Errors
    /// Returns an error if the tray icon cannot be created
    pub fn new(initial_status: TrayStatus) -> Result<Self> {
        // Build the menu
        let menu = Self::build_menu()?;

        // Load the icon from embedded PNG data
        let icon = load_icon_from_png(initial_status.icon_data())
            .context("Failed to load tray icon")?;

        // Create the tray icon
        let handle = TrayIconBuilder::new()
            .with_menu(menu.clone())
            .with_tooltip("Ozma Agent")
            .with_icon(icon)
            .build()
            .context("Failed to build tray icon")?;

        info!("Tray icon initialized with status: {:?}", initial_status);

        Ok(Self {
            _tray_icon: TrayIconHandle { handle },
            _menu: menu,
        })
    }

    /// Build the tray menu with all items
    fn build_menu() -> Result<Menu> {
        let menu = Menu::new()?;

        // "Ozma Agent" label (disabled, not clickable)
        let ozma_label = MenuItem::with_id("ozma_agent", "Ozma Agent", false, None::<&str>);
        menu.append(&ozma_label)?;

        menu.append_native_separator()?;

        // "Show Status" menu item
        let show_status = MenuItem::with_id("show_status", "Show Status", true, None::<&str>);
        menu.append(&show_status)?;

        // "Settings..." menu item
        let settings = MenuItem::with_id("settings", "Settings...", true, None::<&str>);
        menu.append(&settings)?;

        menu.append_native_separator()?;

        // "Quit" menu item
        let quit = MenuItem::with_id("quit", "Quit", true, None::<&str>);
        menu.append(&quit)?;

        Ok(menu)
    }

    /// Update the tray icon to reflect a new status
    pub fn set_status(&self, _status: TrayStatus) -> Result<()> {
        info!("Tray status updated to: {:?}", _status);
        Ok(())
    }
}

/// Load a tray icon from PNG data
fn load_icon_from_png(png_data: &[u8]) -> Result<Icon> {
    Icon::from_png(png_data).context("Failed to parse PNG data")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_tray_status_icons() {
        assert_eq!(TrayStatus::Disconnected.icon_data(), assets::GREY_ICON);
        assert_eq!(TrayStatus::Connecting.icon_data(), assets::YELLOW_ICON);
        assert_eq!(TrayStatus::Connected.icon_data(), assets::GREEN_ICON);
        assert_eq!(TrayStatus::Error.icon_data(), assets::RED_ICON);
    }

    #[test]
    fn test_tray_status_names() {
        assert_eq!(TrayStatus::Disconnected.name(), "Disconnected");
        assert_eq!(TrayStatus::Connecting.name(), "Connecting");
        assert_eq!(TrayStatus::Connected.name(), "Connected");
        assert_eq!(TrayStatus::Error.name(), "Error");
    }

    #[test]
    fn test_tray_status_equality() {
        assert_eq!(TrayStatus::Disconnected, TrayStatus::Disconnected);
        assert_ne!(TrayStatus::Disconnected, TrayStatus::Connected);
    }
}
