// SPDX-License-Identifier: AGPL-3.0-only
//! Supervised background media daemon.
//!
//! [`FunctionFsMediaDaemon`] owns the FunctionFS event stream and a
//! [`MassStorageLoop`] for one virtual SCSI disk. It ties together the pieces
//! that previously existed only as separately tested building blocks:
//!
//! 1. write the descriptor/string blobs to `ep0`;
//! 2. wait (bounded) for `FUNCTIONFS_ENABLE`, `DISABLE`, `UNBIND`, and standard
//!    control requests;
//! 3. on `ENABLE`, attach a [`PollingFileBulkTransport`] and service the SCSI
//!    loop until the host stalls, the function is disabled, or shutdown;
//! 4. on success, reset the restart streak; on a transport failure or observed
//!    rebind, apply [`MediaRestartPolicy`], rebuild the device (reloading the
//!    persisted overlay when configured), and re-attach within the attempt cap;
//! 5. stop the runner and commit the overlay policy on shutdown.
//!
//! The daemon is transport- and device-injected. Default tests use in-memory
//! endpoints, fake transports, and synthetic devices; no test mounts
//! FunctionFS, opens a real UDC endpoint, or needs root. The real
//! [`FunctionFsMediaDaemon::file`] constructor opens only the explicit mount it
//! is given and still never mounts anything itself.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::error::UsbError;
use crate::functionfs::{FunctionFsConfig, FunctionFsEndpoint};
use crate::functionfs_daemon::{
    FileEndpointOpener, FunctionFsDaemonCore, FunctionFsDaemonStats, FunctionFsDescriptors,
    FunctionFsEvent, FunctionFsEventOutcome, FunctionFsEventType, FunctionFsMount, MemoryEp0,
    PollingFileEp0Io, MAX_FUNCTIONFS_ENDPOINT_TIMEOUT_MS,
};
use crate::mass_storage::MassStorageDevice;
use crate::mass_storage_loop::{
    BoxedMassStorageTransport, MassStorageLoop, MassStorageLoopConfig, MassStorageLoopStats,
    MASS_STORAGE_MAX_TIMEOUT_MS,
};
use crate::media_restart::{MediaRestartPolicy, MediaRestartTracker};

/// Default wait for one `ep0` event.
pub const MEDIA_DAEMON_DEFAULT_EVENT_TIMEOUT_MS: u64 = 100;

/// Default idle sleep when an injected event source returns immediately.
pub const MEDIA_DAEMON_DEFAULT_IDLE_POLL_MS: u64 = 20;

/// Largest accepted `ep0` event wait.
pub const MEDIA_DAEMON_MAX_EVENT_TIMEOUT_MS: u64 = MAX_FUNCTIONFS_ENDPOINT_TIMEOUT_MS;

/// Largest accepted idle poll interval.
pub const MEDIA_DAEMON_MAX_IDLE_POLL_MS: u64 = 1_000;

/// Default bound for waiting for a stopped worker to flush and exit.
///
/// The worker only observes the stop flag between bounded `ep0` waits and SCSI
/// transfer deadlines, so this must exceed the configured transfer timeout.
pub const MEDIA_DAEMON_DEFAULT_SHUTDOWN_TIMEOUT_MS: u64 = 10_000;

/// Device state exposed by `/usb/media`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum MediaDaemonState {
    /// Not running (never started or explicitly stopped).
    #[default]
    Stopped,
    /// `ep0` descriptors were written; waiting for FunctionFS `ENABLE`.
    WaitingForEnable,
    /// Enabled but not yet serving a command cycle.
    Enabled,
    /// At least one command cycle completed since the last attach.
    Running,
    /// A transport/rebind failure is being retried within the policy cap.
    Restarting,
    /// The retry cap was exhausted or the daemon failed to start.
    Failed,
}

impl MediaDaemonState {
    /// Stable lower-case name.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Stopped => "stopped",
            Self::WaitingForEnable => "waiting-for-enable",
            Self::Enabled => "enabled",
            Self::Running => "running",
            Self::Restarting => "restarting",
            Self::Failed => "failed",
        }
    }

    /// Whether the export is configured and may still serve traffic.
    pub fn is_configured(self) -> bool {
        matches!(
            self,
            Self::WaitingForEnable | Self::Enabled | Self::Running | Self::Restarting
        )
    }
}

/// Path-free function/lifecycle counters.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDaemonCounters {
    /// `ep0` events handled.
    pub events: u64,
    /// `FUNCTIONFS_BIND` events.
    pub binds: u64,
    /// `FUNCTIONFS_UNBIND` events.
    pub unbinds: u64,
    /// `FUNCTIONFS_ENABLE` events.
    pub enables: u64,
    /// `FUNCTIONFS_DISABLE` events.
    pub disables: u64,
    /// Standard control requests handled.
    pub setups: u64,
    /// Successful bulk transports attached.
    pub attaches: u64,
    /// Bulk transports detached (disable/unbind/stop/restart).
    pub detaches: u64,
    /// Supervised rebuild attempts made.
    pub restarts: u64,
}

/// Path-free error summary.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDaemonError {
    /// Stable [`UsbError::kind`] token.
    pub kind: String,
    /// Redacted, path-free explanation.
    pub message: String,
}

impl MediaDaemonError {
    /// Summarise a typed USB error without exposing backing paths.
    pub fn from_usb(error: &UsbError) -> Self {
        let message = match error {
            UsbError::MassStorageEndpointTimeout {
                operation,
                timeout_ms,
            } => format!("endpoint {operation} timed out after {timeout_ms} ms"),
            UsbError::MassStorageTransport { operation, reason } => {
                format!("{operation}: {reason}")
            }
            UsbError::MassStorageBuffer {
                context,
                limit,
                requested,
            } => format!("{context} exceeds {limit} bytes (requested {requested})"),
            UsbError::MassStorageNotAttached => "no bulk endpoint transport is attached".to_owned(),
            UsbError::ReadOnlyMedia(_) => "media is read-only".to_owned(),
            UsbError::OverlayBounds { reason } => reason.clone(),
            other => other.kind().to_owned(),
        };
        Self {
            kind: error.kind().to_owned(),
            message,
        }
    }
}

/// Counters and state published to the node status route.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDaemonStatus {
    /// Lifecycle state.
    pub state: MediaDaemonState,
    /// FunctionFS descriptors were accepted and the function is enabled.
    pub live: bool,
    /// FunctionFS `ENABLE` was observed.
    pub enabled: bool,
    /// A bulk transport is attached.
    pub attached: bool,
    /// A command cycle completed since the last attach.
    pub running: bool,
    /// SCSI loop counters (preserved across a supervised rebuild).
    pub stats: MassStorageLoopStats,
    /// FunctionFS `ep0` counters.
    pub daemon_stats: FunctionFsDaemonStats,
    /// Lifecycle counters.
    pub counters: MediaDaemonCounters,
    /// Restart attempts in the current failure streak.
    pub restart_attempts: u32,
    /// Whether the restart cap is exhausted.
    pub restart_exhausted: bool,
    /// Backoff for the next scheduled rebuild, when one is pending.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pending_backoff_ms: Option<u64>,
    /// Last typed failure.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_error: Option<MediaDaemonError>,
}

/// Bounds and policy for the background daemon.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaDaemonConfig {
    /// Per-transfer FunctionFS deadline and SCSI loop bounds.
    pub loop_config: MassStorageLoopConfig,
    /// Bounded restart policy.
    pub restart: MediaRestartPolicy,
    /// Maximum wait for one `ep0` event.
    pub event_timeout_ms: u64,
    /// Sleep after an injected event source reports no work.
    pub idle_poll_ms: u64,
}

impl Default for MediaDaemonConfig {
    fn default() -> Self {
        Self {
            loop_config: MassStorageLoopConfig::default(),
            restart: MediaRestartPolicy::default(),
            event_timeout_ms: MEDIA_DAEMON_DEFAULT_EVENT_TIMEOUT_MS,
            idle_poll_ms: MEDIA_DAEMON_DEFAULT_IDLE_POLL_MS,
        }
    }
}

impl MediaDaemonConfig {
    /// Validate every bound before an endpoint is touched.
    pub fn validate(&self) -> Result<(), UsbError> {
        self.loop_config.validate()?;
        self.restart.validate()?;
        if self.event_timeout_ms == 0 || self.event_timeout_ms > MEDIA_DAEMON_MAX_EVENT_TIMEOUT_MS {
            return Err(UsbError::InvalidField {
                field: "usb-media-daemon.event_timeout_ms",
                reason: format!(
                    "event timeout must be 1..={MEDIA_DAEMON_MAX_EVENT_TIMEOUT_MS} ms, got {}",
                    self.event_timeout_ms
                ),
            });
        }
        if self.idle_poll_ms == 0 || self.idle_poll_ms > MEDIA_DAEMON_MAX_IDLE_POLL_MS {
            return Err(UsbError::InvalidField {
                field: "usb-media-daemon.idle_poll_ms",
                reason: format!(
                    "idle poll must be 1..={MEDIA_DAEMON_MAX_IDLE_POLL_MS} ms, got {}",
                    self.idle_poll_ms
                ),
            });
        }
        if self.loop_config.transfer_timeout_ms > MASS_STORAGE_MAX_TIMEOUT_MS {
            return Err(UsbError::InvalidField {
                field: "usb-media-daemon.transfer_timeout_ms",
                reason: format!(
                    "transfer timeout must be 1..={MASS_STORAGE_MAX_TIMEOUT_MS} ms, got {}",
                    self.loop_config.transfer_timeout_ms
                ),
            });
        }
        Ok(())
    }
}

/// Builds a fresh SCSI device. `reload_overlay` asks the factory to load the
/// persisted write overlay instead of starting from the immutable base.
pub type MediaDeviceFactory = Box<dyn FnMut(bool) -> Result<MassStorageDevice, UsbError> + Send>;

/// Builds a fresh bulk transport for one `ENABLE`/restart.
pub type MediaTransportFactory =
    Box<dyn FnMut() -> Result<BoxedMassStorageTransport, UsbError> + Send>;

/// Shared handle used by the node to query a daemon running on its own thread.
#[derive(Clone)]
pub struct MediaDaemonHandle {
    status: Arc<Mutex<MediaDaemonStatus>>,
    stop: Arc<AtomicBool>,
    join: Arc<Mutex<Option<std::thread::JoinHandle<()>>>>,
    done: Arc<(Mutex<bool>, Condvar)>,
}

impl std::fmt::Debug for MediaDaemonHandle {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MediaDaemonHandle")
            .field("status", &self.status.lock().ok())
            .finish_non_exhaustive()
    }
}

impl MediaDaemonHandle {
    /// Current published status.
    pub fn status(&self) -> MediaDaemonStatus {
        self.status
            .lock()
            .map(|status| status.clone())
            .unwrap_or_default()
    }

    /// Ask the worker to stop and flush.
    pub fn request_stop(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Whether a stop was requested.
    pub fn stop_requested(&self) -> bool {
        self.stop.load(Ordering::SeqCst)
    }

    /// Request stop and wait for the worker to flush and exit.
    ///
    /// The wait is bounded by [`MEDIA_DAEMON_DEFAULT_SHUTDOWN_TIMEOUT_MS`] so a
    /// stalled transport cannot hold node shutdown forever. No UDC unbind is
    /// required to make the worker leave: callers should unbind the gadget
    /// before this to quiesce host traffic, then this flushes and closes the
    /// endpoints.
    pub fn shutdown(&self) {
        let _ = self.shutdown_timeout(Duration::from_millis(
            MEDIA_DAEMON_DEFAULT_SHUTDOWN_TIMEOUT_MS,
        ));
    }

    /// Bounded variant of [`Self::shutdown`].
    ///
    /// Returns `true` when the worker finished within `timeout`; `false` means
    /// the worker is still winding down (for example a transport transfer hit
    /// its own deadline) and the caller should not wait indefinitely. The stop
    /// flag remains set, so a later call can reap it.
    pub fn shutdown_timeout(&self, timeout: Duration) -> bool {
        self.request_stop();
        let (done, ready) = &*self.done;
        let guard = match done.lock() {
            Ok(guard) => guard,
            Err(poisoned) => poisoned.into_inner(),
        };
        let (guard, _) = match ready.wait_timeout_while(guard, timeout, |done| !*done) {
            Ok(result) => result,
            Err(poisoned) => poisoned.into_inner(),
        };
        let finished = *guard;
        drop(guard);
        if finished {
            let handle = self.join.lock().ok().and_then(|mut join| join.take());
            if let Some(handle) = handle {
                let _ = handle.join();
            }
        }
        finished
    }
}

/// One running background media daemon.
pub struct FunctionFsMediaDaemon {
    config: FunctionFsConfig,
    core: FunctionFsDaemonCore,
    device_factory: MediaDeviceFactory,
    transport_factory: MediaTransportFactory,
    daemon_config: MediaDaemonConfig,
    runner: Option<MassStorageLoop<BoxedMassStorageTransport>>,
    stats: MassStorageLoopStats,
    restart: MediaRestartTracker,
    pending_restart: Option<PendingRestart>,
    reload_next_attach: bool,
    status: MediaDaemonStatus,
    published: Option<Arc<Mutex<MediaDaemonStatus>>>,
}

#[derive(Clone, Copy, Debug)]
struct PendingRestart {
    attempt: u32,
    backoff_ms: u64,
    due: Instant,
}

impl std::fmt::Debug for FunctionFsMediaDaemon {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("FunctionFsMediaDaemon")
            .field("instance", &self.config.instance)
            .field("state", &self.status.state)
            .field("attached", &self.status.attached)
            .field("stats", &self.stats)
            .field("restart", &self.restart)
            .finish_non_exhaustive()
    }
}

impl FunctionFsMediaDaemon {
    /// Build a daemon over an injected core, device factory, and transport
    /// factory.
    pub fn new(
        config: FunctionFsConfig,
        core: FunctionFsDaemonCore,
        device_factory: MediaDeviceFactory,
        transport_factory: MediaTransportFactory,
        daemon_config: MediaDaemonConfig,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        daemon_config.validate()?;
        core.descriptors().validate()?;
        Ok(Self {
            config,
            core,
            device_factory,
            transport_factory,
            daemon_config,
            runner: None,
            stats: MassStorageLoopStats::default(),
            restart: MediaRestartTracker::new(),
            pending_restart: None,
            reload_next_attach: false,
            status: MediaDaemonStatus {
                state: MediaDaemonState::Stopped,
                ..MediaDaemonStatus::default()
            },
            published: None,
        })
    }

    /// Build the real file-backed daemon for an explicitly supplied mount.
    ///
    /// The caller mounts FunctionFS and creates the ConfigFS function/link.
    /// This constructor only opens `ep0` (nonblocking) and `epN` files, writes
    /// the descriptor blobs, and waits for the kernel's events.
    pub fn file(
        config: FunctionFsConfig,
        mount: FunctionFsMount,
        device_factory: MediaDeviceFactory,
        transport_factory: MediaTransportFactory,
        daemon_config: MediaDaemonConfig,
    ) -> Result<Self, UsbError> {
        config.validate()?;
        mount.validate()?;
        daemon_config.validate()?;
        let descriptors = config.ep0_descriptors()?;
        let ep0 = PollingFileEp0Io::open(mount.ep0_path(), daemon_config.event_timeout_ms)?;
        let core = FunctionFsDaemonCore::new(
            config.clone(),
            Some(mount.clone()),
            descriptors,
            Box::new(ep0),
            Box::new(FileEndpointOpener::new(mount)),
        )?
        .with_external_endpoints();
        Self::new(
            config,
            core,
            device_factory,
            transport_factory,
            daemon_config,
        )
    }

    /// FunctionFS descriptor/event model.
    pub fn config(&self) -> &FunctionFsConfig {
        &self.config
    }

    /// Current status snapshot.
    pub fn status(&self) -> MediaDaemonStatus {
        let mut status = self.status.clone();
        status.stats = self.stats.clone();
        status.daemon_stats = self.core.stats().clone();
        status.live = self.core.is_live();
        status.enabled = self.core.is_enabled();
        status.attached = self.runner.is_some();
        status.restart_attempts = self.restart.attempts();
        status.restart_exhausted = self.restart.is_exhausted();
        status.pending_backoff_ms = self.pending_restart.map(|pending| pending.backoff_ms);
        status
    }

    /// Queue one `ep0` event for an in-memory core (tests/dry runs).
    pub fn push_event(&mut self, event: FunctionFsEvent) -> Result<(), UsbError> {
        let ep0 = self
            .core
            .ep0_mut()
            .as_any_mut()
            .downcast_mut::<MemoryEp0>()
            .ok_or_else(|| UsbError::InvalidFunctionFsState {
                reason: "ep0 is not an in-memory event source".to_owned(),
            })?;
        ep0.push_event(event);
        Ok(())
    }

    /// Write descriptors if the core has not done so yet.
    pub fn write_descriptors(&mut self) -> Result<(), UsbError> {
        if self.core.descriptors_written() {
            return Ok(());
        }
        let descriptors = self.config.ep0_descriptors()?;
        self.core.write_descriptors(&self.config, &descriptors)?;
        self.status.state = MediaDaemonState::WaitingForEnable;
        Ok(())
    }

    /// Run until `stop` is set, then stop and flush.
    ///
    /// The loop is never busy: a real `ep0` read waits for the configured event
    /// deadline, and an injected source that returns immediately triggers the
    /// bounded idle poll sleep. Transport operations use the SCSI loop's
    /// polling deadline, so a stalled host becomes a typed timeout.
    pub fn run(&mut self, stop: &AtomicBool) -> Result<(), UsbError> {
        self.write_descriptors()?;
        while !stop.load(Ordering::SeqCst) {
            match self.tick() {
                Ok(true) => continue,
                Ok(false) => {
                    let sleep_ms = self.sleep_before_next_tick();
                    if sleep_ms > 0 {
                        std::thread::sleep(Duration::from_millis(sleep_ms));
                    }
                }
                Err(_error) => {
                    // The typed error is already in `last_error`; a stop request
                    // is not itself an error, so keep supervising until asked to
                    // shut down.
                }
            }
        }
        self.shutdown()?;
        Ok(())
    }

    /// Start a background worker and return its shared handle.
    pub fn spawn(mut self, thread_name: impl Into<String>) -> Result<MediaDaemonHandle, UsbError> {
        let status = Arc::new(Mutex::new(self.status()));
        self.published = Some(Arc::clone(&status));
        let stop = Arc::new(AtomicBool::new(false));
        let worker_status = Arc::clone(&status);
        let worker_stop = Arc::clone(&stop);
        let done = Arc::new((Mutex::new(false), Condvar::new()));
        let worker_done = Arc::clone(&done);
        let join = std::thread::Builder::new()
            .name(thread_name.into())
            .spawn(move || {
                if let Err(error) = self.run(&worker_stop) {
                    self.status.state = MediaDaemonState::Failed;
                    self.status.last_error = Some(MediaDaemonError::from_usb(&error));
                }
                self.publish();
                if let Ok(mut published) = worker_status.lock() {
                    *published = self.status.clone();
                }
                let (finished, ready) = &*worker_done;
                if let Ok(mut finished) = finished.lock() {
                    *finished = true;
                }
                ready.notify_all();
            })
            .map_err(|source| UsbError::Spawn {
                command: "usb-media-daemon".to_owned(),
                source,
            })?;
        Ok(MediaDaemonHandle {
            status,
            stop,
            join: Arc::new(Mutex::new(Some(join))),
            done,
        })
    }

    /// Process at most one event or one SCSI command cycle.
    ///
    /// Returns `true` when real work was done. A pending restart that is not
    /// due yet returns `false` and the caller should poll later.
    pub fn tick(&mut self) -> Result<bool, UsbError> {
        if let Some(pending) = self.pending_restart {
            if Instant::now() < pending.due {
                self.publish();
                return Ok(false);
            }
            self.pending_restart = None;
            self.attempt_restart(pending.attempt)?;
            self.publish();
            return Ok(true);
        }

        if let Some(outcome) = self
            .core
            .handle_next_event_timeout(&self.config, self.daemon_config.event_timeout_ms)?
        {
            self.record_event(&outcome);
            self.after_event(&outcome)?;
            self.publish();
            return Ok(true);
        }

        if !self.core.is_enabled() {
            self.status.enabled = false;
            self.status.live = self.core.is_live();
            if self.status.state != MediaDaemonState::Failed {
                self.status.state = if self.status.state == MediaDaemonState::Stopped {
                    MediaDaemonState::Stopped
                } else {
                    MediaDaemonState::WaitingForEnable
                };
            }
            self.publish();
            return Ok(false);
        }

        self.status.enabled = true;
        self.status.live = self.core.is_live();
        if self.runner.is_none() {
            self.attach_runner(false)?;
            self.publish();
            return Ok(true);
        }

        let before = self.stats.clone();
        let result = {
            let runner = self
                .runner
                .as_mut()
                .ok_or(UsbError::MassStorageNotAttached)?;
            runner.step()
        };
        if let Some(runner) = self.runner.as_ref() {
            self.stats = runner.stats().clone();
        }

        match result {
            Ok(outcome) => {
                self.status.running = true;
                self.status.state = MediaDaemonState::Running;
                self.status.last_error = None;
                self.restart.record_success();
                self.pending_restart = None;
                self.publish();
                let _ = outcome;
                Ok(true)
            }
            Err(error) if is_idle_timeout(&error, &before, &self.stats) => {
                // Waiting for the next CBW is normal idleness, not a failure.
                // The loop remains attached and keeps the same deadline.
                self.status.state = if self.status.running {
                    MediaDaemonState::Running
                } else {
                    MediaDaemonState::Enabled
                };
                self.publish();
                Ok(false)
            }
            Err(error) => {
                self.status.state = MediaDaemonState::Failed;
                self.status.running = false;
                self.status.last_error = Some(MediaDaemonError::from_usb(&error));
                let _ = self.detach_runner(true);
                self.schedule_restart();
                self.publish();
                Err(error)
            }
        }
    }

    /// Stop the runner, apply the persistence policy, and close endpoints.
    pub fn shutdown(&mut self) -> Result<(), UsbError> {
        self.pending_restart = None;
        let result = self.detach_runner(true);
        self.core.shutdown();
        self.status.state = MediaDaemonState::Stopped;
        self.status.enabled = false;
        self.status.live = false;
        self.status.running = false;
        self.status.attached = false;
        self.status.pending_backoff_ms = None;
        self.publish();
        result
    }

    fn publish(&mut self) {
        self.status.stats = self.stats.clone();
        self.status.daemon_stats = self.core.stats().clone();
        self.status.live = self.core.is_live();
        self.status.enabled = self.core.is_enabled();
        self.status.attached = self.runner.is_some();
        self.status.restart_attempts = self.restart.attempts();
        self.status.restart_exhausted = self.restart.is_exhausted();
        self.status.pending_backoff_ms = self.pending_restart.map(|pending| pending.backoff_ms);
        if let Some(published) = self.published.as_ref() {
            if let Ok(mut published) = published.lock() {
                *published = self.status.clone();
            }
        }
    }

    fn record_event(&mut self, outcome: &FunctionFsEventOutcome) {
        self.status.counters.events = self.status.counters.events.saturating_add(1);
        match outcome.event {
            FunctionFsEventType::Bind => {
                self.status.counters.binds = self.status.counters.binds.saturating_add(1);
            }
            FunctionFsEventType::Unbind => {
                self.status.counters.unbinds = self.status.counters.unbinds.saturating_add(1);
            }
            FunctionFsEventType::Enable => {
                self.status.counters.enables = self.status.counters.enables.saturating_add(1);
            }
            FunctionFsEventType::Disable => {
                self.status.counters.disables = self.status.counters.disables.saturating_add(1);
            }
            FunctionFsEventType::Setup => {
                self.status.counters.setups = self.status.counters.setups.saturating_add(1);
            }
            FunctionFsEventType::Suspend | FunctionFsEventType::Resume => {}
        }
    }

    fn after_event(&mut self, outcome: &FunctionFsEventOutcome) -> Result<(), UsbError> {
        match outcome.event {
            FunctionFsEventType::Enable => {
                self.status.state = MediaDaemonState::Enabled;
                self.status.enabled = true;
                self.status.live = self.core.is_live();
                if self.runner.is_none() {
                    let reload = self.reload_next_attach;
                    self.reload_next_attach = false;
                    self.attach_runner(reload)?;
                }
            }
            FunctionFsEventType::Disable => {
                let _ = self.detach_runner(true);
                self.status.state = MediaDaemonState::WaitingForEnable;
                self.status.enabled = false;
                self.status.live = self.core.is_live();
                self.status.running = false;
            }
            FunctionFsEventType::Unbind => {
                let _ = self.detach_runner(true);
                self.status.state = MediaDaemonState::Restarting;
                self.status.enabled = false;
                self.status.live = false;
                self.status.running = false;
                self.reload_next_attach = self.daemon_config.restart.reload_overlay;
                self.schedule_restart();
            }
            FunctionFsEventType::Bind => {
                self.status.state = MediaDaemonState::WaitingForEnable;
            }
            FunctionFsEventType::Setup
            | FunctionFsEventType::Suspend
            | FunctionFsEventType::Resume => {}
        }
        Ok(())
    }

    fn attach_runner(&mut self, reload_overlay: bool) -> Result<(), UsbError> {
        let device = (self.device_factory)(reload_overlay)?;
        let transport = (self.transport_factory)()?;
        let mut runner = MassStorageLoop::new(device, transport, self.daemon_config.loop_config)?;
        runner.restore_stats(self.stats.clone());
        self.runner = Some(runner);
        self.status.attached = true;
        self.status.state = MediaDaemonState::Enabled;
        self.status.counters.attaches = self.status.counters.attaches.saturating_add(1);
        Ok(())
    }

    fn detach_runner(&mut self, apply_policy: bool) -> Result<(), UsbError> {
        let Some(mut runner) = self.runner.take() else {
            return Ok(());
        };
        let result = if apply_policy { runner.stop() } else { Ok(()) };
        self.stats = runner.stats().clone();
        self.status.attached = false;
        self.status.running = false;
        self.status.counters.detaches = self.status.counters.detaches.saturating_add(1);
        result
    }

    fn schedule_restart(&mut self) {
        let policy = self.daemon_config.restart;
        match self.restart.record_failure(&policy) {
            Some(attempt) => {
                self.pending_restart = Some(PendingRestart {
                    attempt: attempt.attempt,
                    backoff_ms: attempt.backoff_ms,
                    due: Instant::now() + Duration::from_millis(attempt.backoff_ms),
                });
                self.status.state = MediaDaemonState::Restarting;
                self.status.pending_backoff_ms = Some(attempt.backoff_ms);
            }
            None => {
                self.pending_restart = None;
                self.status.state = MediaDaemonState::Failed;
                self.status.pending_backoff_ms = None;
            }
        }
    }

    fn attempt_restart(&mut self, _attempt: u32) -> Result<(), UsbError> {
        self.status.counters.restarts = self.status.counters.restarts.saturating_add(1);
        let reload = self.daemon_config.restart.reload_overlay;
        if !self.core.is_enabled() {
            // FunctionFS is unbound/disabled. The rebuilt device is opened on
            // the next ENABLE; do not touch endpoint files that do not exist.
            match (self.device_factory)(reload) {
                Ok(_device) => {
                    self.reload_next_attach = reload;
                    self.status.state = MediaDaemonState::WaitingForEnable;
                    self.status.last_error = None;
                    self.status.pending_backoff_ms = None;
                    Ok(())
                }
                Err(error) => {
                    self.status.last_error = Some(MediaDaemonError::from_usb(&error));
                    self.status.state = MediaDaemonState::Failed;
                    self.schedule_restart();
                    Err(error)
                }
            }
        } else {
            match self.attach_runner(reload) {
                Ok(()) => {
                    self.status.state = MediaDaemonState::Enabled;
                    self.status.last_error = None;
                    self.status.pending_backoff_ms = None;
                    Ok(())
                }
                Err(error) => {
                    self.status.last_error = Some(MediaDaemonError::from_usb(&error));
                    self.status.state = MediaDaemonState::Failed;
                    self.schedule_restart();
                    Err(error)
                }
            }
        }
    }

    fn sleep_before_next_tick(&self) -> u64 {
        if let Some(pending) = self.pending_restart {
            let remaining = pending.due.saturating_duration_since(Instant::now());
            return remaining
                .as_millis()
                .min(self.daemon_config.idle_poll_ms as u128) as u64;
        }
        self.daemon_config.idle_poll_ms
    }
}

fn is_idle_timeout(
    error: &UsbError,
    before: &MassStorageLoopStats,
    after: &MassStorageLoopStats,
) -> bool {
    matches!(error, UsbError::MassStorageEndpointTimeout { .. }) && before.cbws == after.cbws
}

/// Select the first non-zero bulk OUT and bulk IN endpoints in a FunctionFS
/// configuration. Media daemons require exactly one of each.
pub fn media_bulk_endpoints(
    config: &FunctionFsConfig,
) -> Result<(FunctionFsEndpoint, FunctionFsEndpoint), UsbError> {
    let mut out = None;
    let mut input = None;
    for endpoint in &config.endpoints {
        if endpoint.address & 0x0f == 0 {
            continue;
        }
        if endpoint.is_in() {
            if input.is_none() && endpoint.transfer == crate::functionfs::FunctionFsTransfer::Bulk {
                input = Some(endpoint.clone());
            }
        } else if out.is_none() && endpoint.transfer == crate::functionfs::FunctionFsTransfer::Bulk
        {
            out = Some(endpoint.clone());
        }
    }
    match (out, input) {
        (Some(out), Some(input)) => Ok((out, input)),
        _ => Err(UsbError::InvalidFunctionFsState {
            reason: "the media daemon requires one bulk OUT and one bulk IN endpoint".to_owned(),
        }),
    }
}

/// Build the standard polling endpoint factory for one mount.
pub fn polling_transport_factory(
    mount: FunctionFsMount,
    config: &FunctionFsConfig,
    timeout_ms: u64,
) -> Result<MediaTransportFactory, UsbError> {
    let (out, input) = media_bulk_endpoints(config)?;
    let endpoint_index = |target: &FunctionFsEndpoint| {
        config
            .endpoints
            .iter()
            .position(|endpoint| endpoint.address == target.address)
            .map(|index| index as u8 + 1)
            .ok_or_else(|| UsbError::InvalidFunctionFsState {
                reason: format!(
                    "endpoint 0x{:02x} is not declared by FunctionFS {}",
                    target.address, config.instance
                ),
            })
    };
    let out_path = mount.endpoint_path(endpoint_index(&out)?)?;
    let in_path = mount.endpoint_path(endpoint_index(&input)?)?;
    Ok(Box::new(move || {
        Ok(
            Box::new(crate::mass_storage_loop::PollingFileBulkTransport::open(
                out.clone(),
                out_path.clone(),
                input.clone(),
                in_path.clone(),
                timeout_ms,
            )?) as BoxedMassStorageTransport,
        )
    }))
}

/// Descriptors for one daemon, exposed for callers that build the core
/// themselves.
pub fn media_descriptors(config: &FunctionFsConfig) -> Result<FunctionFsDescriptors, UsbError> {
    config.ep0_descriptors()
}

/// Default event source for a file-backed daemon, exposed so an embedder can
/// assemble a core with [`FunctionFsDaemonCore::with_external_endpoints`].
pub fn polling_ep0(mount: &FunctionFsMount, timeout_ms: u64) -> Result<PollingFileEp0Io, UsbError> {
    PollingFileEp0Io::open(mount.ep0_path(), timeout_ms)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::fs;

    use super::*;
    use crate::functionfs::FunctionFsTransfer;
    use crate::functionfs_daemon::{FunctionFsEvent, LoopbackEndpointOpener, MemoryEp0};
    use crate::mass_storage::{
        MassStoragePersistPolicy, SCSI_CBW_BYTES, SCSI_CSW_BYTES, SCSI_SECTOR_BYTES,
    };
    use crate::mass_storage_loop::MassStorageBulkTransport;
    use crate::overlay::OverlayConfig;

    #[derive(Clone, Default)]
    struct SharedTransport {
        state: Arc<Mutex<FakeState>>,
    }

    #[derive(Default)]
    struct FakeState {
        host: VecDeque<u8>,
        device: Vec<u8>,
        flushes: u64,
        read_timeouts: u64,
    }

    impl SharedTransport {
        fn push(&self, data: &[u8]) {
            self.state.lock().unwrap().host.extend(data);
        }

        fn take(&self) -> Vec<u8> {
            std::mem::take(&mut self.state.lock().unwrap().device)
        }

        fn flushes(&self) -> u64 {
            self.state.lock().unwrap().flushes
        }
    }

    impl MassStorageBulkTransport for SharedTransport {
        fn read_out(&mut self, buffer: &mut [u8], timeout_ms: u64) -> Result<usize, UsbError> {
            let mut state = self.state.lock().unwrap();
            if state.host.is_empty() {
                state.read_timeouts += 1;
                return Err(UsbError::MassStorageEndpointTimeout {
                    operation: "read",
                    timeout_ms,
                });
            }
            let count = buffer.len().min(state.host.len());
            for slot in &mut buffer[..count] {
                *slot = state.host.pop_front().unwrap();
            }
            Ok(count)
        }

        fn write_in(&mut self, data: &[u8], _timeout_ms: u64) -> Result<usize, UsbError> {
            self.state.lock().unwrap().device.extend(data);
            Ok(data.len())
        }

        fn flush_in(&mut self, _timeout_ms: u64) -> Result<(), UsbError> {
            self.state.lock().unwrap().flushes += 1;
            Ok(())
        }
    }

    fn sample_base(sectors: usize) -> Vec<u8> {
        (0..sectors * SCSI_SECTOR_BYTES)
            .map(|index| (index as u8).wrapping_mul(13))
            .collect()
    }

    fn cbw(opcode: u8, cdb: &[u8], transfer_length: u32, direction_in: bool) -> Vec<u8> {
        let mut bytes = vec![0u8; SCSI_CBW_BYTES];
        bytes[0..4].copy_from_slice(&0x4342_5355u32.to_le_bytes());
        bytes[4..8].copy_from_slice(&0x1234_5678u32.to_le_bytes());
        bytes[8..12].copy_from_slice(&transfer_length.to_le_bytes());
        bytes[12] = if direction_in { 0x80 } else { 0x00 };
        bytes[13] = 0;
        bytes[14] = cdb.len() as u8;
        bytes[15..15 + cdb.len()].copy_from_slice(cdb);
        bytes[15] = opcode;
        bytes
    }

    fn test_unit_ready() -> Vec<u8> {
        cbw(0x00, &[0u8; 6], 0, false)
    }

    fn write10(lba: u32, count: u16, data: &[u8]) -> Vec<u8> {
        let mut cdb = vec![0u8; 10];
        cdb[0] = 0x2a;
        cdb[2..6].copy_from_slice(&lba.to_be_bytes());
        cdb[7..9].copy_from_slice(&count.to_be_bytes());
        cbw(0x2a, &cdb, data.len() as u32, false)
    }

    fn vendor_config() -> FunctionFsConfig {
        FunctionFsConfig::new("vendor0", 0x1209, 0x0001)
            .with_interface(0xff, 0x00, 0x00)
            .with_endpoint(FunctionFsEndpoint::bulk_out(1, 512))
            .with_endpoint(FunctionFsEndpoint::bulk_in(2, 512))
    }

    fn daemon_with(
        device_factory: MediaDeviceFactory,
        transport_factory: MediaTransportFactory,
        restart: MediaRestartPolicy,
    ) -> FunctionFsMediaDaemon {
        let config = vendor_config();
        let descriptors = config.ep0_descriptors().unwrap();
        let core = FunctionFsDaemonCore::new(
            config.clone(),
            None,
            descriptors,
            Box::new(MemoryEp0::new()),
            Box::new(LoopbackEndpointOpener),
        )
        .unwrap()
        .with_external_endpoints();
        FunctionFsMediaDaemon::new(
            config,
            core,
            device_factory,
            transport_factory,
            MediaDaemonConfig {
                loop_config: MassStorageLoopConfig {
                    transfer_timeout_ms: 20,
                    ..MassStorageLoopConfig::default()
                },
                restart,
                event_timeout_ms: 1,
                idle_poll_ms: 1,
            },
        )
        .unwrap()
    }

    fn read_only_daemon(
        transport: SharedTransport,
        restart: MediaRestartPolicy,
    ) -> FunctionFsMediaDaemon {
        let base = sample_base(8);
        let device_factory =
            Box::new(move |_reload: bool| MassStorageDevice::read_only_bytes(base.clone(), 4096));
        let shared = transport.clone();
        let transport_factory =
            Box::new(move || Ok(Box::new(shared.clone()) as BoxedMassStorageTransport));
        daemon_with(device_factory, transport_factory, restart)
    }

    #[test]
    fn enable_attach_serve_disable_detach_lifecycle() {
        let transport = SharedTransport::default();
        let mut daemon = read_only_daemon(transport.clone(), MediaRestartPolicy::disabled());
        daemon.write_descriptors().unwrap();

        daemon.push_event(FunctionFsEvent::Enable).unwrap();
        assert!(daemon.tick().unwrap());
        let status = daemon.status();
        assert_eq!(status.state, MediaDaemonState::Enabled);
        assert!(status.attached);
        assert!(!status.running);
        assert_eq!(status.counters.enables, 1);

        // An idle tick (no host CBW) must not claim a transfer or restart.
        assert!(!daemon.tick().unwrap());
        assert!(!daemon.status().running);

        transport.push(&test_unit_ready());
        assert!(daemon.tick().unwrap());
        let status = daemon.status();
        assert!(status.running);
        assert_eq!(status.state, MediaDaemonState::Running);
        assert_eq!(status.stats.commands, 1);
        assert_eq!(status.stats.csws, 1);
        let device_bytes = transport.take();
        assert_eq!(device_bytes.len(), SCSI_CSW_BYTES);
        assert_eq!(&device_bytes[0..4], &0x5342_5355u32.to_le_bytes());

        daemon.push_event(FunctionFsEvent::Disable).unwrap();
        assert!(daemon.tick().unwrap());
        let status = daemon.status();
        assert_eq!(status.state, MediaDaemonState::WaitingForEnable);
        assert!(!status.attached);
        assert!(!status.running);
        assert_eq!(status.counters.disables, 1);
        assert!(transport.flushes() > 0);
    }

    #[test]
    fn timeout_mid_command_triggers_a_bounded_restart_then_recovers() {
        let transport = SharedTransport::default();
        let mut daemon = read_only_daemon(transport.clone(), MediaRestartPolicy::new(2, 0, 0));
        daemon.write_descriptors().unwrap();
        daemon.push_event(FunctionFsEvent::Enable).unwrap();
        daemon.tick().unwrap();

        // WRITE(10) declares a data phase. Provide only the CBW so the loop
        // advances cbws and then stalls reading the OUT data.
        transport.push(&write10(0, 1, &[0u8; SCSI_SECTOR_BYTES]));
        assert!(daemon.tick().is_err());
        let status = daemon.status();
        assert_eq!(status.state, MediaDaemonState::Restarting);
        assert_eq!(status.restart_attempts, 1);
        assert!(!status.attached);
        assert_eq!(
            status.last_error.as_ref().unwrap().kind,
            "mass-storage-endpoint-timeout"
        );

        // The zero-backoff attempt rebuilds and re-attaches.
        assert!(daemon.tick().unwrap());
        let status = daemon.status();
        assert_eq!(status.state, MediaDaemonState::Enabled);
        assert!(status.attached);

        transport.push(&test_unit_ready());
        assert!(daemon.tick().unwrap());
        let status = daemon.status();
        assert!(status.running);
        assert_eq!(status.restart_attempts, 0);
    }

    #[test]
    fn unbind_reloads_the_overlay_and_waits_for_the_next_enable() {
        let reloads = Arc::new(Mutex::new(Vec::new()));
        let base = sample_base(4);
        let seen = Arc::clone(&reloads);
        let device_factory = Box::new(move |reload: bool| {
            seen.lock().unwrap().push(reload);
            MassStorageDevice::read_only_bytes(base.clone(), 4096)
        });
        let transport = SharedTransport::default();
        let shared = transport.clone();
        let transport_factory =
            Box::new(move || Ok(Box::new(shared.clone()) as BoxedMassStorageTransport));
        let mut daemon = daemon_with(
            device_factory,
            transport_factory,
            MediaRestartPolicy::new(2, 0, 0).with_overlay_reload(true),
        );
        daemon.write_descriptors().unwrap();
        daemon.push_event(FunctionFsEvent::Enable).unwrap();
        daemon.tick().unwrap();
        assert_eq!(reloads.lock().unwrap().as_slice(), &[false]);

        daemon.push_event(FunctionFsEvent::Unbind).unwrap();
        daemon.tick().unwrap();
        let status = daemon.status();
        assert_eq!(status.state, MediaDaemonState::Restarting);
        assert!(!status.attached);
        assert_eq!(status.counters.unbinds, 1);

        daemon.tick().unwrap();
        assert_eq!(reloads.lock().unwrap().as_slice(), &[false, true]);
        let status = daemon.status();
        assert_eq!(status.state, MediaDaemonState::WaitingForEnable);
        assert!(!status.attached);

        daemon.push_event(FunctionFsEvent::Enable).unwrap();
        daemon.tick().unwrap();
        assert_eq!(reloads.lock().unwrap().as_slice(), &[false, true, true]);
        assert!(daemon.status().attached);
    }

    #[test]
    fn shutdown_stops_the_runner_and_flushes_the_overlay() {
        let temp = tempfile::tempdir().unwrap();
        let base = temp.path().join("base.img");
        let persist = temp.path().join("overlay.bin");
        let contents = sample_base(4);
        fs::write(&base, &contents).unwrap();

        let base_for_factory = base.clone();
        let persist_for_factory = persist.clone();
        let device_factory = Box::new(move |reload: bool| {
            let config = OverlayConfig::default().with_persistence(&persist_for_factory);
            let policy = MassStoragePersistPolicy {
                persist_on_flush: true,
                persist_on_stop: true,
            };
            if reload && persist_for_factory.exists() {
                MassStorageDevice::load_writable_file(
                    &base_for_factory,
                    config,
                    policy,
                    &persist_for_factory,
                )
            } else {
                MassStorageDevice::writable_file(&base_for_factory, config, policy)
            }
        });
        let transport = SharedTransport::default();
        let shared = transport.clone();
        let transport_factory =
            Box::new(move || Ok(Box::new(shared.clone()) as BoxedMassStorageTransport));
        let mut daemon = daemon_with(
            device_factory,
            transport_factory,
            MediaRestartPolicy::disabled(),
        );
        daemon.write_descriptors().unwrap();
        daemon.push_event(FunctionFsEvent::Enable).unwrap();
        daemon.tick().unwrap();

        let payload: Vec<u8> = (0..SCSI_SECTOR_BYTES).map(|index| index as u8).collect();
        transport.push(&write10(0, 1, &payload));
        transport.push(&payload);
        daemon.tick().unwrap();
        assert!(daemon.status().running);

        daemon.shutdown().unwrap();
        assert_eq!(daemon.status().state, MediaDaemonState::Stopped);
        assert!(persist.exists(), "shutdown must commit the overlay");
        assert_eq!(fs::read(&base).unwrap(), contents, "base stays immutable");
        assert!(transport.flushes() > 0);
    }

    #[test]
    fn spawned_shutdown_is_bounded_and_needs_no_external_unbind() {
        let transport = SharedTransport::default();
        let mut daemon = read_only_daemon(transport, MediaRestartPolicy::disabled());
        daemon.write_descriptors().unwrap();
        daemon.push_event(FunctionFsEvent::Enable).unwrap();
        let handle = daemon.spawn("bounded-shutdown-test").unwrap();

        // Wait until the worker attached its endpoint transport; the UDC is
        // still "bound" because this test never sends an UNBIND event.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !handle.status().enabled && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(handle.status().enabled);

        let started = Instant::now();
        assert!(
            handle.shutdown_timeout(Duration::from_secs(2)),
            "the worker exits within its bound without a second UDC unbind"
        );
        assert!(started.elapsed() < Duration::from_secs(2));
        let status = handle.status();
        assert_eq!(status.state, MediaDaemonState::Stopped);
        assert_eq!(status.counters.unbinds, 0);
    }

    #[test]
    fn invalid_bounds_are_typed() {
        let mut config = MediaDaemonConfig::default();
        config.event_timeout_ms = 0;
        assert_eq!(config.validate().unwrap_err().kind(), "invalid-config");
        config.event_timeout_ms = 1;
        config.idle_poll_ms = 0;
        assert_eq!(config.validate().unwrap_err().kind(), "invalid-config");
    }

    #[test]
    fn bulk_endpoint_selection_requires_one_in_and_one_out() {
        let config = vendor_config();
        let (out, input) = media_bulk_endpoints(&config).unwrap();
        assert_eq!(out.address, 0x01);
        assert_eq!(input.address, 0x82);
        assert_eq!(out.transfer, FunctionFsTransfer::Bulk);

        let missing =
            FunctionFsConfig::new("bad", 1, 2).with_endpoint(FunctionFsEndpoint::bulk_out(1, 512));
        let error = media_bulk_endpoints(&missing).unwrap_err();
        assert_eq!(error.kind(), "invalid-functionfs-state");
    }
}
