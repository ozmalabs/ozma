// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Runtime wiring for the UAC2 isochronous bridge and the `/usb` status view.
//!
//! `ozma-usb` has a transport-independent [`ozma_usb::Uac2Bridge`] with
//! injectable endpoint and audio traits. This module supplies the real node
//! side:
//!
//! * an ALSA endpoint adapter over the kernel UAC2 gadget card (the card's
//!   capture side is host→device playback, its playback side is device→host
//!   capture),
//! * real ALSA/PipeWire audio adapters from `ozma-audio`,
//! * a bounded pump thread with a shared, serializable status snapshot for
//!   `GET /usb`, and
//! * an injected device factory so default tests never open a real UDC or
//!   ALSA device.
//!
//! The bridge is started only after the UAC2 ConfigFS function, the gadget ALSA
//! card, and the host audio devices all open successfully. A failure is a typed
//! [`ozma_usb::UsbError`] and the shared report stays `unsupported`; status
//! never claims `running` without PCM counters.

use std::{
    path::Path,
    sync::{
        atomic::{AtomicBool, Ordering},
        mpsc as std_mpsc, Arc, Mutex, MutexGuard,
    },
    thread::{self, JoinHandle},
    time::Duration,
};

use alsa::{
    pcm::{Access, Format, Frames, HwParams, PCM},
    Direction, ValueOr,
};
use ozma_audio::{AlsaUac2Config, AlsaUac2Sink, AlsaUac2Source};
use ozma_usb::{
    Uac2Bridge, Uac2BridgeConfig, Uac2BridgeInputs, Uac2BridgeReport, Uac2BridgeStatus, Uac2Config,
    Uac2Direction, Uac2Endpoint, Uac2EndpointIo, Uac2PcmFormat, Uac2SampleSize as UsbSampleSize,
    UsbError, UAC2_PLAYBACK_ENDPOINT,
};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

use crate::config::{NodeConfig, DEFAULT_USB_GADGET_NAME};

/// How long `start` waits for the worker to open devices and build a bridge.
pub const UAC2_BRIDGE_START_TIMEOUT: Duration = Duration::from_secs(5);
/// Default pump interval. USB high-speed isochronous service is 125 µs; 1 ms
/// keeps the bounded ring small without busy-spinning the node.
pub const UAC2_BRIDGE_POLL_INTERVAL: Duration = Duration::from_millis(1);
/// Default playback ring capacity (about 170 ms of 48 kHz stereo S16).
pub const UAC2_BRIDGE_RING_BYTES: usize = 64 * 1024;

/// Typed adapter error exposed alongside the bridge report on `/usb`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Uac2BridgeError {
    /// Stable `UsbError::kind()` value such as `uac2-endpoint`.
    pub kind: String,
    /// Human-readable detail, without secrets or backing paths.
    pub message: String,
}

impl Uac2BridgeError {
    /// Build from a typed USB error.
    pub fn from_usb(error: &UsbError) -> Self {
        Self {
            kind: error.kind().to_owned(),
            message: error.to_string(),
        }
    }
}

#[derive(Clone, Debug)]
struct SharedState {
    report: Uac2BridgeReport,
    last_error: Option<Uac2BridgeError>,
}

/// Cloneable status snapshot shared by the pump thread and `/usb`.
#[derive(Clone, Debug)]
pub struct Uac2BridgeShared {
    state: Arc<Mutex<SharedState>>,
}

impl Uac2BridgeShared {
    /// A report that is explicitly unsupported, with a typed reason.
    pub fn unsupported(reason: impl Into<String>) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                report: Uac2BridgeReport::unsupported(reason),
                last_error: None,
            })),
        }
    }

    /// Wrap a report supplied by a test or an embedding caller.
    ///
    /// A `running` report without PCM counters is downgraded to `configured`,
    /// so no injected snapshot can claim live audio.
    pub fn from_report(report: Uac2BridgeReport) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                report: normalize_report(report),
                last_error: None,
            })),
        }
    }

    /// Wrap a report and a typed adapter error.
    pub fn from_report_with_error(report: Uac2BridgeReport, error: Uac2BridgeError) -> Self {
        Self {
            state: Arc::new(Mutex::new(SharedState {
                report: report_after_error(report),
                last_error: Some(error),
            })),
        }
    }

    /// Current bridge report. Safe to call from any thread.
    pub fn report(&self) -> Uac2BridgeReport {
        lock(&self.state).report.clone()
    }

    /// Last typed adapter/bridge error, if any.
    pub fn last_error(&self) -> Option<Uac2BridgeError> {
        lock(&self.state).last_error.clone()
    }

    fn set(&self, report: Uac2BridgeReport) {
        lock(&self.state).report = report;
    }

    fn set_with_error(&self, report: Uac2BridgeReport, error: Uac2BridgeError) {
        let mut state = lock(&self.state);
        state.report = report_after_error(report);
        state.last_error = Some(error);
    }
}

/// Downgrade a `running` snapshot when the last pump failed.
///
/// Counters can move on the ALSA side even when the USB endpoint rejects the
/// transfer (for example a virtual UDC that does not implement isochronous
/// URBs). The typed error is the honest state; `configured` keeps `/usb` from
/// claiming live bridge audio until a pump actually succeeds again.
fn report_after_error(report: Uac2BridgeReport) -> Uac2BridgeReport {
    let mut report = normalize_report(report);
    if report.status == Uac2BridgeStatus::Running {
        report.status = Uac2BridgeStatus::Configured;
    }
    report
}

/// Devices and plans the factory must open inside the pump thread.
#[derive(Clone, Debug)]
pub struct Uac2BridgeDeviceConfig {
    /// Bridge instance name (`audio`).
    pub instance: String,
    /// UAC2 function descriptor model used for bridge plans.
    pub uac2: Uac2Config,
    /// Kernel UAC2 gadget playback PCM (`hw:N,0`, device→host).
    pub gadget_playback_device: String,
    /// Kernel UAC2 gadget capture PCM (`hw:N,1`, host→device).
    pub gadget_capture_device: String,
    /// Host playback device receiving host→device PCM.
    pub host_sink_device: String,
    /// Host capture device feeding device→host PCM.
    pub host_source_device: String,
}

/// Build the bridge inputs. Called inside the pump thread, so the returned
/// trait objects never cross a thread boundary.
pub trait Uac2BridgeDeviceFactory: Send + Sync {
    /// Open endpoints and audio for one bridge.
    fn open(&self, config: &Uac2BridgeDeviceConfig) -> Result<Uac2BridgeInputs, UsbError>;
}

/// A running (or stopped) UAC2 pump with shared status.
pub struct Uac2BridgeRuntime {
    shared: Uac2BridgeShared,
    stop: Arc<AtomicBool>,
    thread: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for Uac2BridgeRuntime {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Uac2BridgeRuntime")
            .field("report", &self.shared.report())
            .field("last_error", &self.shared.last_error())
            .finish()
    }
}

impl Uac2BridgeRuntime {
    /// Start with a concrete factory.
    ///
    /// The worker opens every device, builds and starts the bridge, and only
    /// then reports readiness. A missing device returns the factory's typed
    /// error and leaves the shared report `unsupported`.
    pub fn start_with(
        config: Uac2BridgeConfig,
        devices: Uac2BridgeDeviceConfig,
        factory: Box<dyn Uac2BridgeDeviceFactory>,
        poll_interval: Duration,
        start_timeout: Duration,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        let shared = Uac2BridgeShared::unsupported("UAC2 bridge is starting");
        let stop = Arc::new(AtomicBool::new(false));
        let worker_shared = shared.clone();
        let worker_stop = Arc::clone(&stop);
        let instance = config.instance.clone();
        let (ready_tx, ready_rx) = std_mpsc::sync_channel(1);

        let thread = thread::Builder::new()
            .name(format!("ozma-uac2-{instance}"))
            .spawn(move || {
                run_bridge(
                    config,
                    devices,
                    factory,
                    worker_shared,
                    worker_stop,
                    poll_interval,
                    ready_tx,
                );
            })
            .map_err(|error| UsbError::Uac2Endpoint {
                address: UAC2_PLAYBACK_ENDPOINT,
                operation: "start",
                reason: format!("cannot start the UAC2 pump thread: {error}"),
            })?;

        match ready_rx.recv_timeout(start_timeout) {
            Ok(Ok(())) => Ok(Self {
                shared,
                stop,
                thread: Some(thread),
            }),
            Ok(Err(error)) => {
                let _ = thread.join();
                Err(error)
            }
            Err(std_mpsc::RecvTimeoutError::Timeout) => {
                stop.store(true, Ordering::Relaxed);
                let _ = thread.join();
                Err(UsbError::Uac2Endpoint {
                    address: UAC2_PLAYBACK_ENDPOINT,
                    operation: "start",
                    reason: format!(
                        "UAC2 bridge did not open devices within {} ms",
                        start_timeout.as_millis()
                    ),
                })
            }
            Err(std_mpsc::RecvTimeoutError::Disconnected) => {
                let _ = thread.join();
                Err(UsbError::Uac2Endpoint {
                    address: UAC2_PLAYBACK_ENDPOINT,
                    operation: "start",
                    reason: "UAC2 pump thread exited during startup".to_owned(),
                })
            }
        }
    }

    /// Shared status snapshot for `/usb`.
    pub fn shared(&self) -> Uac2BridgeShared {
        self.shared.clone()
    }

    /// Stop the pump and flush both directions. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

impl Drop for Uac2BridgeRuntime {
    fn drop(&mut self) {
        // Do not block an async shutdown on a wedged device; the explicit
        // `stop` path is the joined one.
        self.stop.store(true, Ordering::Relaxed);
    }
}

fn run_bridge(
    config: Uac2BridgeConfig,
    devices: Uac2BridgeDeviceConfig,
    factory: Box<dyn Uac2BridgeDeviceFactory>,
    shared: Uac2BridgeShared,
    stop: Arc<AtomicBool>,
    poll_interval: Duration,
    ready: std_mpsc::SyncSender<Result<(), UsbError>>,
) {
    let inputs = match factory.open(&devices) {
        Ok(inputs) => inputs,
        Err(error) => {
            shared.set(Uac2BridgeReport::unsupported(format!(
                "{}: {error}",
                error.kind()
            )));
            let _ = ready.send(Err(error));
            return;
        }
    };

    let mut bridge = match Uac2Bridge::new(config, inputs) {
        Ok(bridge) => bridge,
        Err(error) => {
            shared.set(Uac2BridgeReport::unsupported(format!(
                "{}: {error}",
                error.kind()
            )));
            let _ = ready.send(Err(error));
            return;
        }
    };
    if let Err(error) = bridge.start() {
        shared.set(Uac2BridgeReport::unsupported(format!(
            "{}: {error}",
            error.kind()
        )));
        let _ = ready.send(Err(error));
        return;
    }

    shared.set(bridge.report());
    if ready.send(Ok(())).is_err() {
        let _ = bridge.stop();
        return;
    }
    info!(
        instance = bridge.instance(),
        "UAC2 bridge started; waiting for PCM to move"
    );

    while !stop.load(Ordering::Relaxed) {
        match bridge.pump() {
            Ok(_) => {
                shared.set(bridge.report());
            }
            Err(error) => {
                let report = bridge.report();
                shared.set_with_error(report.clone(), Uac2BridgeError::from_usb(&error));
                warn!(
                    kind = error.kind(),
                    instance = bridge.instance(),
                    counters = ?report.counters,
                    error = %error,
                    "UAC2 bridge transfer failed; counters retained"
                );
            }
        }
        thread::sleep(poll_interval);
    }

    if let Err(error) = bridge.stop() {
        shared.set_with_error(bridge.report(), Uac2BridgeError::from_usb(&error));
        warn!(kind = error.kind(), error = %error, "UAC2 bridge stop/flush failed");
    } else {
        shared.set(bridge.report());
    }
    debug!(instance = bridge.instance(), "UAC2 bridge stopped");
}

/// Resolve the kernel UAC2 gadget ALSA card and start the production bridge.
///
/// Requires `--usb-uac2`, a UAC2 function in the configured ConfigFS gadget,
/// the gadget ALSA card, and both host audio devices. If any is absent, the
/// returned error keeps `/usb` at a typed `unsupported` report.
pub fn start_real(config: &NodeConfig) -> Result<Uac2BridgeRuntime, UsbError> {
    if !config.usb_uac2 {
        return Err(UsbError::UnsupportedUac2Bridge {
            instance: "audio".to_owned(),
        });
    }
    if !uac2_function_present(config) {
        return Err(UsbError::Uac2MissingPath {
            instance: "audio".to_owned(),
            path: "gadget endpoint",
        });
    }
    let gadget_playback =
        crate::usb_audio::find_uac2_playback_device().ok_or_else(|| UsbError::Uac2MissingPath {
            instance: "audio".to_owned(),
            path: "gadget ALSA card",
        })?;
    let gadget_capture =
        crate::usb_audio::find_uac2_capture_device().ok_or_else(|| UsbError::Uac2MissingPath {
            instance: "audio".to_owned(),
            path: "gadget ALSA capture device",
        })?;

    let uac2 = Uac2Config::new(2, 1, 48_000, 16);
    let bridge = Uac2BridgeConfig::from_uac2("audio", &uac2)?
        .with_ring_capacity_bytes(UAC2_BRIDGE_RING_BYTES);
    let devices = Uac2BridgeDeviceConfig {
        instance: "audio".to_owned(),
        uac2,
        gadget_playback_device: gadget_playback,
        gadget_capture_device: gadget_capture,
        host_sink_device: config.usb_uac2_sink_device.clone(),
        host_source_device: config.usb_uac2_source_device.clone(),
    };
    Uac2BridgeRuntime::start_with(
        bridge,
        devices,
        Box::new(AlsaUac2BridgeFactory),
        UAC2_BRIDGE_POLL_INTERVAL,
        UAC2_BRIDGE_START_TIMEOUT,
    )
}

/// Whether the configured gadget contains a UAC2 function.
pub fn uac2_function_present(config: &NodeConfig) -> bool {
    let gadget = config
        .usb_configfs_root
        .join(DEFAULT_USB_GADGET_NAME)
        .join("functions");
    uac2_function_in(&gadget)
}

fn uac2_function_in(functions_dir: &Path) -> bool {
    let Ok(entries) = std::fs::read_dir(functions_dir) else {
        return false;
    };
    entries
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().starts_with("uac2."))
}

/// Production factory: ALSA gadget endpoints plus host ALSA audio.
#[derive(Clone, Copy, Debug, Default)]
pub struct AlsaUac2BridgeFactory;

impl Uac2BridgeDeviceFactory for AlsaUac2BridgeFactory {
    fn open(&self, config: &Uac2BridgeDeviceConfig) -> Result<Uac2BridgeInputs, UsbError> {
        let mut inputs = Uac2BridgeInputs::default();

        if let Some(plan) = config.bridge_plan_playback() {
            let endpoint = AlsaUac2EndpointIo::open(
                &config.gadget_capture_device,
                Direction::Capture,
                plan.format,
                plan.endpoint,
            )?;
            let host_format = host_format(plan.format);
            let sink = AlsaUac2Sink::open(AlsaUac2Config::new(
                config.host_sink_device.clone(),
                host_format,
            ))?;
            inputs.playback_endpoint = Some(Box::new(endpoint));
            inputs.playback_sink = Some(Box::new(sink));
        }

        if let Some(plan) = config.bridge_plan_capture() {
            let endpoint = AlsaUac2EndpointIo::open(
                &config.gadget_playback_device,
                Direction::Playback,
                plan.format,
                plan.endpoint,
            )?;
            let host_format = host_format(plan.format);
            let source = AlsaUac2Source::open(AlsaUac2Config::new(
                config.host_source_device.clone(),
                host_format,
            ))?;
            inputs.capture_endpoint = Some(Box::new(endpoint));
            inputs.capture_source = Some(Box::new(source));
        }

        Ok(inputs)
    }
}

impl Uac2BridgeDeviceConfig {
    /// Playback plan derived from the function model.
    pub fn bridge_plan_playback(&self) -> Option<ozma_usb::Uac2StreamPlan> {
        if !self.uac2.playback_enabled() {
            return None;
        }
        let format = Uac2PcmFormat::from_uac2(&self.uac2, self.uac2.playback_channels).ok()?;
        let endpoint =
            Uac2Endpoint::playback(self.uac2.endpoint_packet_size(self.uac2.playback_channels))
                .ok()?;
        Some(ozma_usb::Uac2StreamPlan { format, endpoint })
    }

    /// Capture plan derived from the function model.
    pub fn bridge_plan_capture(&self) -> Option<ozma_usb::Uac2StreamPlan> {
        if !self.uac2.capture_enabled() {
            return None;
        }
        let format = Uac2PcmFormat::from_uac2(&self.uac2, self.uac2.capture_channels).ok()?;
        let endpoint =
            Uac2Endpoint::capture(self.uac2.endpoint_packet_size(self.uac2.capture_channels))
                .ok()?;
        Some(ozma_usb::Uac2StreamPlan { format, endpoint })
    }
}

fn host_format(usb: Uac2PcmFormat) -> Uac2PcmFormat {
    Uac2PcmFormat {
        sample_rate: usb.sample_rate,
        channels: 2,
        sample_size: UsbSampleSize::S16,
    }
}

/// ALSA PCM wrapper that presents the kernel UAC2 gadget card as an
/// isochronous endpoint.
struct AlsaUac2EndpointIo {
    endpoint: Uac2Endpoint,
    device: String,
    frame_bytes: usize,
    pcm: PCM,
}

impl AlsaUac2EndpointIo {
    fn open(
        device: &str,
        direction: Direction,
        format: Uac2PcmFormat,
        endpoint: Uac2Endpoint,
    ) -> Result<Self, UsbError> {
        let expected_direction = match direction {
            Direction::Playback => Uac2Direction::In,
            Direction::Capture => Uac2Direction::Out,
        };
        if endpoint.direction != expected_direction {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "ALSA {:?} endpoint used for UAC2 {:?} direction",
                    direction, endpoint.direction
                ),
            });
        }
        let address = endpoint.address;
        let pcm = PCM::new(device, direction, true).map_err(|error| UsbError::Uac2Endpoint {
            address,
            operation: "open",
            reason: format!("cannot open UAC2 gadget ALSA device {device:?}: {error}"),
        })?;
        {
            let hwp = HwParams::any(&pcm).map_err(|error| UsbError::Uac2Endpoint {
                address,
                operation: "configure",
                reason: format!("UAC2 gadget {device:?} has no hardware parameters: {error}"),
            })?;
            hwp.set_channels(u32::from(format.channels))
                .map_err(|error| endpoint_param(address, device, "channels", error))?;
            hwp.set_rate(format.sample_rate, ValueOr::Nearest)
                .map_err(|error| endpoint_param(address, device, "sample rate", error))?;
            hwp.set_format(usb_alsa_format(format.sample_size))
                .map_err(|error| endpoint_param(address, device, "sample format", error))?;
            hwp.set_access(Access::RWInterleaved)
                .map_err(|error| endpoint_param(address, device, "interleaved access", error))?;
            hwp.set_period_size(1_024 as Frames, ValueOr::Nearest)
                .map_err(|error| endpoint_param(address, device, "period size", error))?;
            hwp.set_buffer_size(4_096 as Frames)
                .map_err(|error| endpoint_param(address, device, "buffer size", error))?;
            pcm.hw_params(&hwp)
                .map_err(|error| endpoint_param(address, device, "hardware parameters", error))?;
            // Start lazily on the first packet read/write. `snd-aloop` and some
            // UAC2 gadget PCMs reject starting an empty buffer with EPIPE.
            let swp = pcm
                .sw_params_current()
                .map_err(|error| endpoint_param(address, device, "software parameters", error))?;
            swp.set_start_threshold(1)
                .map_err(|error| endpoint_param(address, device, "start threshold", error))?;
            pcm.sw_params(&swp)
                .map_err(|error| endpoint_param(address, device, "software parameters", error))?;
            pcm.prepare()
                .map_err(|error| endpoint_param(address, device, "prepare", error))?;
        }
        let current = pcm
            .hw_params_current()
            .map_err(|error| UsbError::Uac2Endpoint {
                address,
                operation: "configure",
                reason: format!("cannot read negotiated UAC2 gadget parameters: {error}"),
            })?;
        let rate = current
            .get_rate()
            .map_err(|error| endpoint_param(address, device, "negotiated rate", error))?;
        let channels = current
            .get_channels()
            .map_err(|error| endpoint_param(address, device, "negotiated channels", error))?;
        let sample_format = current
            .get_format()
            .map_err(|error| endpoint_param(address, device, "negotiated format", error))?;
        if rate != format.sample_rate
            || channels != u32::from(format.channels)
            || sample_format != usb_alsa_format(format.sample_size)
        {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "UAC2 gadget {device:?} negotiated {rate} Hz / {channels} ch / {sample_format:?}, \
                     required {} Hz / {} ch / {:?}",
                    format.sample_rate,
                    format.channels,
                    usb_alsa_format(format.sample_size)
                ),
            });
        }
        drop(current);
        Ok(Self {
            endpoint,
            device: device.to_owned(),
            frame_bytes: format.frame_bytes(),
            pcm,
        })
    }
}

impl Uac2EndpointIo for AlsaUac2EndpointIo {
    fn endpoint(&self) -> Uac2Endpoint {
        self.endpoint
    }

    fn read_packet(&mut self, buffer: &mut [u8]) -> Result<usize, UsbError> {
        let capacity = buffer.len() / self.frame_bytes * self.frame_bytes;
        if capacity == 0 {
            return Ok(0);
        }
        let io = self.pcm.io_bytes();
        match io.readi(&mut buffer[..capacity]) {
            Ok(frames) => Ok(frames * self.frame_bytes),
            Err(error) if would_block(&error) => Ok(0),
            Err(error) => {
                let message = error.to_string();
                if self.pcm.try_recover(error, true).is_ok() {
                    Ok(0)
                } else {
                    Err(UsbError::Uac2Endpoint {
                        address: self.endpoint.address,
                        operation: "read",
                        reason: format!("UAC2 gadget {:?}: {message}", self.device),
                    })
                }
            }
        }
    }

    fn write_packet(&mut self, data: &[u8]) -> Result<usize, UsbError> {
        if data.len() % self.frame_bytes != 0 {
            return Err(UsbError::Uac2Format {
                reason: format!(
                    "IN packet of {} bytes is not frame-aligned ({} bytes/frame)",
                    data.len(),
                    self.frame_bytes
                ),
            });
        }
        let mut written = 0usize;
        while written < data.len() {
            let io = self.pcm.io_bytes();
            match io.writei(&data[written..]) {
                Ok(0) => break,
                Ok(frames) => written += frames * self.frame_bytes,
                Err(error) if would_block(&error) => break,
                Err(error) => {
                    let message = error.to_string();
                    if self.pcm.try_recover(error, true).is_ok() {
                        break;
                    }
                    return Err(UsbError::Uac2Endpoint {
                        address: self.endpoint.address,
                        operation: "write",
                        reason: format!("UAC2 gadget {:?}: {message}", self.device),
                    });
                }
            }
        }
        Ok(written)
    }

    fn flush(&mut self) -> Result<(), UsbError> {
        self.pcm.prepare().map_err(|error| UsbError::Uac2Endpoint {
            address: self.endpoint.address,
            operation: "flush",
            reason: format!("UAC2 gadget {:?}: {error}", self.device),
        })
    }
}

fn usb_alsa_format(size: UsbSampleSize) -> Format {
    match size {
        UsbSampleSize::S16 => Format::S16LE,
        UsbSampleSize::S24 => Format::S243LE,
        UsbSampleSize::S32 => Format::S32LE,
    }
}

fn endpoint_param(address: u8, device: &str, parameter: &str, error: alsa::Error) -> UsbError {
    UsbError::Uac2Endpoint {
        address,
        operation: "configure",
        reason: format!("UAC2 gadget {device:?} cannot provide {parameter}: {error}"),
    }
}

fn would_block(error: &alsa::Error) -> bool {
    std::io::Error::from_raw_os_error(error.errno()).kind() == std::io::ErrorKind::WouldBlock
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

fn normalize_report(mut report: Uac2BridgeReport) -> Uac2BridgeReport {
    if report.status.is_live() && !report.counters.has_data() {
        report.status = Uac2BridgeStatus::Configured;
    }
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_usb::{
        FakeUac2AudioSink, FakeUac2AudioSource, FakeUac2Endpoint, Uac2BridgeCounters,
        Uac2BridgeStatus, Uac2PcmFormat,
    };
    use std::time::{Duration, Instant};

    fn uac2() -> Uac2Config {
        Uac2Config::new(2, 2, 48_000, 16)
    }

    fn bridge_config() -> Uac2BridgeConfig {
        Uac2BridgeConfig::from_uac2("audio", &uac2())
            .unwrap()
            .with_underrun_policy(ozma_usb::Uac2UnderrunPolicy::SkipPacket)
    }

    fn devices() -> Uac2BridgeDeviceConfig {
        Uac2BridgeDeviceConfig {
            instance: "audio".to_owned(),
            uac2: uac2(),
            gadget_playback_device: "hw:9,0".to_owned(),
            gadget_capture_device: "hw:9,1".to_owned(),
            host_sink_device: "default".to_owned(),
            host_source_device: "default".to_owned(),
        }
    }

    struct FakeFactory {
        playback: Vec<Vec<u8>>,
        source: Option<Vec<u8>>,
        fail_open: bool,
        failing_endpoint: bool,
    }

    impl FakeFactory {
        fn configured() -> Self {
            Self {
                playback: Vec::new(),
                source: None,
                fail_open: false,
                failing_endpoint: false,
            }
        }
    }

    impl Uac2BridgeDeviceFactory for FakeFactory {
        fn open(&self, config: &Uac2BridgeDeviceConfig) -> Result<Uac2BridgeInputs, UsbError> {
            if self.fail_open {
                return Err(UsbError::Uac2Endpoint {
                    address: UAC2_PLAYBACK_ENDPOINT,
                    operation: "open",
                    reason: "fake device missing".to_owned(),
                });
            }
            let playback_format =
                Uac2PcmFormat::from_uac2(&config.uac2, config.uac2.playback_channels)?;
            let capture_format =
                Uac2PcmFormat::from_uac2(&config.uac2, config.uac2.capture_channels)?;
            let playback = FakeUac2Endpoint::out(1024)?;
            for packet in &self.playback {
                playback.handle().queue_host_packet(packet.clone())?;
            }
            let capture = FakeUac2Endpoint::in_endpoint(1024)?;
            let source = match &self.source {
                Some(pcm) => FakeUac2AudioSource::with_pcm(capture_format, pcm.clone())?,
                None => FakeUac2AudioSource::with_pcm(capture_format, Vec::new())?,
            };
            Ok(Uac2BridgeInputs {
                playback_endpoint: Some(if self.failing_endpoint {
                    Box::new(FailingEndpoint {
                        endpoint: Uac2Endpoint::playback(1024)?,
                    })
                } else {
                    Box::new(playback)
                }),
                playback_sink: Some(Box::new(FakeUac2AudioSink::new(playback_format))),
                capture_endpoint: Some(Box::new(capture)),
                capture_source: Some(Box::new(source)),
            })
        }
    }

    struct FailingEndpoint {
        endpoint: Uac2Endpoint,
    }

    impl Uac2EndpointIo for FailingEndpoint {
        fn endpoint(&self) -> Uac2Endpoint {
            self.endpoint
        }

        fn read_packet(&mut self, _buffer: &mut [u8]) -> Result<usize, UsbError> {
            Err(UsbError::Uac2Endpoint {
                address: self.endpoint.address,
                operation: "read",
                reason: "fake endpoint unplugged".to_owned(),
            })
        }

        fn write_packet(&mut self, _data: &[u8]) -> Result<usize, UsbError> {
            Err(UsbError::Uac2Endpoint {
                address: self.endpoint.address,
                operation: "write",
                reason: "fake endpoint unplugged".to_owned(),
            })
        }
    }

    fn start(factory: FakeFactory) -> Uac2BridgeRuntime {
        Uac2BridgeRuntime::start_with(
            bridge_config(),
            devices(),
            Box::new(factory),
            Duration::from_millis(1),
            Duration::from_secs(2),
        )
        .unwrap()
    }

    fn wait_for(shared: &Uac2BridgeShared, status: Uac2BridgeStatus) -> Uac2BridgeReport {
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let report = shared.report();
            if report.status == status {
                return report;
            }
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {:?}; last report {:?}",
                    status, report
                );
            }
            thread::sleep(Duration::from_millis(2));
        }
    }

    #[test]
    fn configured_bridge_never_claims_running_without_pcm() {
        let mut runtime = start(FakeFactory::configured());
        let report = runtime.shared().report();
        assert_eq!(report.status, Uac2BridgeStatus::Configured);
        assert!(!report.is_live());
        // The pump may have polled an idle capture source once, which records
        // empty/underrun counters; that must never count as PCM movement.
        assert!(!report.counters.has_data());
        assert_eq!(report.counters.sink_bytes, 0);
        runtime.stop();
        let stopped = runtime.shared().report();
        assert_eq!(stopped.status, Uac2BridgeStatus::Configured);
        assert!(!stopped.is_live());
    }

    #[test]
    fn running_requires_observed_pcm_and_stop_reports_stopped() {
        let format = Uac2PcmFormat::from_uac2(&uac2(), 2).unwrap();
        let packet = vec![3u8; format.frame_bytes() * 2];
        let source = vec![5u8; format.frame_bytes() * 4];
        let mut runtime = start(FakeFactory {
            playback: vec![packet],
            source: Some(source),
            fail_open: false,
            failing_endpoint: false,
        });
        let report = wait_for(&runtime.shared(), Uac2BridgeStatus::Running);
        assert!(report.counters.has_data());
        assert!(report.counters.sink_bytes > 0);
        runtime.stop();
        let stopped = runtime.shared().report();
        assert_eq!(stopped.status, Uac2BridgeStatus::Stopped);
        assert!(stopped.counters.has_data());
    }

    #[test]
    fn missing_device_is_a_typed_start_error() {
        let error = Uac2BridgeRuntime::start_with(
            bridge_config(),
            devices(),
            Box::new(FakeFactory {
                playback: Vec::new(),
                source: None,
                fail_open: true,
                failing_endpoint: false,
            }),
            Duration::from_millis(1),
            Duration::from_secs(1),
        )
        .unwrap_err();
        assert!(matches!(error, UsbError::Uac2Endpoint { .. }));
    }

    #[test]
    fn endpoint_error_is_retained_without_claiming_running() {
        let format = Uac2PcmFormat::from_uac2(&uac2(), 2).unwrap();
        let mut runtime = start(FakeFactory {
            playback: vec![vec![1u8; format.frame_bytes()]],
            source: None,
            fail_open: false,
            failing_endpoint: true,
        });
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            if let Some(error) = runtime.shared().last_error() {
                assert_eq!(error.kind, "uac2-endpoint");
                break;
            }
            if Instant::now() >= deadline {
                panic!("timed out waiting for a typed bridge error");
            }
            thread::sleep(Duration::from_millis(2));
        }
        let report = runtime.shared().report();
        assert_ne!(report.status, Uac2BridgeStatus::Running);
        assert!(!report.counters.has_data());
        runtime.stop();
    }

    #[test]
    fn injected_running_report_without_pcm_is_downgraded() {
        let report = Uac2BridgeReport {
            status: Uac2BridgeStatus::Running,
            playback: None,
            capture: None,
            counters: Uac2BridgeCounters::default(),
            missing_paths: Vec::new(),
            reason: None,
        };
        let shared = Uac2BridgeShared::from_report(report);
        assert_eq!(shared.report().status, Uac2BridgeStatus::Configured);
        assert!(!shared.report().is_live());
    }

    #[test]
    fn running_report_with_counters_is_downgraded_when_the_pump_failed() {
        let report = Uac2BridgeReport {
            status: Uac2BridgeStatus::Running,
            playback: None,
            capture: None,
            counters: Uac2BridgeCounters {
                capture_bytes_out: 96,
                capture_frames_out: 48,
                ..Uac2BridgeCounters::default()
            },
            missing_paths: Vec::new(),
            reason: None,
        };
        let shared = Uac2BridgeShared::from_report_with_error(
            report,
            Uac2BridgeError {
                kind: "uac2-endpoint".to_owned(),
                message: "usb transfer failed".to_owned(),
            },
        );
        assert_eq!(shared.report().status, Uac2BridgeStatus::Configured);
        assert!(!shared.report().is_live());
        assert_eq!(shared.last_error().unwrap().kind, "uac2-endpoint");
    }

    #[test]
    fn uac2_function_probe_rejects_a_missing_directory() {
        assert!(!uac2_function_in(Path::new(
            "/definitely-not-an-ozma-configfs-root"
        )));
    }
}
