//! # ozma-hid
//!
//! Cross-platform HID injection for the Ozma agent.
//!
//! Ports `agent/ozma_desktop_agent.py` (HIDInjector* classes) and
//! `controller/paste_typing.py` to Rust using the `enigo` crate.
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use ozma_hid::{HidInjector, PasteTyper, Layout};
//!
//! #[tokio::main]
//! async fn main() {
//!     let mut inj = ozma_hid::build_injector().unwrap();
//!     inj.inject_text("Hello, World!\n", Layout::Us, 30.0).await.unwrap();
//! }
//! ```

pub mod hid_report;
pub mod injector;
pub mod layout;
pub mod paste_typing;

pub use injector::{build_injector, HidInjector};
pub use layout::Layout;
pub use paste_typing::PasteTyper;
