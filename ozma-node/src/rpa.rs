// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! RPA helper primitives shared by the controller compatibility routes.
//!
//! This module deliberately contains no automation policy: it maps the
//! Python node's key names/modifier bits to HID usages and runs the optional
//! `tesseract` CLI over a real captured JPEG. The HTTP layer owns auth,
//! capability advertisement, and typed errors.
//!
//! OCR is best-effort and never synthesized: when the binary is absent the
//! caller returns a typed 503; when it runs, the returned text is exactly what
//! tesseract wrote to stdout (trailing whitespace trimmed). Input, frame
//! bytes, pixel count, and output are all bounded.

use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::io::AsyncWriteExt;
use tokio::process::Command;

use crate::keyboard_layout::key_event;

/// Bound for one tesseract invocation (spawn + read + wait).
pub const DEFAULT_OCR_TIMEOUT: Duration = Duration::from_secs(20);
/// Largest JPEG accepted for OCR (16 MiB).
pub const MAX_OCR_FRAME_BYTES: usize = 16 * 1024 * 1024;
/// Largest captured frame accepted for OCR (8192x4096 pixels).
pub const MAX_OCR_PIXELS: u64 = 8192 * 4096;
/// Largest tesseract stdout accepted (1 MiB).
pub const MAX_OCR_TEXT_BYTES: usize = 1024 * 1024;

/// Optional tesseract control attached to [`crate::controller_compat::ConnectionState`].
///
/// `detect()` resolves `tesseract` on `PATH`; `unavailable()` and
/// `with_program()` make the route deterministic in tests and on nodes that
/// intentionally do not install OCR.
#[derive(Clone, Debug)]
pub struct OcrControl {
    program: Option<PathBuf>,
    timeout: Duration,
}

impl Default for OcrControl {
    fn default() -> Self {
        Self::detect()
    }
}

impl OcrControl {
    /// Use `tesseract` from `PATH` if it is installed.
    pub fn detect() -> Self {
        Self {
            program: find_executable("tesseract"),
            timeout: DEFAULT_OCR_TIMEOUT,
        }
    }

    /// Report OCR as unavailable; the route answers `rpa.ocr-unavailable`.
    pub fn unavailable() -> Self {
        Self {
            program: None,
            timeout: DEFAULT_OCR_TIMEOUT,
        }
    }

    /// Use an explicit tesseract-compatible program path.
    ///
    /// This is primarily for tests that install a fake CLI; production uses
    /// [`Self::detect`].
    pub fn with_program(program: impl Into<PathBuf>) -> Self {
        Self {
            program: Some(program.into()),
            timeout: DEFAULT_OCR_TIMEOUT,
        }
    }

    /// Bound the whole OCR invocation.
    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    /// Whether an OCR program is configured.
    pub fn is_available(&self) -> bool {
        self.program.is_some()
    }

    /// Resolved program path, when available.
    pub fn program(&self) -> Option<&Path> {
        self.program.as_deref()
    }

    /// OCR one captured JPEG frame and return tesseract's text.
    ///
    /// The frame is piped on stdin, so no temporary image is written. The
    /// output is bounded and trimmed; an empty screen yields an empty string,
    /// never invented text.
    pub async fn recognise(
        &self,
        jpeg: &[u8],
        width: u32,
        height: u32,
    ) -> Result<String, OcrError> {
        let Some(program) = self.program.as_ref() else {
            return Err(OcrError::Unavailable);
        };
        if jpeg.len() > MAX_OCR_FRAME_BYTES {
            return Err(OcrError::FrameTooLarge {
                bytes: jpeg.len(),
                limit: MAX_OCR_FRAME_BYTES,
            });
        }
        let pixels = u64::from(width) * u64::from(height);
        if pixels > MAX_OCR_PIXELS {
            return Err(OcrError::FrameTooManyPixels {
                width,
                height,
                limit: MAX_OCR_PIXELS,
            });
        }

        let mut child = Command::new(program)
            .arg("stdin")
            .arg("stdout")
            .arg("--psm")
            .arg("3")
            .arg("-l")
            .arg("eng")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(OcrError::Spawn)?;

        let mut stdin = child.stdin.take().ok_or_else(|| {
            OcrError::Spawn(io::Error::new(
                io::ErrorKind::BrokenPipe,
                "tesseract child has no stdin pipe",
            ))
        })?;
        stdin.write_all(jpeg).await.map_err(OcrError::Io)?;
        drop(stdin);

        let output = match tokio::time::timeout(self.timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(error)) => return Err(OcrError::Spawn(error)),
            Err(_) => {
                return Err(OcrError::Timeout {
                    timeout: self.timeout,
                })
            }
        };

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(OcrError::Failed {
                status: output.status.to_string(),
                stderr: truncate_chars(stderr.trim(), 512),
            });
        }
        if output.stdout.len() > MAX_OCR_TEXT_BYTES {
            return Err(OcrError::OutputTooLarge {
                bytes: output.stdout.len(),
                limit: MAX_OCR_TEXT_BYTES,
            });
        }

        Ok(String::from_utf8_lossy(&output.stdout)
            .trim_end()
            .to_owned())
    }
}

/// OCR failures mapped to typed HTTP responses by the compatibility layer.
#[derive(Debug, thiserror::Error)]
pub enum OcrError {
    /// No tesseract binary was found or configured.
    #[error("tesseract is not installed or not on PATH")]
    Unavailable,
    /// The JPEG exceeded [`MAX_OCR_FRAME_BYTES`].
    #[error("frame is {bytes} bytes; the OCR limit is {limit} bytes")]
    FrameTooLarge {
        /// Actual JPEG length.
        bytes: usize,
        /// Accepted maximum.
        limit: usize,
    },
    /// The captured dimensions exceeded [`MAX_OCR_PIXELS`].
    #[error("frame is {width}x{height}; the OCR pixel limit is {limit}")]
    FrameTooManyPixels {
        /// Frame width.
        width: u32,
        /// Frame height.
        height: u32,
        /// Accepted maximum pixel count.
        limit: u64,
    },
    /// The process could not be spawned or waited for.
    #[error("running tesseract failed: {0}")]
    Spawn(#[source] io::Error),
    /// Writing the frame to the process failed.
    #[error("writing the OCR frame to tesseract failed: {0}")]
    Io(#[source] io::Error),
    /// The process exceeded the configured bound.
    #[error("tesseract did not finish within {timeout:?}")]
    Timeout {
        /// Bound that elapsed.
        timeout: Duration,
    },
    /// The process exited non-zero.
    #[error("tesseract exited with {status}: {stderr}")]
    Failed {
        /// Exit status.
        status: String,
        /// Bounded stderr excerpt.
        stderr: String,
    },
    /// Tesseract wrote more text than [`MAX_OCR_TEXT_BYTES`].
    #[error("tesseract produced {bytes} bytes of text; the limit is {limit} bytes")]
    OutputTooLarge {
        /// Actual stdout length.
        bytes: usize,
        /// Accepted maximum.
        limit: usize,
    },
}

/// Map the Python node's named key spellings to USB HID keyboard usages.
///
/// Single printable characters are handled by the caller through the selected
/// keyboard layout, not here, so dead keys and AltGr remain layout-aware.
pub fn named_key_usage(name: &str) -> Option<u8> {
    let name = name.trim().to_ascii_lowercase();
    let usage = match name.as_str() {
        "enter" | "return" => 0x28,
        "esc" | "escape" => 0x29,
        "backspace" | "back" => 0x2A,
        "tab" => 0x2B,
        "space" | "spacebar" => 0x2C,
        "capslock" | "caps" => 0x39,
        "printscreen" | "print" => 0x46,
        "scrolllock" | "scroll" => 0x47,
        "pause" => 0x48,
        "insert" | "ins" => 0x49,
        "home" => 0x4A,
        "pageup" | "pgup" => 0x4B,
        "delete" | "del" => 0x4C,
        "end" => 0x4D,
        "pagedown" | "pgdn" => 0x4E,
        "right" | "arrowright" => 0x4F,
        "left" | "arrowleft" => 0x50,
        "down" | "arrowdown" => 0x51,
        "up" | "arrowup" => 0x52,
        "numlock" => 0x53,
        "menu" | "apps" => 0x65,
        "minus" | "hyphen" => 0x2D,
        "equal" | "equals" => 0x2E,
        "leftbracket" => 0x2F,
        "rightbracket" => 0x30,
        "backslash" => 0x31,
        "semicolon" => 0x33,
        "apostrophe" | "quote" => 0x34,
        "grave" | "backtick" => 0x35,
        "comma" => 0x36,
        "period" | "dot" => 0x37,
        "slash" => 0x38,
        _ => {
            let function = name.strip_prefix('f')?.parse::<u8>().ok()?;
            if !(1..=12).contains(&function) {
                return None;
            }
            0x3A + (function - 1)
        }
    };
    Some(usage)
}

/// Map a modifier name to the USB HID boot-keyboard modifier bit.
pub fn modifier_bit(name: &str) -> Option<u8> {
    let bit = match name.trim().to_ascii_lowercase().as_str() {
        "ctrl" | "control" => 0x01,
        "shift" => 0x02,
        "alt" => 0x04,
        "meta" | "gui" | "win" | "super" | "command" | "cmd" => 0x08,
        "rightctrl" | "rctrl" => 0x10,
        "rightshift" | "rshift" => 0x20,
        "altgr" | "rightalt" | "ralt" => 0x40,
        "rightgui" | "rightmeta" | "rwin" => 0x80,
        _ => return None,
    };
    Some(bit)
}

/// Modifier HID usages for a boot-keyboard modifier bitmask, in press order.
pub fn modifier_usages(bits: u8) -> Vec<u8> {
    const MODIFIERS: [(u8, u8); 8] = [
        (0x01, 0xE0), // left ctrl
        (0x02, 0xE1), // left shift
        (0x04, 0xE2), // left alt
        (0x08, 0xE3), // left gui
        (0x10, 0xE4), // right ctrl
        (0x20, 0xE5), // right shift
        (0x40, 0xE6), // right alt / AltGr
        (0x80, 0xE7), // right gui
    ];
    MODIFIERS
        .iter()
        .filter(|(bit, _)| bits & bit != 0)
        .map(|(_, usage)| *usage)
        .collect()
}

/// Build one balanced key press/release with the requested modifier bits.
///
/// Modifiers are pressed in canonical order, released in reverse, and the key
/// itself is held by the caller between the down/up halves.
pub fn key_events(usage: u8, modifier_bits: u8) -> Vec<ozma_protocol::input::InputEvent> {
    let modifiers = modifier_usages(modifier_bits);
    let mut events = Vec::with_capacity(2 + modifiers.len() * 2);
    for &modifier in &modifiers {
        events.push(key_event(u32::from(modifier), true));
    }
    events.push(key_event(u32::from(usage), true));
    events.push(key_event(u32::from(usage), false));
    for &modifier in modifiers.iter().rev() {
        events.push(key_event(u32::from(modifier), false));
    }
    events
}

fn find_executable(name: &str) -> Option<PathBuf> {
    let requested = Path::new(name);
    if requested.components().count() > 1 {
        return is_executable(requested).then(|| requested.to_path_buf());
    }
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|directory| directory.join(name))
        .find(|candidate| is_executable(candidate))
}

#[cfg(unix)]
fn is_executable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.metadata()
        .map(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
fn is_executable(path: &Path) -> bool {
    path.is_file()
}

fn truncate_chars(value: &str, limit: usize) -> String {
    if value.chars().count() <= limit {
        return value.to_owned();
    }
    let mut truncated: String = value.chars().take(limit).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::input::InputEvent;

    fn key(code: u32, down: bool) -> InputEvent {
        InputEvent::Key {
            code,
            down,
            modifiers: ozma_protocol::input::KeyModifiers(0),
        }
    }

    #[test]
    fn named_keys_cover_the_python_node_spellings_and_aliases() {
        for (name, usage) in [
            ("enter", 0x28),
            ("return", 0x28),
            ("esc", 0x29),
            ("backspace", 0x2A),
            ("tab", 0x2B),
            ("space", 0x2C),
            ("delete", 0x4C),
            ("up", 0x52),
            ("pagedown", 0x4E),
            ("f1", 0x3A),
            ("f12", 0x45),
            ("insert", 0x49),
            ("printscreen", 0x46),
            ("menu", 0x65),
        ] {
            assert_eq!(named_key_usage(name), Some(usage), "{name}");
        }
        assert_eq!(named_key_usage("F12"), Some(0x45));
        assert_eq!(named_key_usage("f13"), None);
        assert_eq!(named_key_usage("not-a-key"), None);
    }

    #[test]
    fn modifiers_map_to_boot_bits_and_press_in_order() {
        assert_eq!(modifier_bit("ctrl"), Some(0x01));
        assert_eq!(modifier_bit("Control"), Some(0x01));
        assert_eq!(modifier_bit("altgr"), Some(0x40));
        assert_eq!(modifier_bit("super"), Some(0x08));
        assert_eq!(modifier_bit("hyper"), None);
        assert_eq!(modifier_usages(0x01 | 0x02 | 0x40), vec![0xE0, 0xE1, 0xE6]);
    }

    #[test]
    fn key_events_press_modifiers_first_and_release_them_in_reverse() {
        assert_eq!(
            key_events(0x06, 0x01 | 0x02),
            vec![
                key(0xE0, true),
                key(0xE1, true),
                key(0x06, true),
                key(0x06, false),
                key(0xE1, false),
                key(0xE0, false),
            ]
        );
        assert_eq!(key_events(0x28, 0), vec![key(0x28, true), key(0x28, false)]);
    }

    #[test]
    fn unavailable_ocr_is_typed_without_spawning_a_process() {
        let control = OcrControl::unavailable();
        assert!(!control.is_available());
        assert!(control.program().is_none());
        let error = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap()
            .block_on(control.recognise(b"not a jpeg", 1, 1))
            .unwrap_err();
        assert!(matches!(error, OcrError::Unavailable));
    }

    #[test]
    fn ocr_rejects_oversized_frames_before_spawning() {
        let control = OcrControl::with_program("/bin/false");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let large = vec![0u8; MAX_OCR_FRAME_BYTES + 1];
        let error = runtime
            .block_on(control.recognise(&large, 1, 1))
            .unwrap_err();
        assert!(matches!(
            error,
            OcrError::FrameTooLarge {
                bytes,
                limit: MAX_OCR_FRAME_BYTES
            } if bytes == MAX_OCR_FRAME_BYTES + 1
        ));

        let error = runtime
            .block_on(control.recognise(b"jpeg", u32::MAX, u32::MAX))
            .unwrap_err();
        assert!(matches!(error, OcrError::FrameTooManyPixels { .. }));
    }
}
