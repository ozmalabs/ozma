// SPDX-License-Identifier: AGPL-3.0-only
//! Standalone binary: print gamepad events as newline-delimited JSON to stdout.
//!
//! Usage:
//!   RUST_LOG=info ozma-gamepad
//!
//! Useful for debugging mappings and verifying hotplug behaviour.

use ozma_drivers::gamepad::{mapping::ControlEvent, GamepadDriver};
use tracing_subscriber::EnvFilter;

fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let mut driver = GamepadDriver::new().expect("Failed to initialise gilrs");

    driver.set_callback(Box::new(|surface_id: String, ev: ControlEvent| {
        let line = serde_json::json!({
            "surface": surface_id,
            "control": ev.control,
            "action":  ev.action,
        });
        println!("{line}");
    }));

    eprintln!("ozma-gamepad: listening for gamepad events (Ctrl-C to quit)…");
    driver.run(); // blocks
}
