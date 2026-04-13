// SPDX-License-Identifier: AGPL-3.0-only
//! Mouse HID state and report building.
//!
//! Mirrors `MouseState` in `controller/hid.py`.

/// Absolute coordinate range used in the HID report (0–32767).
pub const ABSOLUTE_MAX: i32 = 32767;

/// Packet type byte for absolute mouse reports (matches `controller/transport.py`).
pub const PKT_MOUSE: u8 = 0x02;
/// Packet type byte for relative mouse reports (gaming mode).
pub const PKT_MOUSE_REL: u8 = 0x03;

/// Tracks mouse position, buttons, and scroll; builds HID reports.
///
/// Mirrors `MouseState` in `controller/hid.py`.
#[derive(Debug)]
pub struct MouseState {
    /// Absolute X position in 0–32767 range.
    pub x: i32,
    /// Absolute Y position in 0–32767 range.
    pub y: i32,
    /// Button bitmask: bit 0 = left, bit 1 = right, bit 2 = middle.
    pub buttons: u8,
    /// Pending scroll delta (signed byte, consumed on report build).
    pub scroll: i8,

    // Relative accumulator for gaming mode
    rel_dx: i32,
    rel_dy: i32,

    // Screen dimensions for relative → absolute scaling
    screen_w: i32,
    screen_h: i32,
    scale_x: f64,
    scale_y: f64,
}

impl Default for MouseState {
    fn default() -> Self {
        Self::new(1920, 1080)
    }
}

impl MouseState {
    pub fn new(screen_w: i32, screen_h: i32) -> Self {
        Self {
            x: ABSOLUTE_MAX / 2,
            y: ABSOLUTE_MAX / 2,
            buttons: 0,
            scroll: 0,
            rel_dx: 0,
            rel_dy: 0,
            screen_w,
            screen_h,
            scale_x: ABSOLUTE_MAX as f64 / screen_w as f64,
            scale_y: ABSOLUTE_MAX as f64 / screen_h as f64,
        }
    }

    /// Update screen dimensions and recalculate scale factors.
    pub fn set_screen_size(&mut self, w: i32, h: i32) {
        self.screen_w = w;
        self.screen_h = h;
        self.scale_x = ABSOLUTE_MAX as f64 / w as f64;
        self.scale_y = ABSOLUTE_MAX as f64 / h as f64;
    }

    /// Apply a relative movement delta.
    pub fn move_rel(&mut self, dx: i32, dy: i32) {
        self.x = (self.x + (dx as f64 * self.scale_x) as i32).clamp(0, ABSOLUTE_MAX);
        self.y = (self.y + (dy as f64 * self.scale_y) as i32).clamp(0, ABSOLUTE_MAX);
        self.rel_dx += dx;
        self.rel_dy += dy;
    }

    /// Set absolute position from raw device coordinates.
    pub fn move_abs(&mut self, raw_x: i32, raw_y: i32, max_x: i32, max_y: i32) {
        self.x = (raw_x * ABSOLUTE_MAX / max_x.max(1)).clamp(0, ABSOLUTE_MAX);
        self.y = (raw_y * ABSOLUTE_MAX / max_y.max(1)).clamp(0, ABSOLUTE_MAX);
    }

    /// Record a button press.
    pub fn button_press(&mut self, btn: MouseButton) {
        self.buttons |= btn.bit();
    }

    /// Record a button release.
    pub fn button_release(&mut self, btn: MouseButton) {
        self.buttons &= !btn.bit();
    }

    /// Accumulate a scroll delta (saturating to signed byte range).
    pub fn add_scroll(&mut self, delta: i8) {
        self.scroll = self.scroll.saturating_add(delta);
    }

    /// Build a 6-byte absolute mouse report and consume the scroll delta.
    ///
    /// Layout: `[buttons, x_lo, x_hi, y_lo, y_hi, scroll]`
    pub fn build_report(&mut self) -> [u8; 6] {
        let x_lo = (self.x & 0xFF) as u8;
        let x_hi = ((self.x >> 8) & 0xFF) as u8;
        let y_lo = (self.y & 0xFF) as u8;
        let y_hi = ((self.y >> 8) & 0xFF) as u8;
        let scroll = self.scroll as u8;
        self.scroll = 0;
        [self.buttons, x_lo, x_hi, y_lo, y_hi, scroll]
    }

    /// Build a relative mouse report for gaming mode.
    ///
    /// Returns `None` if there is no movement or scroll to report.
    ///
    /// Layout: `[buttons, dx_lo, dx_hi, dy_lo, dy_hi, scroll]`
    /// where dx/dy are signed 16-bit little-endian.
    pub fn build_relative_report(&mut self) -> Option<[u8; 6]> {
        let dx = self.rel_dx;
        let dy = self.rel_dy;
        let scroll = self.scroll;
        if dx == 0 && dy == 0 && scroll == 0 {
            return None;
        }
        let dx = dx.clamp(-32768, 32767) as i16;
        let dy = dy.clamp(-32768, 32767) as i16;
        self.rel_dx = 0;
        self.rel_dy = 0;
        self.scroll = 0;
        let [dx_lo, dx_hi] = dx.to_le_bytes();
        let [dy_lo, dy_hi] = dy.to_le_bytes();
        Some([self.buttons, dx_lo, dx_hi, dy_lo, dy_hi, scroll as u8])
    }
}

/// Mouse button identifiers.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseButton {
    Left,
    Right,
    Middle,
}

impl MouseButton {
    pub fn bit(self) -> u8 {
        match self {
            MouseButton::Left => 0x01,
            MouseButton::Right => 0x02,
            MouseButton::Middle => 0x04,
        }
    }
}

/// Try to map an evdev `Key` to a [`MouseButton`].
pub fn evdev_to_mouse_button(key: evdev::Key) -> Option<MouseButton> {
    match key {
        evdev::Key::BTN_LEFT => Some(MouseButton::Left),
        evdev::Key::BTN_RIGHT => Some(MouseButton::Right),
        evdev::Key::BTN_MIDDLE => Some(MouseButton::Middle),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_initial_position_is_center() {
        let state = MouseState::new(1920, 1080);
        assert_eq!(state.x, ABSOLUTE_MAX / 2);
        assert_eq!(state.y, ABSOLUTE_MAX / 2);
    }

    #[test]
    fn test_abs_report_layout() {
        let mut state = MouseState::new(1920, 1080);
        state.x = 0x0102;
        state.y = 0x0304;
        let report = state.build_report();
        assert_eq!(report[0], 0x00); // buttons
        assert_eq!(report[1], 0x02); // x_lo
        assert_eq!(report[2], 0x01); // x_hi
        assert_eq!(report[3], 0x04); // y_lo
        assert_eq!(report[4], 0x03); // y_hi
    }

    #[test]
    fn test_scroll_consumed_after_report() {
        let mut state = MouseState::default();
        state.add_scroll(3);
        let report = state.build_report();
        assert_eq!(report[5], 3);
        let report2 = state.build_report();
        assert_eq!(report2[5], 0);
    }

    #[test]
    fn test_relative_report_none_when_no_movement() {
        let mut state = MouseState::default();
        assert!(state.build_relative_report().is_none());
    }

    #[test]
    fn test_relative_report_some_on_movement() {
        let mut state = MouseState::default();
        state.move_rel(10, -5);
        let rel = state.build_relative_report();
        assert!(rel.is_some());
        // Deltas consumed — next call should return None
        assert!(state.build_relative_report().is_none());
    }

    #[test]
    fn test_button_press_release() {
        let mut state = MouseState::default();
        state.button_press(MouseButton::Left);
        assert_eq!(state.buttons & 0x01, 0x01);
        state.button_release(MouseButton::Left);
        assert_eq!(state.buttons & 0x01, 0x00);
    }

    #[test]
    fn test_move_abs_clamps() {
        let mut state = MouseState::default();
        state.move_abs(99999, 99999, 1920, 1080);
        assert_eq!(state.x, ABSOLUTE_MAX);
        assert_eq!(state.y, ABSOLUTE_MAX);
    }
}
