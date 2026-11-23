// SPDX-License-Identifier: AGPL-3.0-only
//! USB HID gadget sink backed by `std::fs` / `std::io`.
//!
//! The sink opens the kernel's ConfigFS gadget device nodes (normally
//! `/dev/hidg0` for the keyboard and `/dev/hidg1` for the mouse) once and
//! writes one report per `write_all` call. It never uses `unsafe`, C bindings,
//! or a native dependency, so it builds on every target and can be pointed at
//! regular files in tests.

use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, MutexGuard};

use ozma_protocol::input::InputEvent;
use ozma_protocol::ApiError;

use crate::error;
use crate::report::{AbsoluteMapping, MouseProtocol};
use crate::sink::{InputSink, SubmitReport};
use crate::state::{HeldInput, Report, Router};

/// Default keyboard gadget device created by the Ozma gadget setup.
pub const DEFAULT_KEYBOARD_PATH: &str = "/dev/hidg0";
/// Default mouse gadget device created by the Ozma gadget setup.
pub const DEFAULT_MOUSE_PATH: &str = "/dev/hidg1";

/// Construction options for [`HidGadgetSink`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GadgetOptions {
    /// Mouse report layout. The default matches the Ozma six-byte absolute
    /// gadget descriptor.
    pub mouse_protocol: MouseProtocol,
    /// Mapping from event pixel coordinates to absolute report coordinates.
    /// The default is [`AbsoluteMapping::hid_space`].
    pub mapping: AbsoluteMapping,
}

impl Default for GadgetOptions {
    fn default() -> Self {
        Self {
            mouse_protocol: MouseProtocol::default(),
            mapping: AbsoluteMapping::default(),
        }
    }
}

#[derive(Debug)]
struct Devices {
    keyboard: File,
    mouse: File,
}

impl Devices {
    fn write(
        &mut self,
        report: Report,
        keyboard_path: &Path,
        mouse_path: &Path,
    ) -> Result<(), ApiError> {
        match report {
            Report::Keyboard(report) => {
                write_report(&mut self.keyboard, keyboard_path, &report.to_bytes())
            }
            Report::RelativeMouse(report) => {
                write_report(&mut self.mouse, mouse_path, &report.to_bytes())
            }
            Report::AbsoluteMouse(report) => {
                write_report(&mut self.mouse, mouse_path, &report.to_bytes())
            }
        }
    }
}

#[derive(Debug)]
struct Inner {
    devices: Devices,
    router: Router,
}

/// HID sink that writes boot/absolute reports to gadget device nodes.
///
/// Submissions are serialized through one mutex. For a normal event the sink
/// clones the held state, computes the reports, writes them, and commits the
/// clone only after every write succeeds. A failed write therefore leaves the
/// in-memory state untouched and the next event can retry safely.
///
/// [`InputSink::release_all`] is the exception: it clears held state even if a
/// neutral report cannot be written, because clearing local state is what
/// prevents a later reconnect from believing keys are still down. The write
/// error is still returned.
#[derive(Debug)]
pub struct HidGadgetSink {
    keyboard_path: PathBuf,
    mouse_path: PathBuf,
    options: GadgetOptions,
    inner: Mutex<Inner>,
}

impl HidGadgetSink {
    /// Open the default gadget paths with default options.
    pub fn open() -> Result<Self, ApiError> {
        Self::with_paths(DEFAULT_KEYBOARD_PATH, DEFAULT_MOUSE_PATH)
    }

    /// Open explicit gadget paths with default options.
    pub fn with_paths(
        keyboard: impl AsRef<Path>,
        mouse: impl AsRef<Path>,
    ) -> Result<Self, ApiError> {
        Self::with_options(keyboard, mouse, GadgetOptions::default())
    }

    /// Open explicit gadget paths with an explicit mouse mode and mapping.
    ///
    /// Both devices are opened before the sink is constructed. If either open
    /// fails, the other handle is dropped and the error names the failing path.
    pub fn with_options(
        keyboard: impl AsRef<Path>,
        mouse: impl AsRef<Path>,
        options: GadgetOptions,
    ) -> Result<Self, ApiError> {
        let keyboard_path = keyboard.as_ref().to_path_buf();
        let mouse_path = mouse.as_ref().to_path_buf();

        let keyboard = open_device(&keyboard_path)?;
        let mouse = open_device(&mouse_path)?;

        Ok(Self {
            keyboard_path,
            mouse_path,
            options,
            inner: Mutex::new(Inner {
                devices: Devices { keyboard, mouse },
                router: Router::new(options.mapping, options.mouse_protocol),
            }),
        })
    }

    pub fn keyboard_path(&self) -> &Path {
        &self.keyboard_path
    }

    pub fn mouse_path(&self) -> &Path {
        &self.mouse_path
    }

    pub fn options(&self) -> GadgetOptions {
        self.options
    }

    pub fn mouse_protocol(&self) -> MouseProtocol {
        self.options.mouse_protocol
    }

    fn inner(&self) -> MutexGuard<'_, Inner> {
        crate::sink::lock(&self.inner)
    }
}

impl InputSink for HidGadgetSink {
    fn submit_report(&self, event: &InputEvent) -> Result<SubmitReport, ApiError> {
        let mut inner = self.inner();
        let mut next = inner.router.clone();
        let reports = next.apply(event)?;
        let reports_written = reports.len();

        for report in reports {
            inner
                .devices
                .write(report, &self.keyboard_path, &self.mouse_path)?;
        }

        inner.router = next;
        Ok(SubmitReport {
            reports_written,
            state_changed: reports_written > 0,
        })
    }

    fn release_all(&self) -> Result<(), ApiError> {
        let mut inner = self.inner();
        let reports = inner.router.release_all();

        // Try every neutral report even if an earlier one fails, and surface
        // the first error afterwards.
        let mut first_error = None;
        for report in reports {
            if let Err(error) = inner
                .devices
                .write(report, &self.keyboard_path, &self.mouse_path)
            {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    fn held(&self) -> HeldInput {
        self.inner().router.snapshot()
    }
}

fn open_device(path: &Path) -> Result<File, ApiError> {
    // No create/truncate: a gadget node must already exist, and opening with
    // write-only is what the kernel gadget driver expects.
    OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(|error| error::open_error(path, error))
}

fn write_report(file: &mut File, path: &Path, bytes: &[u8]) -> Result<(), ApiError> {
    file.write_all(bytes)
        .map_err(|error| error::write_error(path, error))
}
