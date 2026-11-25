// SPDX-License-Identifier: AGPL-3.0-only
//! Real evdev/uinput backend for `ozma-seat-input`.
//!
//! Uses the safe `evdev` crate for `EVIOCGRAB` (`Device::grab`) and uinput
//! (`VirtualDeviceBuilder`). No unsafe lives here; the only unsafe call is
//! `ioctl::set_nonblocking` so the relay thread can poll its stop flag.
//!
//! The relay mirrors the source device's key/relative/absolute/switch
//! capabilities onto one uinput device per grabbed node and forwards events.
//! Force feedback, LEDs, and exotic event types are not mirrored; that is a
//! documented limitation, not an isolation claim.

use std::collections::BTreeMap;
use std::os::unix::io::AsRawFd;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::thread::JoinHandle;
use std::time::Duration;

use evdev::uinput::{VirtualDevice, VirtualDeviceBuilder};
use evdev::{AbsInfo, AbsoluteAxisType, BusType, Device, EventType, InputId, UinputAbsSetup};
use ozma_multiseat::{
    default_uinput_name, HelperBackend, HelperBackendKind, HelperDevice, HelperError,
    HelperErrorCode,
};

use crate::ioctl;

/// Poll interval for the non-blocking relay loop.
const RELAY_POLL: Duration = Duration::from_millis(4);

struct Relay {
    stop: Arc<AtomicBool>,
    handle: Option<JoinHandle<Result<(), String>>>,
}

/// Real backend: one open `Device` per un-relayed grab, one uinput device per
/// confirmed grab, and one relay thread per relayed pair.
pub struct RealBackend {
    devices: BTreeMap<String, Device>,
    uinputs: BTreeMap<String, VirtualDevice>,
    relays: BTreeMap<String, Relay>,
}

impl RealBackend {
    /// An empty backend. Opens nothing until a grab is requested.
    pub fn new() -> Self {
        Self {
            devices: BTreeMap::new(),
            uinputs: BTreeMap::new(),
            relays: BTreeMap::new(),
        }
    }
}

impl Default for RealBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl HelperBackend for RealBackend {
    fn backend_kind(&self) -> HelperBackendKind {
        HelperBackendKind::Real
    }

    fn grab(&mut self, device: &HelperDevice) -> Result<(), HelperError> {
        if self.devices.contains_key(&device.path) || self.relays.contains_key(&device.path) {
            return Err(HelperError::new(
                HelperErrorCode::CrossSeatConflict,
                format!("{} is already grabbed", device.path),
            )
            .with_device(&device.path));
        }
        if let Ok(metadata) = std::fs::symlink_metadata(&device.path) {
            if metadata.file_type().is_symlink() {
                return Err(HelperError::new(
                    HelperErrorCode::InvalidPath,
                    format!(
                        "{} is a symlink; only real event nodes are grabbed",
                        device.path
                    ),
                )
                .with_device(&device.path));
            }
        }
        let mut source = Device::open(&device.path).map_err(|error| {
            HelperError::new(
                HelperErrorCode::GrabFailed,
                format!("opening {}: {error}", device.path),
            )
            .with_device(&device.path)
        })?;
        source.grab().map_err(|error| {
            HelperError::new(
                HelperErrorCode::GrabFailed,
                format!("EVIOCGRAB {}: {error}", device.path),
            )
            .with_device(&device.path)
        })?;
        self.devices.insert(device.path.clone(), source);
        Ok(())
    }

    fn ungrab(&mut self, path: &str) -> Result<(), HelperError> {
        if self.relays.contains_key(path) {
            return self.stop_relay(path);
        }
        let Some(mut device) = self.devices.remove(path) else {
            return Err(
                HelperError::new(HelperErrorCode::NotGrabbed, "path is not grabbed")
                    .with_device(path.to_owned()),
            );
        };
        device.ungrab().map_err(|error| {
            HelperError::new(
                HelperErrorCode::Backend,
                format!("releasing EVIOCGRAB {path}: {error}"),
            )
            .with_device(path.to_owned())
        })
    }

    fn create_uinput(
        &mut self,
        seat_id: &str,
        device: &HelperDevice,
    ) -> Result<String, HelperError> {
        let source = self.devices.get(&device.path).ok_or_else(|| {
            HelperError::new(
                HelperErrorCode::UinputFailed,
                format!("cannot create uinput for ungrabbed {}", device.path),
            )
            .with_device(&device.path)
        })?;
        let name = default_uinput_name(seat_id, device.kind, &device.path);

        let mut builder = VirtualDeviceBuilder::new()
            .map_err(|error| uinput_error(&device.path, error))?
            .name(&name)
            .input_id(InputId::new(BusType::BUS_USB, 0x1209, 0x0002, 1));
        if let Some(keys) = source.supported_keys() {
            builder = builder
                .with_keys(keys)
                .map_err(|error| uinput_error(&device.path, error))?;
        }
        if let Some(axes) = source.supported_relative_axes() {
            builder = builder
                .with_relative_axes(axes)
                .map_err(|error| uinput_error(&device.path, error))?;
        }
        if let Some(axes) = source.supported_absolute_axes() {
            for axis in axes.iter() {
                builder = builder
                    .with_absolute_axis(&absolute_setup(axis))
                    .map_err(|error| uinput_error(&device.path, error))?;
            }
        }
        if let Some(switches) = source.supported_switches() {
            builder = builder
                .with_switches(switches)
                .map_err(|error| uinput_error(&device.path, error))?;
        }
        let uinput = builder
            .build()
            .map_err(|error| uinput_error(&device.path, error))?;
        self.uinputs.insert(name.clone(), uinput);
        Ok(name)
    }

    fn start_relay(&mut self, device: &HelperDevice, uinput_name: &str) -> Result<(), HelperError> {
        let source = self.devices.remove(&device.path).ok_or_else(|| {
            HelperError::new(
                HelperErrorCode::Backend,
                format!("cannot relay ungrabbed {}", device.path),
            )
            .with_device(&device.path)
        })?;
        let uinput = self.uinputs.remove(uinput_name).ok_or_else(|| {
            HelperError::new(
                HelperErrorCode::UinputFailed,
                format!("uinput {uinput_name} was not created"),
            )
            .with_device(&device.path)
        })?;

        ioctl::set_nonblocking(source.as_raw_fd()).map_err(|error| {
            HelperError::new(
                HelperErrorCode::Backend,
                format!("setting {} non-blocking: {error}", device.path),
            )
            .with_device(&device.path)
        })?;

        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let path = device.path.clone();
        let handle = std::thread::Builder::new()
            .name(format!("ozma-seat-input-relay-{path}"))
            .spawn(move || relay_loop(source, uinput, thread_stop))
            .map_err(|error| {
                HelperError::new(
                    HelperErrorCode::Backend,
                    format!("starting relay for {path}: {error}"),
                )
                .with_device(&path)
            })?;
        self.relays.insert(
            device.path.clone(),
            Relay {
                stop,
                handle: Some(handle),
            },
        );
        Ok(())
    }

    fn stop_relay(&mut self, path: &str) -> Result<(), HelperError> {
        let Some(mut relay) = self.relays.remove(path) else {
            // The relay may have exited on its own after a read error (it
            // ungrabs before returning) or may never have started. Either way
            // no grab is held, so releasing is idempotent rather than stuck.
            if let Some(mut device) = self.devices.remove(path) {
                return device.ungrab().map_err(|error| {
                    HelperError::new(
                        HelperErrorCode::Backend,
                        format!("releasing EVIOCGRAB {path}: {error}"),
                    )
                    .with_device(path.to_owned())
                });
            }
            return Ok(());
        };
        relay.stop.store(true, Ordering::Relaxed);
        let result = relay.handle.take().map(|handle| handle.join());
        match result {
            Some(Ok(Ok(()))) => Ok(()),
            Some(Ok(Err(message))) => {
                Err(HelperError::new(HelperErrorCode::Backend, message)
                    .with_device(path.to_owned()))
            }
            Some(Err(_)) => Err(HelperError::new(
                HelperErrorCode::Backend,
                "relay thread panicked",
            )
            .with_device(path.to_owned())),
            None => Ok(()),
        }
    }

    fn destroy_uinput(&mut self, name: &str) -> Result<(), HelperError> {
        // A relayed uinput device was dropped by its relay thread when
        // `stop_relay` joined. Removing it here is a no-op in that case.
        self.uinputs.remove(name);
        Ok(())
    }
}

impl Drop for RealBackend {
    fn drop(&mut self) {
        for (_, relay) in std::mem::take(&mut self.relays) {
            relay.stop.store(true, Ordering::Relaxed);
            if let Some(handle) = relay.handle {
                let _ = handle.join();
            }
        }
        for (_, mut device) in std::mem::take(&mut self.devices) {
            let _ = device.ungrab();
        }
        self.uinputs.clear();
    }
}

fn relay_loop(
    mut source: Device,
    mut uinput: VirtualDevice,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    while !stop.load(Ordering::Relaxed) {
        let fetched: Result<Vec<_>, _> = source.fetch_events().map(|events| events.collect());
        match fetched {
            Ok(batch) => {
                let batch: Vec<_> = batch
                    .into_iter()
                    .filter(|event| {
                        !(event.event_type() == EventType::SYNCHRONIZATION && event.code() == 0)
                    })
                    .collect();
                if !batch.is_empty() {
                    uinput
                        .emit(&batch)
                        .map_err(|error| format!("writing uinput events: {error}"))?;
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(RELAY_POLL);
            }
            Err(error) => {
                let _ = source.ungrab();
                return Err(format!("reading evdev events: {error}"));
            }
        }
    }
    source
        .ungrab()
        .map_err(|error| format!("releasing EVIOCGRAB: {error}"))
}

fn uinput_error(path: &str, error: std::io::Error) -> HelperError {
    HelperError::new(
        HelperErrorCode::UinputFailed,
        format!("creating uinput device for {path}: {error}"),
    )
    .with_device(path.to_owned())
}

fn absolute_setup(axis: AbsoluteAxisType) -> UinputAbsSetup {
    // The public evdev API exposes the axis type but not the source's AbsInfo
    // in a constructible form. A full-range default keeps ABS events usable;
    // games with calibrated sticks may prefer a real device-specific range.
    UinputAbsSetup::new(axis, AbsInfo::new(0, 0, 65_535, 0, 0, 0))
}
