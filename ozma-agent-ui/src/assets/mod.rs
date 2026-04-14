//! Tray icon assets module
//!
//! PNG icons are auto-generated at build time if they don't exist.

/// Grey icon (disconnected state)
pub static GREY_ICON: &[u8] = include_bytes!("grey.png");

/// Green icon (connected state)
pub static GREEN_ICON: &[u8] = include_bytes!("green.png");

/// Yellow icon (connecting state)
pub static YELLOW_ICON: &[u8] = include_bytes!("yellow.png");

/// Red icon (error state)
pub static RED_ICON: &[u8] = include_bytes!("red.png");
