use anyhow::{Context, Result};
use tray_icon::menu::{Menu, MenuItem};
use tray_icon::{Icon, TrayIconBuilder};
use tracing::info;

pub enum TrayEvent {
    ShowStatus,
    Settings,
    Quit,
}

pub struct TrayManager {
    _menu: Menu,
}

impl TrayManager {
    pub fn new() -> Result<Self> {
        let menu = Menu::new();
        
        let ozma_label = MenuItem::new("Ozma Agent", false, None);
        menu.append(&ozma_label)?;

        menu.append_native_separator()?;

        let show_status = MenuItem::new("Show Status", true, None);
        menu.append(&show_status)?;

        let settings = MenuItem::new("Settings...", true, None);
        menu.append(&settings)?;

        menu.append_native_separator()?;

        let quit = MenuItem::new("Quit", true, None);
        menu.append(&quit)?;

        let icon = Self::create_dummy_icon(128, 128, 128)?;

        let _tray_icon = TrayIconBuilder::new()
            .with_menu(menu.clone())
            .with_tooltip("Ozma Agent")
            .with_icon(icon)
            .build()?;

        info!("Tray icon initialized");

        Ok(Self {
            _menu: menu,
        })
    }

    fn create_dummy_icon(r: u8, g: u8, b: u8) -> Result<Icon> {
        let mut rgba = Vec::with_capacity(16 * 16 * 4);
        for _ in 0..(16 * 16) {
            rgba.push(r);
            rgba.push(g);
            rgba.push(b);
            rgba.push(255);
        }
        Ok(Icon::from_rgba(rgba, 16, 16)
            .context("Failed to create dummy icon")?)
    }
}
