// SPDX-License-Identifier: AGPL-3.0-only
//! End-to-end tests for the gadget sink using regular files as device nodes.

use std::fs;
use std::path::PathBuf;

use ozma_input::{
    codes, AbsoluteMapping, FakeInputSink, GadgetOptions, HidGadgetSink, InputSink, MouseProtocol,
};
use ozma_protocol::input::{InputEvent, KeyModifiers, PointerButton, TouchPhase};
use tempfile::TempDir;

struct Fixture {
    _dir: TempDir,
    sink: HidGadgetSink,
    keyboard: PathBuf,
    mouse: PathBuf,
}

impl Fixture {
    fn new(options: GadgetOptions) -> Self {
        let dir = tempfile::tempdir().expect("temp dir");
        let keyboard = dir.path().join("hidg0");
        let mouse = dir.path().join("hidg1");
        fs::write(&keyboard, b"").expect("create keyboard file");
        fs::write(&mouse, b"").expect("create mouse file");
        let sink = HidGadgetSink::with_options(&keyboard, &mouse, options).expect("open sink");
        Self {
            _dir: dir,
            sink,
            keyboard,
            mouse,
        }
    }

    fn keyboard_bytes(&self) -> Vec<u8> {
        fs::read(&self.keyboard).expect("read keyboard reports")
    }

    fn mouse_bytes(&self) -> Vec<u8> {
        fs::read(&self.mouse).expect("read mouse reports")
    }
}

fn key(code: u32, down: bool, modifiers: KeyModifiers) -> InputEvent {
    InputEvent::Key {
        code,
        down,
        modifiers,
    }
}

#[test]
fn keyboard_reports_modifiers_and_releases_cleanly() {
    let fixture = Fixture::new(GadgetOptions::default());
    let sink = &fixture.sink;

    sink.submit(&key(
        0x04,
        true,
        KeyModifiers::new(KeyModifiers::LEFT_SHIFT),
    ))
    .unwrap();
    sink.submit(&key(0x05, true, KeyModifiers::new(KeyModifiers::LEFT_CTRL)))
        .unwrap();
    sink.submit(&key(
        0x04,
        false,
        KeyModifiers::new(KeyModifiers::LEFT_CTRL),
    ))
    .unwrap();
    sink.submit(&key(0x05, false, KeyModifiers::default()))
        .unwrap();

    assert_eq!(
        fixture.keyboard_bytes(),
        vec![
            0x02, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00, 0x01, 0x00, 0x04, 0x05, 0x00, 0x00,
            0x00, 0x00, 0x01, 0x00, 0x05, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00,
        ]
    );
    assert!(sink.held().is_empty());
}

#[test]
fn modifier_usage_keys_and_client_modifiers_combine() {
    let fixture = Fixture::new(GadgetOptions::default());
    let sink = &fixture.sink;

    // Left Shift as a usage id, with no client modifier byte.
    sink.submit(&key(0xE1, true, KeyModifiers::default()))
        .unwrap();
    sink.submit(&key(0x04, true, KeyModifiers::default()))
        .unwrap();
    // Releasing the usage keeps "a" held but clears the modifier.
    sink.submit(&key(0xE1, false, KeyModifiers::default()))
        .unwrap();

    assert_eq!(
        fixture.keyboard_bytes(),
        vec![
            0x02, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x02, 0x00, 0x04, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x04, 0x00, 0x00, 0x00, 0x00, 0x00,
        ]
    );
    assert!(sink.held().has_key(0x04));
    assert_eq!(sink.held().modifiers, 0);
}

#[test]
fn duplicate_key_press_writes_no_second_report() {
    let fixture = Fixture::new(GadgetOptions::default());
    let sink = &fixture.sink;

    let first = sink
        .submit_report(&key(0x04, true, KeyModifiers::default()))
        .unwrap();
    let second = sink
        .submit_report(&key(0x04, true, KeyModifiers::default()))
        .unwrap();

    assert_eq!(first.reports_written, 1);
    assert_eq!(second.reports_written, 0);
    assert!(second.duplicate());
    assert_eq!(fixture.keyboard_bytes().len(), 8);
}

#[test]
fn seventh_key_is_rejected_and_state_stays_consistent() {
    let fixture = Fixture::new(GadgetOptions::default());
    let sink = &fixture.sink;

    for code in 0x04..0x0A {
        sink.submit(&key(code, true, KeyModifiers::default()))
            .unwrap();
    }
    assert_eq!(fixture.keyboard_bytes().len(), 6 * 8);

    let error = sink
        .submit(&key(0x0A, true, KeyModifiers::default()))
        .unwrap_err();
    assert_eq!(error.code, codes::INPUT_KEY_LIMIT);
    assert_eq!(error.details["held_keys"], 6);
    assert_eq!(sink.held().keys.len(), 6);
    // The rejected event did not reach the gadget.
    assert_eq!(fixture.keyboard_bytes().len(), 6 * 8);

    // A slot can be freed and reused.
    sink.submit(&key(0x04, false, KeyModifiers::default()))
        .unwrap();
    sink.submit(&key(0x0A, true, KeyModifiers::default()))
        .unwrap();
    assert!(sink.held().has_key(0x0A));
}

#[test]
fn absolute_pointer_buttons_wheel_and_touch_are_encoded() {
    let fixture = Fixture::new(GadgetOptions {
        mouse_protocol: MouseProtocol::Absolute,
        mapping: AbsoluteMapping::new(1920, 1080),
    });
    let sink = &fixture.sink;

    sink.submit(&InputEvent::PointerAbsolute {
        display_id: None,
        x: 0,
        y: 0,
    })
    .unwrap();
    sink.submit(&InputEvent::Button {
        button: PointerButton::Left,
        down: true,
    })
    .unwrap();
    sink.submit(&InputEvent::Wheel { dx: 9, dy: -1 }).unwrap();
    sink.submit(&InputEvent::Button {
        button: PointerButton::Left,
        down: false,
    })
    .unwrap();
    sink.submit(&InputEvent::Touch {
        contact: 7,
        phase: TouchPhase::Down,
        display_id: None,
        x: 1919,
        y: 1079,
        pressure: 42,
    })
    .unwrap();
    sink.submit(&InputEvent::Touch {
        contact: 7,
        phase: TouchPhase::Up,
        display_id: None,
        x: 1919,
        y: 1079,
        pressure: 0,
    })
    .unwrap();

    assert_eq!(
        fixture.mouse_bytes(),
        vec![
            // absolute 0,0, no buttons
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // left button at 0,0
            0x01, 0x00, 0x00, 0x00, 0x00, 0x00, // wheel -1 keeps the button and position
            0x01, 0x00, 0x00, 0x00, 0x00, 0xFF, // button released
            0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
            // touch down at the bottom-right edge synthesizes left button
            0x01, 0xFF, 0x7F, 0xFF, 0x7F, 0x00, // touch up releases it
            0x00, 0xFF, 0x7F, 0xFF, 0x7F, 0x00,
        ]
    );
    assert!(sink.held().is_empty());
    assert_eq!(
        sink.held().pointer,
        Some(ozma_input::AbsolutePoint::new(32_767, 32_767))
    );
}

#[test]
fn pen_contact_is_mapped_to_the_absolute_pointer() {
    let fixture = Fixture::new(GadgetOptions::default());
    let sink = &fixture.sink;
    let pen = |phase| InputEvent::Pen {
        phase,
        display_id: None,
        x: 100,
        y: 200,
        pressure: 50,
        tilt_x: 10,
        tilt_y: -10,
    };

    sink.submit(&pen(TouchPhase::Down)).unwrap();
    assert!(sink.held().has_button(PointerButton::Left));
    assert!(sink.held().pen_down);
    sink.submit(&pen(TouchPhase::Up)).unwrap();
    assert!(!sink.held().has_button(PointerButton::Left));
    assert!(!sink.held().pen_down);

    let bytes = fixture.mouse_bytes();
    assert_eq!(bytes.len(), 12);
    assert_eq!(bytes[0], 0x01);
    assert_eq!(bytes[6], 0x00);
}

#[test]
fn relative_mouse_reports_and_mode_mismatch() {
    let fixture = Fixture::new(GadgetOptions {
        mouse_protocol: MouseProtocol::Relative,
        mapping: AbsoluteMapping::default(),
    });
    let sink = &fixture.sink;

    sink.submit(&InputEvent::PointerRelative { dx: 5, dy: -3 })
        .unwrap();
    sink.submit(&InputEvent::Wheel { dx: 99, dy: 1 }).unwrap();
    sink.submit(&InputEvent::Button {
        button: PointerButton::Right,
        down: true,
    })
    .unwrap();

    assert_eq!(
        fixture.mouse_bytes(),
        vec![0x00, 0x05, 0xFD, 0x00, 0x00, 0x00, 0x00, 0x01, 0x02, 0x00, 0x00, 0x00,]
    );

    let error = sink
        .submit(&InputEvent::PointerAbsolute {
            display_id: None,
            x: 1,
            y: 1,
        })
        .unwrap_err();
    assert_eq!(error.code, codes::UNSUPPORTED);
    assert_eq!(error.details["event"], "PointerAbsolute");

    let error = sink
        .submit(&InputEvent::Touch {
            contact: 1,
            phase: TouchPhase::Down,
            display_id: None,
            x: 1,
            y: 1,
            pressure: 1,
        })
        .unwrap_err();
    assert_eq!(error.code, codes::UNSUPPORTED);
}

#[test]
fn relative_pointer_is_rejected_by_the_absolute_sink() {
    let fixture = Fixture::new(GadgetOptions::default());
    let error = fixture
        .sink
        .submit(&InputEvent::PointerRelative { dx: 1, dy: 1 })
        .unwrap_err();
    assert_eq!(error.code, codes::UNSUPPORTED);
    assert_eq!(error.details["event"], "PointerRelative");
    assert!(fixture.mouse_bytes().is_empty());
}

#[test]
fn release_all_releases_everything_and_is_idempotent() {
    let fixture = Fixture::new(GadgetOptions::default());
    let sink = &fixture.sink;

    sink.submit(&key(0x04, true, KeyModifiers::new(KeyModifiers::LEFT_GUI)))
        .unwrap();
    sink.submit(&InputEvent::Button {
        button: PointerButton::Left,
        down: true,
    })
    .unwrap();
    sink.submit(&InputEvent::Button {
        button: PointerButton::Back,
        down: true,
    })
    .unwrap();
    sink.submit(&InputEvent::Touch {
        contact: 1,
        phase: TouchPhase::Down,
        display_id: None,
        x: 10,
        y: 10,
        pressure: 1,
    })
    .unwrap();
    assert!(!sink.held().is_empty());

    sink.release_all().unwrap();
    assert!(sink.held().is_empty());
    assert!(
        sink.held().pointer.is_some(),
        "the cursor position is not held input"
    );

    sink.release_all().unwrap();
    assert!(sink.held().is_empty());

    let keyboard = fixture.keyboard_bytes();
    assert_eq!(&keyboard[keyboard.len() - 8..], &[0u8; 8]);

    let mouse = fixture.mouse_bytes();
    let last = &mouse[mouse.len() - 6..];
    assert_eq!(last[0], 0, "buttons must be neutral");
    assert_eq!(last[5], 0, "scroll must be neutral");
}

#[test]
fn clipboard_is_rejected_by_both_sinks() {
    let fixture = Fixture::new(GadgetOptions::default());
    let error = fixture
        .sink
        .submit(&InputEvent::Clipboard {
            text: "secret".into(),
        })
        .unwrap_err();
    assert_eq!(error.code, codes::UNSUPPORTED);
    assert_eq!(error.details["event"], "clipboard");
    assert!(fixture.keyboard_bytes().is_empty());
    assert!(fixture.mouse_bytes().is_empty());

    let fake = FakeInputSink::new();
    let error = fake
        .submit(&InputEvent::Clipboard {
            text: "secret".into(),
        })
        .unwrap_err();
    assert_eq!(error.code, codes::UNSUPPORTED);
    assert_eq!(fake.events().len(), 1);
    assert!(fake.held().is_empty());
}

#[test]
fn unwritable_path_returns_a_structured_error() {
    let dir = tempfile::tempdir().unwrap();
    let keyboard = dir.path().join("missing-hidg0");
    let mouse = dir.path().join("missing-hidg1");

    let error = HidGadgetSink::with_paths(&keyboard, &mouse).unwrap_err();
    assert_eq!(error.code, codes::INPUT_UNAVAILABLE);
    assert_eq!(error.details["operation"], "open");
    assert_eq!(
        error.details["path"].as_str(),
        Some(keyboard.to_str().unwrap())
    );
    assert!(error.remediation.is_some());
}

#[cfg(target_os = "linux")]
#[test]
fn write_failure_is_structured_and_does_not_commit_state() {
    if !std::path::Path::new("/dev/full").exists() {
        return;
    }
    // /dev/full opens successfully and fails every write with ENOSPC.
    let sink = HidGadgetSink::with_paths("/dev/full", "/dev/full").expect("open /dev/full");

    let error = sink
        .submit(&key(0x04, true, KeyModifiers::default()))
        .unwrap_err();
    assert_eq!(error.code, codes::INPUT_WRITE_FAILED);
    assert_eq!(error.details["operation"], "write");
    assert_eq!(error.details["path"], "/dev/full");
    assert!(
        sink.held().is_empty(),
        "a failed write must not commit held state"
    );
}

#[cfg(target_os = "linux")]
#[test]
fn failed_release_still_clears_local_state() {
    if !std::path::Path::new("/dev/full").exists() {
        return;
    }
    let dir = tempfile::tempdir().unwrap();
    let keyboard = dir.path().join("hidg0");
    fs::write(&keyboard, b"").unwrap();

    let sink = HidGadgetSink::with_paths(&keyboard, "/dev/full").expect("open devices");
    sink.submit(&key(0x04, true, KeyModifiers::default()))
        .unwrap();
    assert!(!sink.held().is_empty());

    let error = sink.release_all().unwrap_err();
    assert_eq!(error.code, codes::INPUT_WRITE_FAILED);
    assert!(
        sink.held().is_empty(),
        "release_all clears local state even when the mouse write fails"
    );
}

#[test]
fn opening_a_directory_as_a_gadget_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    let error = HidGadgetSink::with_paths(dir.path(), dir.path()).unwrap_err();
    assert_eq!(error.code, codes::INPUT_UNAVAILABLE);
    assert_eq!(error.details["operation"], "open");
}
