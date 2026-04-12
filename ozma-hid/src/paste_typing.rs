//! Paste-as-typing engine.
//!
//! High-level wrapper around `HidInjector::inject_text` that mirrors the
//! `PasteTyper` class in `controller/paste_typing.py`.
//!
//! ```rust,no_run
//! use ozma_hid::{PasteTyper, Layout};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut typer = PasteTyper::new().unwrap();
//!     let result = typer.type_text("Hello!\n", Layout::Us, 30.0).await.unwrap();
//!     println!("sent={} skipped={}", result.chars_sent, result.chars_skipped);
//! }
//! ```

use crate::injector::{build_injector, HidInjector, InjectorError, PasteResult};
use crate::layout::Layout;

/// Paste-as-typing engine.
///
/// Wraps a `HidInjector` and exposes the same interface as the Python
/// `PasteTyper` class.
pub struct PasteTyper {
    injector: HidInjector,
    is_typing: bool,
}

impl PasteTyper {
    /// Create a new `PasteTyper` using the platform-default injector.
    pub fn new() -> Result<Self, InjectorError> {
        Ok(Self {
            injector: build_injector()?,
            is_typing: false,
        })
    }

    /// `true` while a `type_text` call is in progress.
    pub fn is_typing(&self) -> bool {
        self.is_typing
    }

    /// Type `text` at `rate` chars/sec using `layout`.
    ///
    /// Rate is clamped to 5-100 chars/sec (default 30).
    /// Mirrors `PasteTyper.type_text()` in `controller/paste_typing.py`.
    pub async fn type_text(
        &mut self,
        text: &str,
        layout: Layout,
        rate: f64,
    ) -> Result<PasteResult, InjectorError> {
        self.is_typing = true;
        let result = self.injector.inject_text(text, layout, rate).await;
        self.is_typing = false;
        result
    }

    /// Type a single named key (e.g. `"enter"`, `"f1"`, `"esc"`).
    ///
    /// Mirrors `PasteTyper.type_key()` in `controller/paste_typing.py`.
    pub fn type_key(&mut self, key_name: &str) -> Result<bool, InjectorError> {
        use enigo::Key;
        let key = match key_name.to_lowercase().as_str() {
            "enter" | "return" => Key::Return,
            "esc"   | "escape" => Key::Escape,
            "backspace"        => Key::Backspace,
            "tab"              => Key::Tab,
            "space"            => Key::Space,
            "delete" | "del"   => Key::Delete,
            "insert" | "ins"   => Key::Insert,
            "home"             => Key::Home,
            "end"              => Key::End,
            "pageup"  | "pgup" => Key::PageUp,
            "pagedown"| "pgdn" => Key::PageDown,
            "up"               => Key::UpArrow,
            "down"             => Key::DownArrow,
            "left"             => Key::LeftArrow,
            "right"            => Key::RightArrow,
            "f1"  => Key::F1,  "f2"  => Key::F2,
            "f3"  => Key::F3,  "f4"  => Key::F4,
            "f5"  => Key::F5,  "f6"  => Key::F6,
            "f7"  => Key::F7,  "f8"  => Key::F8,
            "f9"  => Key::F9,  "f10" => Key::F10,
            "f11" => Key::F11, "f12" => Key::F12,
            _ => return Ok(false),
        };
        self.injector.tap_key(key, false, false)?;
        Ok(true)
    }

    /// List available layout names.
    pub fn available_layouts() -> &'static [&'static str] {
        &["us", "uk", "de"]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke-test: PasteTyper::available_layouts returns the expected set.
    #[test]
    fn available_layouts() {
        let layouts = PasteTyper::available_layouts();
        assert!(layouts.contains(&"us"));
        assert!(layouts.contains(&"uk"));
        assert!(layouts.contains(&"de"));
    }
}
