//! Integration tests for ozma-hid.
//!
//! # Linux evdev readback test
//!
//! On Linux with a real or virtual uinput device available, this test:
//!   1. Creates a `PasteTyper`
//!   2. Types a known string ("hello\n")
//!   3. Reads back the key events via evdev and verifies the sequence
//!
//! The test is skipped automatically if:
//!   - Not running on Linux
//!   - `/dev/uinput` is not accessible (no CAP_SYS_ADMIN / no uinput module)
//!   - The `OZMA_HID_INTEGRATION` env var is not set (to avoid running in CI
//!     without a display/uinput)
//!
//! Run manually:
//!   OZMA_HID_INTEGRATION=1 cargo test --test inject_test -- --nocapture

use ozma_hid::{Layout, PasteTyper};

/// Verify that `PasteTyper::available_layouts()` is stable.
#[test]
fn layouts_available() {
    let layouts = PasteTyper::available_layouts();
    assert!(layouts.contains(&"us"));
    assert!(layouts.contains(&"uk"));
    assert!(layouts.contains(&"de"));
}

/// Verify that `PasteTyper::new()` succeeds (or fails gracefully without a display).
#[test]
fn paste_typer_new() {
    // On headless CI this may fail — that's acceptable.
    // We just check it doesn't panic.
    let _ = PasteTyper::new();
}

/// Paste-as-typing smoke test: type a short string and verify counts.
///
/// Requires `OZMA_HID_INTEGRATION=1` and a display/uinput to be available.
#[tokio::test]
async fn type_text_counts() {
    if std::env::var("OZMA_HID_INTEGRATION").is_err() {
        eprintln!("Skipping integration test (set OZMA_HID_INTEGRATION=1 to enable)");
        return;
    }

    let mut typer = match PasteTyper::new() {
        Ok(t) => t,
        Err(e) => {
            eprintln!("PasteTyper::new() failed (no display?): {e}");
            return;
        }
    };

    // "hello\n" — 6 chars, all in US layout, none skipped
    let result = typer
        .type_text("hello\n", Layout::Us, 100.0)
        .await
        .expect("type_text failed");

    assert_eq!(result.chars_sent, 6, "expected 6 chars sent");
    assert_eq!(result.chars_skipped, 0, "expected 0 chars skipped");
}

/// Verify that unmappable characters are counted as skipped.
#[tokio::test]
async fn type_text_skips_unmappable() {
    if std::env::var("OZMA_HID_INTEGRATION").is_err() {
        return;
    }

    let mut typer = match PasteTyper::new() {
        Ok(t) => t,
        Err(_) => return,
    };

    // '\x00' and '\x01' have no layout mapping
    let result = typer
        .type_text("\x00\x01", Layout::Us, 100.0)
        .await
        .expect("type_text failed");

    assert_eq!(result.chars_sent, 0);
    assert_eq!(result.chars_skipped, 2);
}

/// Verify that `type_key` returns true for known keys and false for unknown.
#[test]
fn type_key_known_and_unknown() {
    if std::env::var("OZMA_HID_INTEGRATION").is_err() {
        return;
    }

    let mut typer = match PasteTyper::new() {
        Ok(t) => t,
        Err(_) => return,
    };

    assert_eq!(typer.type_key("enter").unwrap(), true);
    assert_eq!(typer.type_key("f12").unwrap(), true);
    assert_eq!(typer.type_key("notakey").unwrap(), false);
}

// ── Linux evdev readback ─────────────────────────────────────────────────────

/// Full evdev readback test (Linux only).
///
/// Types "ab" and verifies the evdev event stream contains KEY_A and KEY_B
/// press+release events in order.
#[cfg(target_os = "linux")]
#[tokio::test]
async fn linux_evdev_readback() {
    if std::env::var("OZMA_HID_INTEGRATION").is_err() {
        eprintln!("Skipping evdev readback test (set OZMA_HID_INTEGRATION=1)");
        return;
    }

    use std::path::Path;

    // Find the enigo virtual keyboard device (created by enigo on Linux via uinput)
    // It appears as /dev/input/eventN with name containing "enigo"
    let dev_path = find_enigo_device();
    if dev_path.is_none() {
        eprintln!("No enigo uinput device found — skipping evdev readback");
        return;
    }
    let dev_path = dev_path.unwrap();

    // Open the evdev device for reading *before* we type
    let mut dev = evdev::Device::open(&dev_path).expect("open evdev device");
    dev.grab().ok(); // exclusive grab so we capture all events

    let mut typer = PasteTyper::new().expect("PasteTyper::new");

    // Type "ab" at max rate
    let result = typer
        .type_text("ab", Layout::Us, 100.0)
        .await
        .expect("type_text");

    assert_eq!(result.chars_sent, 2);

    // Collect events for up to 500ms
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_millis(500);
    let mut key_events: Vec<(evdev::Key, i32)> = Vec::new();

    while tokio::time::Instant::now() < deadline {
        if let Ok(events) = dev.fetch_events() {
            for ev in events {
                if ev.event_type() == evdev::EventType::KEY {
                    let key = evdev::Key::new(ev.code());
                    key_events.push((key, ev.value())); // value: 1=press, 0=release
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }

    // Verify KEY_A press, KEY_A release, KEY_B press, KEY_B release (in order)
    let key_a = evdev::Key::KEY_A;
    let key_b = evdev::Key::KEY_B;

    let a_press   = key_events.iter().position(|(k, v)| *k == key_a && *v == 1);
    let a_release = key_events.iter().position(|(k, v)| *k == key_a && *v == 0);
    let b_press   = key_events.iter().position(|(k, v)| *k == key_b && *v == 1);
    let b_release = key_events.iter().position(|(k, v)| *k == key_b && *v == 0);

    assert!(a_press.is_some(),   "KEY_A press not found in evdev stream");
    assert!(a_release.is_some(), "KEY_A release not found in evdev stream");
    assert!(b_press.is_some(),   "KEY_B press not found in evdev stream");
    assert!(b_release.is_some(), "KEY_B release not found in evdev stream");

    // Order: a_press < a_release < b_press < b_release
    assert!(a_press.unwrap()   < a_release.unwrap(), "KEY_A release before press");
    assert!(a_release.unwrap() < b_press.unwrap(),   "KEY_B press before KEY_A release");
    assert!(b_press.unwrap()   < b_release.unwrap(), "KEY_B release before press");

    dev.ungrab().ok();
}

#[cfg(target_os = "linux")]
fn find_enigo_device() -> Option<std::path::PathBuf> {
    let input_dir = std::path::Path::new("/dev/input");
    if !input_dir.exists() {
        return None;
    }
    for entry in std::fs::read_dir(input_dir).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.to_string_lossy().contains("event") {
            if let Ok(dev) = evdev::Device::open(&path) {
                let name = dev.name().unwrap_or("");
                if name.to_lowercase().contains("enigo") {
                    return Some(path);
                }
            }
        }
    }
    None
}
