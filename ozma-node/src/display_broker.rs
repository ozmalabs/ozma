// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded multi-console snapshot broker.
//!
//! Runtime console switching ([`crate::display_control`]) keeps one active
//! `Console_N` for HLS/WebRTC. This module adds a separate, bounded path for
//! controller snapshots and info: it can open and cache a capture source for
//! another advertised console without touching the active HLS/WebRTC source.
//!
//! ```text
//! GET /display/snapshot?console=1  -> broker Control(Console_1) -> JPEG
//! GET /display/info?console=1      -> capability/last-frame/last-error
//! ```
//!
//! The broker is deliberately conservative:
//!
//! * only providers that really expose per-console capture (today D-Bus) may
//!   serve a non-active console; QMP/VNC/Looking Glass return a typed 409;
//! * a console that is not advertised is a typed 404, never an empty JPEG;
//! * at most [`ConsoleBrokerConfig::max_open`] console sources are cached and
//!   the least-recently-used console is stopped first;
//! * an idle source is stopped after [`ConsoleBrokerConfig::idle_timeout`];
//! * selecting a console as active, removing it from `display_outputs`, or
//!   shutting the node down stops the broker's cached source for it.
//!
//! The active console keeps using the existing one-shot snapshot control (the
//! same registry the HLS/WebRTC pipelines use). The broker never swaps that
//! registry, so an active stream is not disturbed. If a provider cannot expose
//! two consoles, the truthful answer is a typed conflict rather than a fake
//! second frame.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use ozma_display::vm::VmConsoleBackend;
use ozma_protocol::{codes, ApiError};
use tokio::sync::Mutex;

use crate::display_compat::{DisplayInfo, SnapshotControl, SnapshotError, SnapshotFrame};
use crate::display_control::ConsoleSourceFactory;
use crate::display_outputs::DisplayOutputSelection;

/// Default number of console capture sources kept open by the broker.
pub const DEFAULT_MAX_OPEN_CONSOLES: usize = 4;
/// Default time an unused console source may remain open.
pub const DEFAULT_CONSOLE_IDLE_TIMEOUT: Duration = Duration::from_secs(30);

/// Bounds and output settings for the multi-console broker.
#[derive(Clone, Debug)]
pub struct ConsoleBrokerConfig {
    /// Maximum number of cached console sources. Clamped to at least one.
    pub max_open: usize,
    /// Stop a console source after it has been unused for this long. Zero
    /// disables idle eviction (the `max_open` bound still applies).
    pub idle_timeout: Duration,
    /// Per-request source-open/frame/encode bound.
    pub snapshot_timeout: Duration,
    /// JPEG quality for console snapshots.
    pub jpeg_quality: u8,
}

impl Default for ConsoleBrokerConfig {
    fn default() -> Self {
        Self {
            max_open: DEFAULT_MAX_OPEN_CONSOLES,
            idle_timeout: DEFAULT_CONSOLE_IDLE_TIMEOUT,
            snapshot_timeout: crate::display_compat::DEFAULT_SNAPSHOT_TIMEOUT,
            jpeg_quality: crate::display_compat::DEFAULT_JPEG_QUALITY,
        }
    }
}

struct BrokerEntry {
    control: SnapshotControl,
    last_used: Instant,
}

struct BrokerInner {
    factory: Arc<dyn ConsoleSourceFactory>,
    config: ConsoleBrokerConfig,
    entries: Mutex<HashMap<u32, BrokerEntry>>,
}

/// Shared, bounded broker for non-active console snapshots.
#[derive(Clone)]
pub struct ConsoleBroker {
    inner: Arc<BrokerInner>,
}

impl std::fmt::Debug for ConsoleBroker {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ConsoleBroker")
            .field("max_open", &self.inner.config.max_open)
            .field("idle_timeout", &self.inner.config.idle_timeout)
            .finish_non_exhaustive()
    }
}

impl ConsoleBroker {
    /// Build a broker from the same factory used by the console selector.
    pub fn new(factory: Arc<dyn ConsoleSourceFactory>, config: ConsoleBrokerConfig) -> Self {
        Self {
            inner: Arc::new(BrokerInner {
                factory,
                config: ConsoleBrokerConfig {
                    max_open: config.max_open.max(1),
                    ..config
                },
                entries: Mutex::new(HashMap::new()),
            }),
        }
    }

    /// Configuration in use (with `max_open` clamped to at least one).
    pub fn config(&self) -> &ConsoleBrokerConfig {
        &self.inner.config
    }

    /// Console sources currently tracked (whether or not a source is open).
    pub async fn tracked_consoles(&self) -> Vec<u32> {
        let mut indexes: Vec<u32> = self.inner.entries.lock().await.keys().copied().collect();
        indexes.sort_unstable();
        indexes
    }

    /// Resolve `?console=<N|label|Console_N|source-id>` against the advertised
    /// outputs. `None` means the active console.
    pub fn resolve_console(
        &self,
        query: Option<&str>,
        selection: &DisplayOutputSelection,
    ) -> Result<u32, SnapshotError> {
        let available: Vec<u32> = selection
            .outputs
            .iter()
            .map(|output| output.index)
            .collect();
        let Some(raw) = query else {
            if selection.outputs.is_empty() {
                return Err(SnapshotError::UnknownConsole {
                    requested: None,
                    available,
                });
            }
            return Ok(selection.active_console);
        };

        let value = raw.trim();
        let parsed = value
            .parse::<u32>()
            .ok()
            .or_else(|| {
                value
                    .strip_prefix("Console_")
                    .or_else(|| value.strip_prefix("console_"))
                    .and_then(|rest| rest.parse::<u32>().ok())
            })
            .or_else(|| {
                selection
                    .outputs
                    .iter()
                    .find(|output| {
                        output.name.eq_ignore_ascii_case(value)
                            || (!output.capture_source_id.is_empty()
                                && output.capture_source_id.eq_ignore_ascii_case(value))
                    })
                    .map(|output| output.index)
            });

        match parsed {
            Some(index) if available.contains(&index) => Ok(index),
            _ => Err(SnapshotError::UnknownConsole {
                requested: Some(raw.to_owned()),
                available,
            }),
        }
    }

    /// Capture one JPEG for `console`, opening/caching its source as needed.
    pub async fn snapshot(
        &self,
        console: u32,
        selection: &DisplayOutputSelection,
    ) -> Result<SnapshotFrame, SnapshotError> {
        self.reject_if_unavailable(console, selection)?;
        let control = self.control_for(console).await?;
        let result = control.capture().await;
        let now = Instant::now();
        let mut entries = self.inner.entries.lock().await;
        if let Some(entry) = entries.get_mut(&console) {
            entry.last_used = now;
        }
        drop(entries);
        result
    }

    /// Build an independent cached control for one per-console MJPEG stream.
    ///
    /// This uses the same availability rules as [`Self::snapshot`] (advertised
    /// console, single-head conflict), but gives the stream its own source slot
    /// so ending the stream does not stop the broker's cached snapshot source.
    pub async fn streaming_control(
        &self,
        console: u32,
        selection: &DisplayOutputSelection,
        quality: Option<u8>,
    ) -> Result<SnapshotControl, SnapshotError> {
        self.reject_if_unavailable(console, selection)?;
        let control = self.control_for(console).await?;
        Ok(control.streaming(quality))
    }

    /// Per-console info without opening the capture device.
    pub async fn info(
        &self,
        console: u32,
        selection: &DisplayOutputSelection,
    ) -> Result<DisplayInfo, SnapshotError> {
        let Some(output) = selection
            .outputs
            .iter()
            .find(|output| output.index == console)
        else {
            return Err(SnapshotError::UnknownConsole {
                requested: Some(console.to_string()),
                available: selection
                    .outputs
                    .iter()
                    .map(|output| output.index)
                    .collect(),
            });
        };
        let active = selection.active_console;

        if !self.provider_supports_concurrent_consoles() && console != active {
            let provider = self
                .inner
                .factory
                .provider()
                .map(|provider| provider.as_str().to_owned())
                .unwrap_or_else(|| "single-head".to_owned());
            return Ok(DisplayInfo {
                ok: true,
                source_id: None,
                source_name: Some(output.name.clone()),
                width: output.width,
                height: output.height,
                pixel_format: None,
                snapshot_available: false,
                snapshot_encoding: "jpeg",
                dimensions_source: if output.width > 0 && output.height > 0 {
                    "display-output"
                } else {
                    "none"
                },
                last_error: Some(format!(
                    "the {provider} capture provider exposes one default console and cannot \
                     capture Console_{console} independently"
                )),
                console: Some(console),
                active: false,
            });
        }

        let control = self.control_for(console).await?;
        let mut info = control.info().await;
        if (info.width == 0 || info.dimensions_source == "capability-max")
            && output.width > 0
            && output.height > 0
        {
            // D-Bus advertises one global capability limit; the per-console
            // `--display-output` geometry is the better pre-frame answer.
            info.width = output.width;
            info.height = output.height;
            info.dimensions_source = "display-output";
        }
        info.console = Some(console);
        info.active = console == active;
        Ok(info)
    }

    /// Stop and forget a console source (for example after it becomes active
    /// or is removed from the advertised outputs).
    pub async fn forget(&self, console: u32) {
        let removed = {
            let mut entries = self.inner.entries.lock().await;
            entries.remove(&console)
        };
        if let Some(entry) = removed {
            entry.control.shutdown().await;
        }
    }

    /// Stop cached sources for a newly active console and for consoles no
    /// longer advertised. The active console returns to the one-shot control
    /// used by HLS/WebRTC snapshots.
    pub async fn on_select(&self, active: u32, available: &[u32]) {
        self.on_switch(active, available).await;
    }

    /// Alias used by the select handler; see [`Self::on_select`].
    pub async fn on_switch(&self, active: u32, available: &[u32]) {
        let removed: Vec<SnapshotControl> = {
            let mut entries = self.inner.entries.lock().await;
            let mut removed = Vec::new();
            entries.retain(|index, entry| {
                let keep = *index != active && available.contains(index);
                if !keep {
                    removed.push(entry.control.clone());
                }
                keep
            });
            removed
        };
        for control in removed {
            control.shutdown().await;
        }
    }

    /// Stop and remove every cached source. Called during node shutdown.
    pub async fn shutdown(&self) {
        let removed: Vec<SnapshotControl> = {
            let mut entries = self.inner.entries.lock().await;
            entries.drain().map(|(_, entry)| entry.control).collect()
        };
        for control in removed {
            control.shutdown().await;
        }
    }

    /// Evict sources idle for at least `idle_timeout`. Returns how many were
    /// stopped. A zero timeout disables idle eviction.
    pub async fn evict_idle(&self, now: Instant) -> usize {
        if self.inner.config.idle_timeout.is_zero() {
            return 0;
        }
        let removed: Vec<SnapshotControl> = {
            let mut entries = self.inner.entries.lock().await;
            let mut removed = Vec::new();
            entries.retain(|_, entry| {
                let idle = now.saturating_duration_since(entry.last_used);
                let keep = idle < self.inner.config.idle_timeout;
                if !keep {
                    removed.push(entry.control.clone());
                }
                keep
            });
            removed
        };
        let count = removed.len();
        for control in removed {
            control.shutdown().await;
        }
        count
    }

    fn provider_supports_concurrent_consoles(&self) -> bool {
        matches!(self.inner.factory.provider(), Some(VmConsoleBackend::Dbus))
    }

    fn reject_if_unavailable(
        &self,
        console: u32,
        selection: &DisplayOutputSelection,
    ) -> Result<(), SnapshotError> {
        if !selection
            .outputs
            .iter()
            .any(|output| output.index == console)
        {
            return Err(SnapshotError::UnknownConsole {
                requested: Some(console.to_string()),
                available: selection
                    .outputs
                    .iter()
                    .map(|output| output.index)
                    .collect(),
            });
        }
        let Some(provider) = self.inner.factory.provider() else {
            return Err(SnapshotError::NoSource {
                requested: Some(format!("Console_{console}")),
            });
        };
        if !self.provider_supports_concurrent_consoles() && console != selection.active_console {
            return Err(SnapshotError::SingleHead {
                provider: provider.as_str().to_owned(),
                active: selection.active_console,
                requested: console,
            });
        }
        Ok(())
    }

    async fn control_for(&self, console: u32) -> Result<SnapshotControl, SnapshotError> {
        self.evict_idle(Instant::now()).await;

        let mut entries = self.inner.entries.lock().await;
        if let Some(entry) = entries.get_mut(&console) {
            entry.last_used = Instant::now();
            return Ok(entry.control.clone());
        }

        let mut evicted = Vec::new();
        if entries.len() >= self.inner.config.max_open {
            if let Some(index) = entries
                .iter()
                .min_by_key(|(_, entry)| entry.last_used)
                .map(|(index, _)| *index)
            {
                if let Some(entry) = entries.remove(&index) {
                    evicted.push(entry.control);
                }
            }
        }

        let registry = self.inner.factory.registry(console).map_err(|message| {
            SnapshotError::Open(
                ApiError::new(
                    codes::CAPTURE_UNAVAILABLE,
                    format!(
                        "building the capture registry for Console_{console} failed: {message}"
                    ),
                )
                .with_details(console)
                .with_remediation(
                    "Check the QMP socket and D-Bus display, then retry the console snapshot.",
                )
                .retryable(),
            )
        })?;
        let control = SnapshotControl::new(registry)
            .with_timeout(self.inner.config.snapshot_timeout)
            .with_quality(self.inner.config.jpeg_quality)
            .with_source_cache(true);
        entries.insert(
            console,
            BrokerEntry {
                control: control.clone(),
                last_used: Instant::now(),
            },
        );
        drop(entries);

        for old in evicted {
            old.shutdown().await;
        }
        Ok(control)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use ozma_capture::{CaptureProvider, CaptureRegistry};
    use ozma_media::{CaptureSource, FakeCapture, Frame, FrameFormat, MediaError};
    use ozma_protocol::{CaptureCapability, CaptureKind};
    use std::sync::atomic::{AtomicUsize, Ordering};

    use crate::display_outputs;

    fn selection(active: u32) -> DisplayOutputSelection {
        display_outputs::outputs_from_specs(
            &[
                "0:Console_0:1920x1080:primary:dbus".to_owned(),
                "1:Console_1:1280x1024:dbus".to_owned(),
            ],
            VmConsoleBackend::Dbus,
            Some("dbus0"),
            Some(active),
        )
        .expect("valid outputs")
    }

    #[derive(Default)]
    struct Counters {
        opens: AtomicUsize,
        stops: AtomicUsize,
    }

    struct CountingProvider {
        id: String,
        name: String,
        width: u32,
        height: u32,
        counters: Arc<Counters>,
    }

    impl CaptureProvider for CountingProvider {
        fn kind(&self) -> CaptureKind {
            CaptureKind::Dbus
        }

        fn enumerate(&self) -> Result<Vec<CaptureCapability>, ApiError> {
            Ok(vec![CaptureCapability {
                id: self.id.clone(),
                name: self.name.clone(),
                kind: CaptureKind::Dbus,
                formats: vec!["bgra".to_owned()],
                max_width: self.width,
                max_height: self.height,
                max_fps: 10,
                compressed: false,
                hdr: false,
                has_audio: false,
            }])
        }

        fn open(&self, capability: &CaptureCapability) -> Result<Box<dyn CaptureSource>, ApiError> {
            assert_eq!(capability.id, self.id);
            self.counters.opens.fetch_add(1, Ordering::SeqCst);
            let source = FakeCapture::new(FrameFormat::rgb24(self.width, self.height))
                .with_id(self.id.clone())
                .with_fps(10);
            Ok(Box::new(CountingSource {
                inner: source,
                stops: self.counters.clone(),
            }))
        }
    }

    struct CountingSource {
        inner: FakeCapture,
        stops: Arc<Counters>,
    }

    #[async_trait]
    impl CaptureSource for CountingSource {
        fn id(&self) -> &str {
            self.inner.id()
        }

        fn format(&self) -> FrameFormat {
            self.inner.format()
        }

        async fn next_frame(&mut self) -> Result<Frame, MediaError> {
            self.inner.next_frame().await
        }

        async fn stop(&mut self) -> Result<(), MediaError> {
            self.stops.stops.fetch_add(1, Ordering::SeqCst);
            self.inner.stop().await
        }
    }

    struct FakeFactory {
        provider: Option<VmConsoleBackend>,
        consoles: Vec<u32>,
        counters: std::sync::Mutex<HashMap<u32, Arc<Counters>>>,
    }

    impl FakeFactory {
        fn dbus() -> (Arc<Self>, Arc<Counters>, Arc<Counters>) {
            let zero = Arc::new(Counters::default());
            let one = Arc::new(Counters::default());
            let factory = Arc::new(Self {
                provider: Some(VmConsoleBackend::Dbus),
                consoles: vec![0, 1],
                counters: std::sync::Mutex::new(HashMap::from([
                    (0, zero.clone()),
                    (1, one.clone()),
                ])),
            });
            (factory, zero, one)
        }
    }

    #[async_trait]
    impl ConsoleSourceFactory for FakeFactory {
        fn provider(&self) -> Option<VmConsoleBackend> {
            self.provider
        }

        fn default_console(&self) -> u32 {
            0
        }

        fn supports_console_switch(&self) -> bool {
            matches!(self.provider, Some(VmConsoleBackend::Dbus))
        }

        fn registry(&self, console: u32) -> Result<CaptureRegistry, String> {
            if !self.consoles.contains(&console) {
                return Err(format!("Console_{console} is not advertised"));
            }
            let counters = self
                .counters
                .lock()
                .unwrap()
                .get(&console)
                .cloned()
                .unwrap_or_default();
            Ok(CaptureRegistry::new().with(CountingProvider {
                id: format!("console-{console}"),
                name: format!("Console_{console}"),
                width: if console == 0 { 64 } else { 32 },
                height: if console == 0 { 48 } else { 24 },
                counters,
            }))
        }

        fn selection(&self, console: u32) -> Result<DisplayOutputSelection, String> {
            Ok(selection(console))
        }
    }

    fn config(max_open: usize, idle: Duration) -> ConsoleBrokerConfig {
        ConsoleBrokerConfig {
            max_open,
            idle_timeout: idle,
            snapshot_timeout: Duration::from_secs(2),
            jpeg_quality: 85,
        }
    }

    #[tokio::test]
    async fn two_consoles_capture_independently_and_reuse_open_sources() {
        let (factory, zero, one) = FakeFactory::dbus();
        let broker = ConsoleBroker::new(factory, config(4, Duration::from_secs(30)));
        let outputs = selection(0);

        let first = broker.snapshot(1, &outputs).await.expect("console 1");
        assert_eq!(first.source_id, "console-1");
        assert_eq!(first.source_name, "Console_1");
        assert_eq!((first.format.width, first.format.height), (32, 24));
        let second = broker.snapshot(1, &outputs).await.expect("console 1 again");
        assert_eq!(second.source_id, "console-1");

        // One open, no stops: the broker reused the cached source.
        assert_eq!(one.opens.load(Ordering::SeqCst), 1);
        assert_eq!(one.stops.load(Ordering::SeqCst), 0);
        // Console 0 was not touched by a Console_1 snapshot.
        assert_eq!(zero.opens.load(Ordering::SeqCst), 0);
        assert_eq!(broker.tracked_consoles().await, vec![1]);
    }

    #[tokio::test]
    async fn max_open_evicts_the_least_recently_used_console() {
        let (factory, zero, one) = FakeFactory::dbus();
        let broker = ConsoleBroker::new(factory, config(1, Duration::from_secs(30)));
        let outputs = selection(0);

        broker.snapshot(1, &outputs).await.expect("console 1");
        assert_eq!(one.opens.load(Ordering::SeqCst), 1);
        broker.snapshot(0, &outputs).await.expect("console 0");

        assert_eq!(zero.opens.load(Ordering::SeqCst), 1);
        assert_eq!(one.stops.load(Ordering::SeqCst), 1);
        assert_eq!(broker.tracked_consoles().await, vec![0]);
    }

    #[tokio::test]
    async fn idle_eviction_stops_the_open_source() {
        let (factory, _zero, one) = FakeFactory::dbus();
        let broker = ConsoleBroker::new(factory, config(4, Duration::from_millis(10)));
        let outputs = selection(0);

        broker.snapshot(1, &outputs).await.expect("console 1");
        assert_eq!(one.stops.load(Ordering::SeqCst), 0);
        let evicted = broker
            .evict_idle(Instant::now() + Duration::from_millis(11))
            .await;
        assert_eq!(evicted, 1);
        assert_eq!(one.stops.load(Ordering::SeqCst), 1);
        assert!(broker.tracked_consoles().await.is_empty());
    }

    #[tokio::test]
    async fn single_head_non_active_console_is_a_typed_conflict() {
        let (factory, _zero, _one) = FakeFactory::dbus();
        let factory = Arc::new(FakeFactory {
            provider: Some(VmConsoleBackend::Qmp),
            consoles: factory.consoles.clone(),
            counters: std::sync::Mutex::new(factory.counters.lock().unwrap().clone()),
        });
        let broker = ConsoleBroker::new(factory, config(4, Duration::from_secs(30)));
        let outputs = selection(0);

        let error = broker.snapshot(1, &outputs).await.unwrap_err();
        let status = error.status();
        match &error {
            SnapshotError::SingleHead {
                provider,
                active,
                requested,
            } => {
                assert_eq!(provider, "qmp");
                assert_eq!(*active, 0);
                assert_eq!(*requested, 1);
            }
            other => panic!("expected single-head conflict, got {other:?}"),
        }
        assert_eq!(status, axum::http::StatusCode::CONFLICT);

        // Info is honest instead of returning a fabricated source.
        let info = broker.info(1, &outputs).await.expect("info");
        assert!(!info.snapshot_available);
        assert_eq!(info.console, Some(1));
        assert!(info
            .last_error
            .as_deref()
            .unwrap()
            .contains("one default console"));
    }

    #[tokio::test]
    async fn unknown_console_is_a_typed_not_found() {
        let (factory, _zero, _one) = FakeFactory::dbus();
        let broker = ConsoleBroker::new(factory, config(4, Duration::from_secs(30)));
        let outputs = selection(0);

        let error = broker
            .resolve_console(Some("Console_9"), &outputs)
            .unwrap_err();
        assert!(matches!(
            error,
            SnapshotError::UnknownConsole { ref available, .. } if available == &[0, 1]
        ));
        assert_eq!(error.status(), axum::http::StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn switch_and_shutdown_stop_cached_sources() {
        let (factory, _zero, one) = FakeFactory::dbus();
        let broker = ConsoleBroker::new(factory, config(4, Duration::from_secs(30)));
        let outputs = selection(0);

        broker.snapshot(1, &outputs).await.expect("console 1");
        broker.on_select(1, &[0, 1]).await;
        assert_eq!(one.stops.load(Ordering::SeqCst), 1);
        assert!(broker.tracked_consoles().await.is_empty());

        broker.snapshot(1, &selection(0)).await.expect("console 1");
        broker.shutdown().await;
        assert_eq!(one.stops.load(Ordering::SeqCst), 2);
        assert!(broker.tracked_consoles().await.is_empty());
    }

    #[tokio::test]
    async fn info_reports_console_geometry_and_active_flag() {
        let (factory, _zero, _one) = FakeFactory::dbus();
        let broker = ConsoleBroker::new(factory, config(4, Duration::from_secs(30)));
        let outputs = selection(0);

        // Before the first frame the per-console output geometry wins over the
        // provider's global capability limit.
        let info = broker.info(1, &outputs).await.expect("console 1 info");
        assert_eq!(info.console, Some(1));
        assert!(!info.active);
        assert_eq!(info.source_id.as_deref(), Some("console-1"));
        assert_eq!((info.width, info.height), (1280, 1024));
        assert!(info.snapshot_available);
        assert_eq!(info.dimensions_source, "display-output");

        // After a capture the real frame geometry and source are reported.
        let snapshot = broker.snapshot(1, &outputs).await.expect("console 1");
        assert_eq!((snapshot.format.width, snapshot.format.height), (32, 24));
        let info = broker
            .info(1, &outputs)
            .await
            .expect("console 1 info again");
        assert_eq!((info.width, info.height), (32, 24));
        assert_eq!(info.dimensions_source, "last-frame");
        assert!(info.last_error.is_none());
    }
}
