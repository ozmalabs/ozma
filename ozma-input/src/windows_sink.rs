// SPDX-License-Identifier: AGPL-3.0-only
//! Windows desktop HID injection sink backed by Win32 `SendInput` (via `enigo`).
//!
//! On Windows it opens an `Enigo` injector and translates [`InputEvent`]s
//! into OS keyboard/mouse events while tracking held state. On non-Windows
//! targets [`WindowsSendInputSink::open`] returns [`codes::UNSUPPORTED`].

#[cfg(target_os = "windows")]
use std::sync::Mutex;

use ozma_protocol::input::InputEvent;
use ozma_protocol::ApiError;

use crate::codes;
use crate::sink::{InputSink, SubmitReport};
use crate::state::HeldInput;

/// Windows `SendInput` input sink.
pub struct WindowsSendInputSink {
    #[cfg(target_os = "windows")]
    inner: Mutex<WindowsInner>,
    #[cfg(not(target_os = "windows"))]
    _private: (),
}

#[cfg(target_os = "windows")]
struct WindowsInner {
    enigo: enigo::Enigo,
    held: HeldInput,
}

impl WindowsSendInputSink {
    /// Open a Windows `SendInput` injector.
    pub fn open() -> Result<Self, ApiError> {
        #[cfg(target_os = "windows")]
        {
            let enigo = enigo::Enigo::new(&enigo::Settings::default()).map_err(|error| {
                ApiError::new(
                    codes::INPUT_UNAVAILABLE,
                    format!("failed to initialize Windows SendInput: {error}"),
                )
            })?;
            Ok(Self {
                inner: Mutex::new(WindowsInner {
                    enigo,
                    held: HeldInput::default(),
                }),
            })
        }
        #[cfg(not(target_os = "windows"))]
        {
            Err(ApiError::new(
                codes::UNSUPPORTED,
                "WindowsSendInputSink is only supported on Windows",
            ))
        }
    }
}

impl InputSink for WindowsSendInputSink {
    fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError> {
        #[cfg(target_os = "windows")]
        {
            use enigo::{Axis, Button, Coordinate, Direction, Keyboard, Mouse};
            use ozma_protocol::input::PointerButton;

            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());

            match event {
                InputEvent::Key {
                    code,
                    down,
                    modifiers: _,
                } => {
                    let usage = u8::try_from(*code).map_err(|_| {
                        ApiError::new(
                            codes::UNSUPPORTED,
                            format!("HID key usage out of range: 0x{code:x}"),
                        )
                    })?;
                    let key = hid_usage_to_enigo_key(usage).ok_or_else(|| {
                        ApiError::new(
                            codes::UNSUPPORTED,
                            format!("unmapped HID key usage 0x{usage:02x} for Windows SendInput"),
                        )
                    })?;
                    let dir = if *down {
                        Direction::Press
                    } else {
                        Direction::Release
                    };
                    inner.enigo.key(key, dir).map_err(|e| {
                        ApiError::new(codes::INPUT_UNAVAILABLE, format!("SendInput key: {e}"))
                    })?;
                    let changed = if *down {
                        inner.held.keys.insert(usage)
                    } else {
                        inner.held.keys.remove(&usage)
                    };
                    Ok(SubmitReport {
                        reports_written: 1,
                        state_changed: changed,
                    })
                }
                InputEvent::PointerAbsolute { x, y, .. } => {
                    inner
                        .enigo
                        .move_mouse(*x, *y, Coordinate::Abs)
                        .map_err(|e| {
                            ApiError::new(
                                codes::INPUT_UNAVAILABLE,
                                format!("SendInput move abs: {e}"),
                            )
                        })?;
                    Ok(SubmitReport {
                        reports_written: 1,
                        state_changed: true,
                    })
                }
                InputEvent::PointerRelative { dx, dy } => {
                    inner
                        .enigo
                        .move_mouse(*dx, *dy, Coordinate::Rel)
                        .map_err(|e| {
                            ApiError::new(
                                codes::INPUT_UNAVAILABLE,
                                format!("SendInput move rel: {e}"),
                            )
                        })?;
                    Ok(SubmitReport {
                        reports_written: 1,
                        state_changed: true,
                    })
                }
                InputEvent::Button { button, down } => {
                    let btn = match button {
                        PointerButton::Left => Button::Left,
                        PointerButton::Right => Button::Right,
                        PointerButton::Middle => Button::Middle,
                        PointerButton::Back => Button::Back,
                        PointerButton::Forward => Button::Forward,
                    };
                    let dir = if *down {
                        Direction::Press
                    } else {
                        Direction::Release
                    };
                    inner.enigo.button(btn, dir).map_err(|e| {
                        ApiError::new(codes::INPUT_UNAVAILABLE, format!("SendInput button: {e}"))
                    })?;
                    Ok(SubmitReport {
                        reports_written: 1,
                        state_changed: true,
                    })
                }
                InputEvent::Wheel { dx, dy } => {
                    if *dy != 0 {
                        inner.enigo.scroll(*dy, Axis::Vertical).map_err(|e| {
                            ApiError::new(
                                codes::INPUT_UNAVAILABLE,
                                format!("SendInput scroll v: {e}"),
                            )
                        })?;
                    }
                    if *dx != 0 {
                        inner.enigo.scroll(*dx, Axis::Horizontal).map_err(|e| {
                            ApiError::new(
                                codes::INPUT_UNAVAILABLE,
                                format!("SendInput scroll h: {e}"),
                            )
                        })?;
                    }
                    Ok(SubmitReport {
                        reports_written: 1,
                        state_changed: true,
                    })
                }
                InputEvent::ReleaseAll => {
                    release_held(&mut inner);
                    Ok(SubmitReport {
                        reports_written: 1,
                        state_changed: true,
                    })
                }
                _ => Err(ApiError::new(
                    codes::UNSUPPORTED,
                    "event kind is not supported by WindowsSendInputSink",
                )),
            }
        }
        #[cfg(not(target_os = "windows"))]
        {
            let _ = event;
            Err(ApiError::new(
                codes::UNSUPPORTED,
                "WindowsSendInputSink is only supported on Windows",
            ))
        }
    }

    fn release_all(&self) -> Result<(), ApiError> {
        #[cfg(target_os = "windows")]
        {
            let mut inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            release_held(&mut inner);
            Ok(())
        }
        #[cfg(not(target_os = "windows"))]
        {
            Ok(())
        }
    }

    fn held(&self) -> HeldInput {
        #[cfg(target_os = "windows")]
        {
            let inner = self
                .inner
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            inner.held.clone()
        }
        #[cfg(not(target_os = "windows"))]
        {
            HeldInput::default()
        }
    }
}

#[cfg(target_os = "windows")]
fn release_held(inner: &mut WindowsInner) {
    use enigo::{Direction, Keyboard};
    let keys: Vec<u8> = inner.held.keys.iter().copied().collect();
    for usage in keys {
        if let Some(key) = hid_usage_to_enigo_key(usage) {
            let _ = inner.enigo.key(key, Direction::Release);
        }
    }
    inner.held.keys.clear();
}

#[cfg(target_os = "windows")]
fn hid_usage_to_enigo_key(usage: u8) -> Option<enigo::Key> {
    use enigo::Key;
    match usage {
        0x04..=0x1D => Some(Key::Unicode((b'a' + (usage - 0x04)) as char)),
        0x1E..=0x26 => Some(Key::Unicode((b'1' + (usage - 0x1E)) as char)),
        0x27 => Some(Key::Unicode('0')),
        0x28 => Some(Key::Return),
        0x29 => Some(Key::Escape),
        0x2A => Some(Key::Backspace),
        0x2B => Some(Key::Tab),
        0x2C => Some(Key::Space),
        0x2D => Some(Key::Unicode('-')),
        0x2E => Some(Key::Unicode('=')),
        0x2F => Some(Key::Unicode('[')),
        0x30 => Some(Key::Unicode(']')),
        0x31 => Some(Key::Unicode('\\')),
        0x33 => Some(Key::Unicode(';')),
        0x34 => Some(Key::Unicode('\'')),
        0x35 => Some(Key::Unicode('`')),
        0x36 => Some(Key::Unicode(',')),
        0x37 => Some(Key::Unicode('.')),
        0x38 => Some(Key::Unicode('/')),
        0x39 => Some(Key::CapsLock),
        0x3A => Some(Key::F1),
        0x3B => Some(Key::F2),
        0x3C => Some(Key::F3),
        0x3D => Some(Key::F4),
        0x3E => Some(Key::F5),
        0x3F => Some(Key::F6),
        0x40 => Some(Key::F7),
        0x41 => Some(Key::F8),
        0x42 => Some(Key::F9),
        0x43 => Some(Key::F10),
        0x44 => Some(Key::F11),
        0x45 => Some(Key::F12),
        0x4A => Some(Key::Home),
        0x4B => Some(Key::PageUp),
        0x4C => Some(Key::Delete),
        0x4D => Some(Key::End),
        0x4E => Some(Key::PageDown),
        0x4F => Some(Key::RightArrow),
        0x50 => Some(Key::LeftArrow),
        0x51 => Some(Key::DownArrow),
        0x52 => Some(Key::UpArrow),
        0xE0 | 0xE4 => Some(Key::Control),
        0xE1 | 0xE5 => Some(Key::Shift),
        0xE2 | 0xE6 => Some(Key::Alt),
        0xE3 | 0xE7 => Some(Key::Meta),
        _ => None,
    }
}
