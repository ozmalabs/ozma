// SPDX-License-Identifier: AGPL-3.0-only
//!
//! Opt-in, live xrandr ownership smoke. Nothing runs unless
//! `OZMA_MULTISEAT_REAL=1` and `OZMA_MULTISEAT_XRANDR_APPLY=1`.
//!
//! This changes one output's mode and position, then releases it with
//! `--auto`. Use a throwaway Xvfb display, never the operator's desktop:
//!
//! ```bash
//! Xvfb :99 -screen 0 1280x800x24 &
//! OZMA_MULTISEAT_REAL=1 OZMA_MULTISEAT_XRANDR_APPLY=1 DISPLAY=:99 \
//!   cargo test -p ozma-multiseat --test real_ownership -- --ignored --nocapture
//! ```

use ozma_multiseat::{
    DisplayOwnership, DisplayOwnershipController, DisplayOwnershipKind, ProcessCommandRunner,
    XrandrOwnershipController,
};

fn enabled() -> bool {
    std::env::var("OZMA_MULTISEAT_REAL").as_deref() == Ok("1")
        && std::env::var("OZMA_MULTISEAT_XRANDR_APPLY").as_deref() == Ok("1")
}

fn display() -> String {
    std::env::var("OZMA_MULTISEAT_XDISPLAY")
        .or_else(|_| std::env::var("DISPLAY"))
        .expect("set DISPLAY or OZMA_MULTISEAT_XDISPLAY")
}

fn parse_mode(mode: &str) -> (u32, u32) {
    let (width, height) = mode.split_once('x').expect("mode must be WIDTHxHEIGHT");
    (
        width.parse().expect("mode width"),
        height.parse().expect("mode height"),
    )
}

#[test]
#[ignore = "opt-in throwaway Xvfb: set OZMA_MULTISEAT_REAL=1 and OZMA_MULTISEAT_XRANDR_APPLY=1"]
fn real_xrandr_ownership_apply_and_release_on_a_throwaway_display() {
    if !enabled() {
        return;
    }

    let display = display();
    let output =
        std::env::var("OZMA_MULTISEAT_XRANDR_OUTPUT").unwrap_or_else(|_| "screen".to_owned());
    let mode =
        std::env::var("OZMA_MULTISEAT_XRANDR_MODE").unwrap_or_else(|_| "1280x800".to_owned());
    let (width, height) = parse_mode(&mode);

    let ownership = DisplayOwnership {
        seat_id: "xvfb-seat-0".to_owned(),
        kind: DisplayOwnershipKind::XrandrOutput,
        x_screen: display.clone(),
        output: output.clone(),
        mode: mode.clone(),
        width,
        height,
        x: 0,
        y: 0,
        primary: false,
        virtual_display: false,
    };

    let mut controller = XrandrOwnershipController::new(Box::new(ProcessCommandRunner));
    controller.apply(&ownership).expect("xrandr apply");
    assert_eq!(controller.status(), vec![ownership.clone()]);
    println!(
        "applied ownership: {} {} mode={} pos={} on {}",
        ownership.kind.as_str(),
        output,
        mode,
        ownership.position(),
        display
    );

    controller.release("xvfb-seat-0").expect("xrandr release");
    assert!(controller.status().is_empty());
    println!("released ownership for {output}");
}
