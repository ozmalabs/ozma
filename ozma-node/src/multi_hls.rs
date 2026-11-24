// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, true simultaneous per-console HLS.
//!
//! The default HLS path (`--hls`) owns one active capture source and serves it
//! at `/stream/*`. This module adds an opt-in manager (`--multi-console-hls`)
//! that runs an independent [`HlsController`] and ffmpeg generation for each
//! advertised, capturable console:
//!
//! ```text
//! <hls-root>/console-0/stream.m3u8  -> /stream/console-0/stream.m3u8
//! <hls-root>/console-1/stream.m3u8  -> /stream/console-1/stream.m3u8
//! <active console dir>              -> /stream/stream.m3u8 (backward compatible)
//! ```
//!
//! Honesty and bounds:
//!
//! * only D-Bus exposes independent console indexes today; QMP/VNC/Looking
//!   Glass are reported as a typed `unsupported` limitation and start nothing;
//! * at most [`MultiConsoleHlsConfig::max_pipelines`] ffmpeg processes run. The
//!   active console is always preferred; another console rotates in by stopping
//!   the least-recently-used non-active pipeline;
//! * a non-active pipeline is stopped after the configured idle timeout without
//!   a `/stream/console-N/...` request (zero disables idle eviction);
//! * selecting a console does not restart the independent pipelines: it only
//!   changes which directory `/stream/*` serves and which console `/display/info`
//!   and `/api/v1/connection` mark active;
//! * a console removed from `display_outputs` and every pipeline on shutdown are
//!   stopped with the existing bounded `HlsPipeline::stop` path.
//!
//! WebRTC remains active-console only. This module never claims simultaneous
//! WebRTC.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use axum::extract::{Path as AxumPath, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::Router;
use ozma_display::vm::{VmConsoleBackend, MAX_VM_CONSOLES};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::display_control::{open_first_source, ConsoleSelectionObserver, ConsoleSourceFactory};
use crate::display_outputs::{DisplayOutput, DisplayOutputSelection};
use crate::hls::{HlsConfig, HlsController, HlsError, HlsSourceFactory, HlsStatus};

/// Default maximum number of simultaneous per-console HLS pipelines.
pub const DEFAULT_MAX_CONSOLE_HLS: usize = 2;
/// Default idle timeout for a non-active per-console pipeline.
pub const DEFAULT_CONSOLE_HLS_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest status error retained (matches the HLS status bound).
const MAX_STATUS_ERROR_CHARS: usize = 512;

/// Bounds and output settings for per-console HLS.
#[derive(Clone, Debug)]
pub struct MultiConsoleHlsConfig {
    /// Maximum simultaneous pipelines/encoders, including the active console.
    /// Clamped to `1..=MAX_VM_CONSOLES`.
    pub max_pipelines: usize,
    /// Stop a non-active pipeline after this long without a stream request.
    /// Zero disables idle eviction (the `max_pipelines` bound still applies).
    pub idle_timeout: Duration,
    /// Root directory containing `console-<N>/stream.m3u8`.
    pub root: PathBuf,
    /// Base HLS configuration; `output_dir` is replaced per console.
    pub base: HlsConfig,
}

impl MultiConsoleHlsConfig {
    /// Build a config with the default per-console idle timeout.
    pub fn new(root: impl Into<PathBuf>, base: HlsConfig) -> Self {
        Self {
            max_pipelines: DEFAULT_MAX_CONSOLE_HLS,
            idle_timeout: DEFAULT_CONSOLE_HLS_IDLE_TIMEOUT,
            root: root.into(),
            base,
        }
    }

    fn bounded_max(&self) -> usize {
        self.max_pipelines.max(1).min(MAX_VM_CONSOLES as usize)
    }
}

/// One owned per-console encoder generation.
#[async_trait]
pub trait ConsoleHlsPipeline: Send + Sync {
    /// Live status (running only while the manifest exists).
    fn status(&self) -> HlsStatus;
    /// Stop the source and reap ffmpeg with the pipeline's bounded wait.
    async fn stop(&self) -> Result<(), String>;
    /// Playlist this generation writes.
    fn manifest_path(&self) -> PathBuf;
}

/// Injectable pipeline factory.
///
/// Production uses [`ControllerHlsFactory`] to open a fresh registry/source for
/// the console and start a real [`HlsController`]. Tests inject a recording fake
/// so no ffmpeg or capture device is needed.
#[async_trait]
pub trait ConsoleHlsFactory: Send + Sync {
    /// Configured capture transport.
    fn provider(&self) -> Option<VmConsoleBackend>;
    /// Start one independent pipeline for `console`.
    async fn start(
        &self,
        console: u32,
        config: HlsConfig,
    ) -> Result<Arc<dyn ConsoleHlsPipeline>, String>;
}

struct ControllerPipeline {
    controller: HlsController,
}

#[async_trait]
impl ConsoleHlsPipeline for ControllerPipeline {
    fn status(&self) -> HlsStatus {
        self.controller.status()
    }

    async fn stop(&self) -> Result<(), String> {
        self.controller
            .stop()
            .await
            .map_err(|error| error.to_string())
    }

    fn manifest_path(&self) -> PathBuf {
        self.controller.config().manifest_path()
    }
}

struct ControllerHlsFactory {
    console_factory: Arc<dyn ConsoleSourceFactory>,
}

#[async_trait]
impl ConsoleHlsFactory for ControllerHlsFactory {
    fn provider(&self) -> Option<VmConsoleBackend> {
        self.console_factory.provider()
    }

    async fn start(
        &self,
        console: u32,
        config: HlsConfig,
    ) -> Result<Arc<dyn ConsoleHlsPipeline>, String> {
        let registry = self.console_factory.registry(console)?;
        let source_factory: HlsSourceFactory = Arc::new(move || {
            open_first_source(&registry).map_err(|message| HlsError::Source { message })
        });
        let controller = HlsController::new(source_factory, config);
        controller
            .start()
            .await
            .map_err(|error| error.to_string())?;
        Ok(Arc::new(ControllerPipeline { controller }))
    }
}

struct Entry {
    pipeline: Option<Arc<dyn ConsoleHlsPipeline>>,
    error: Option<String>,
    last_used: Instant,
}

struct ManagerState {
    selection: DisplayOutputSelection,
    entries: HashMap<u32, Entry>,
    limitation: Option<String>,
}

struct Inner {
    factory: Arc<dyn ConsoleHlsFactory>,
    config: MultiConsoleHlsConfig,
    state: Mutex<ManagerState>,
}

/// Per-console status for one advertised console.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConsoleHlsStreamStatus {
    /// Zero-based `Console_N` index.
    pub console: u32,
    /// True when this console is the active HLS/WebRTC console.
    pub active: bool,
    /// `running`, `starting`, `failed`, `not-started`, or `unsupported`.
    pub status: String,
    /// Why the pipeline is not running, when it is configured and failing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Controller-relative playlist path once a pipeline owns the console.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub playlist: Option<String>,
    /// Local playlist path (diagnostics; not fetched by browsers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub manifest_path: Option<String>,
    /// Local output directory (diagnostics; not fetched by browsers).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_dir: Option<String>,
}

/// Per-console HLS summary for `/api/v1/connection`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MultiConsoleHlsStatus {
    /// The manager is enabled on this node.
    pub enabled: bool,
    /// Capture transport, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Maximum simultaneous pipelines.
    pub max_pipelines: usize,
    /// Idle timeout applied to non-active consoles (zero = disabled).
    pub idle_timeout_seconds: u64,
    /// Active HLS/WebRTC console.
    pub active_console: u32,
    /// Why some advertised consoles are not encoded (provider/bound/transport).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limitation: Option<String>,
    /// Per-console pipeline state.
    pub consoles: Vec<ConsoleHlsStreamStatus>,
}

/// Bounded manager for independent per-console HLS pipelines.
#[derive(Clone)]
pub struct MultiConsoleHls {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MultiConsoleHls {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MultiConsoleHls")
            .field("max_pipelines", &self.inner.config.max_pipelines)
            .field("root", &self.inner.config.root)
            .finish_non_exhaustive()
    }
}

impl MultiConsoleHls {
    /// Build a manager for an injectable pipeline factory (tests/embedding).
    pub fn new(factory: Arc<dyn ConsoleHlsFactory>, config: MultiConsoleHlsConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                factory,
                config: MultiConsoleHlsConfig {
                    max_pipelines: config.bounded_max(),
                    ..config
                },
                state: Mutex::new(ManagerState {
                    selection: DisplayOutputSelection::empty(),
                    entries: HashMap::new(),
                    limitation: None,
                }),
            }),
        }
    }

    /// Build a manager that starts real [`HlsController`] generations from the
    /// node's console source factory.
    pub fn from_console_factory(
        console_factory: Arc<dyn ConsoleSourceFactory>,
        config: MultiConsoleHlsConfig,
    ) -> Self {
        Self::new(Arc::new(ControllerHlsFactory { console_factory }), config)
    }

    /// Root directory containing per-console output directories.
    pub fn root(&self) -> &Path {
        &self.inner.config.root
    }

    /// Directory for one console (`<root>/console-<N>`).
    pub fn console_dir(&self, console: u32) -> PathBuf {
        console_dir_for(&self.inner.config.root, console)
    }

    /// Playlist path for one console (`<root>/console-<N>/stream.m3u8`).
    pub fn manifest_path(&self, console: u32) -> PathBuf {
        console_manifest_path_for(&self.inner.config.root, console)
    }

    /// True when the configured provider can expose independent consoles.
    ///
    /// Today only D-Bus does. A single-head provider keeps the active single
    /// HLS/WebRTC path and reports an `unsupported` per-console limitation.
    pub fn supports_independent_consoles(&self) -> bool {
        matches!(self.inner.factory.provider(), Some(VmConsoleBackend::Dbus))
    }

    /// Active console, or `0` when no outputs are advertised.
    pub async fn active_console(&self) -> u32 {
        self.inner.state.lock().await.selection.active_console
    }

    /// True when `console` is advertised by the current selection.
    pub async fn is_advertised(&self, console: u32) -> bool {
        self.inner
            .state
            .lock()
            .await
            .selection
            .outputs
            .iter()
            .any(|output| output.index == console)
    }

    /// Reconcile the advertised consoles with the running pipelines.
    ///
    /// Called at startup and after every committed `/display/select`. The
    /// active console is always considered first; consoles removed from the
    /// selection are stopped, and new consoles start while capacity remains.
    pub async fn sync(&self, selection: &DisplayOutputSelection) {
        let mut state = self.inner.state.lock().await;
        state.selection = selection.clone();

        let provider = self.inner.factory.provider();
        if !matches!(provider, Some(VmConsoleBackend::Dbus)) {
            let removed: Vec<Entry> = state.entries.drain().map(|(_, entry)| entry).collect();
            state.limitation = Some(unsupported_message(provider));
            drop(state);
            stop_entries(removed).await;
            return;
        }

        let capturable: Vec<u32> = selection
            .outputs
            .iter()
            .filter(|output| output_is_capturable(output, provider))
            .map(|output| output.index)
            .collect();
        let mut notes = Vec::new();
        if capturable.len() < selection.outputs.len() {
            notes.push(format!(
                "{} advertised console(s) are not advertised as {} and do not get a \
                 per-console HLS pipeline",
                selection.outputs.len() - capturable.len(),
                VmConsoleBackend::Dbus
            ));
        }

        let mut desired = capturable;
        if let Some(position) = desired
            .iter()
            .position(|index| *index == selection.active_console)
        {
            let active = desired.remove(position);
            desired.insert(0, active);
        }
        let max = self.inner.config.max_pipelines;
        if desired.len() > max {
            notes.push(format!(
                "{} advertised consoles have a live capture source, but per-console HLS is \
                 bounded to {max} pipeline(s); request /stream/console-<N>/... to rotate a \
                 console into the bound",
                desired.len()
            ));
            desired.truncate(max);
        }
        let desired_set: HashSet<u32> = desired.iter().copied().collect();

        let removed: Vec<Entry> = {
            let stale: Vec<u32> = state
                .entries
                .keys()
                .copied()
                .filter(|index| !desired_set.contains(index))
                .collect();
            stale
                .into_iter()
                .filter_map(|index| state.entries.remove(&index))
                .collect()
        };
        for entry in removed {
            stop_entry(entry).await;
        }

        state.limitation = (!notes.is_empty()).then(|| notes.join("; "));

        for console in desired {
            let needs_start = state
                .entries
                .get(&console)
                .map(|entry| entry.pipeline.is_none())
                .unwrap_or(true);
            if !needs_start {
                if console == selection.active_console {
                    touch_entry(&mut state, console);
                }
                continue;
            }

            if state.entries.len() >= max {
                if let Some(victim) =
                    least_recently_used_non_active(&state, selection.active_console)
                {
                    if let Some(entry) = state.entries.remove(&victim) {
                        stop_entry(entry).await;
                    }
                }
            }

            let mut config = self.inner.config.base.clone();
            config.output_dir = console_dir_for(&self.inner.config.root, console);
            let result = self.inner.factory.start(console, config).await;
            let now = Instant::now();
            match result {
                Ok(pipeline) => {
                    state.entries.insert(
                        console,
                        Entry {
                            pipeline: Some(pipeline),
                            error: None,
                            last_used: now,
                        },
                    );
                }
                Err(error) => {
                    state.entries.insert(
                        console,
                        Entry {
                            pipeline: None,
                            error: Some(bounded(error)),
                            last_used: now,
                        },
                    );
                }
            }
        }
        touch_entry(&mut state, selection.active_console);
    }

    /// Mark a console as recently used, starting it if it was evicted/omitted.
    ///
    /// Returns true when a pipeline currently owns the console. A console that
    /// is not advertised returns false without starting anything.
    pub async fn touch(&self, console: u32) -> bool {
        let mut state = self.inner.state.lock().await;
        if !state
            .selection
            .outputs
            .iter()
            .any(|output| output.index == console)
        {
            return false;
        }
        let provider = self.inner.factory.provider();
        if !matches!(provider, Some(VmConsoleBackend::Dbus)) {
            return false;
        }
        if let Some(entry) = state.entries.get_mut(&console) {
            entry.last_used = Instant::now();
            return entry.pipeline.is_some();
        }

        let max = self.inner.config.max_pipelines;
        if state.entries.len() >= max {
            let active = state.selection.active_console;
            if let Some(victim) = least_recently_used_non_active(&state, active) {
                if let Some(entry) = state.entries.remove(&victim) {
                    stop_entry(entry).await;
                }
            } else {
                return false;
            }
        }

        let mut config = self.inner.config.base.clone();
        config.output_dir = console_dir_for(&self.inner.config.root, console);
        let result = self.inner.factory.start(console, config).await;
        let now = Instant::now();
        match result {
            Ok(pipeline) => {
                state.entries.insert(
                    console,
                    Entry {
                        pipeline: Some(pipeline),
                        error: None,
                        last_used: now,
                    },
                );
                true
            }
            Err(error) => {
                state.entries.insert(
                    console,
                    Entry {
                        pipeline: None,
                        error: Some(bounded(error)),
                        last_used: now,
                    },
                );
                false
            }
        }
    }

    /// Stop non-active pipelines idle for at least the configured timeout.
    ///
    /// Returns how many pipelines were stopped. A zero timeout disables this.
    pub async fn evict_idle(&self, now: Instant) -> usize {
        let timeout = self.inner.config.idle_timeout;
        if timeout.is_zero() {
            return 0;
        }
        let mut state = self.inner.state.lock().await;
        let active = state.selection.active_console;
        let idle: Vec<u32> = state
            .entries
            .iter()
            .filter(|(index, entry)| {
                **index != active && now.saturating_duration_since(entry.last_used) >= timeout
            })
            .map(|(index, _)| *index)
            .collect();
        let entries: Vec<Entry> = idle
            .into_iter()
            .filter_map(|index| state.entries.remove(&index))
            .collect();
        let count = entries.len();
        drop(state);
        stop_entries(entries).await;
        count
    }

    /// Stop one console pipeline without removing the console from the
    /// advertised set. Used by tests and future add/remove control surfaces.
    pub async fn stop_console(&self, console: u32) -> bool {
        let removed = self.inner.state.lock().await.entries.remove(&console);
        match removed {
            Some(entry) => {
                stop_entry(entry).await;
                true
            }
            None => false,
        }
    }

    /// Stop every pipeline. Called during node shutdown.
    pub async fn shutdown(&self) {
        let entries: Vec<Entry> = self
            .inner
            .state
            .lock()
            .await
            .entries
            .drain()
            .map(|(_, entry)| entry)
            .collect();
        stop_entries(entries).await;
    }

    /// Active console status in the shape `/api/v1/connection.hls` uses.
    pub async fn active_status(&self) -> HlsStatus {
        let state = self.inner.state.lock().await;
        if state.selection.outputs.is_empty() {
            return HlsStatus::not_configured();
        }
        let provider = self.inner.factory.provider();
        if !matches!(provider, Some(VmConsoleBackend::Dbus)) {
            return HlsStatus::failed(unsupported_message(provider));
        }
        let active = state.selection.active_console;
        match state.entries.get(&active) {
            Some(entry) => match entry.pipeline.as_ref() {
                Some(pipeline) => pipeline.status(),
                None => HlsStatus::failed(entry.error.clone().unwrap_or_else(|| {
                    format!("per-console HLS for Console_{active} is not running")
                })),
            },
            None => HlsStatus::failed(format!(
                "per-console HLS for the active Console_{active} is not running"
            )),
        }
    }

    /// Status for every advertised console.
    pub async fn status(&self) -> MultiConsoleHlsStatus {
        let state = self.inner.state.lock().await;
        let provider = self.inner.factory.provider();
        let supported = matches!(provider, Some(VmConsoleBackend::Dbus));
        let active = state.selection.active_console;
        let mut consoles = Vec::with_capacity(state.selection.outputs.len());

        for output in &state.selection.outputs {
            let capturable = supported && output_is_capturable(output, provider);
            let entry = state.entries.get(&output.index);
            let (status, error, manifest_path) = if !capturable {
                (
                    "unsupported".to_owned(),
                    Some(unsupported_message(provider)),
                    None,
                )
            } else if let Some(entry) = entry {
                match entry.pipeline.as_ref() {
                    Some(pipeline) => {
                        let status = pipeline.status();
                        (
                            status.status_name().to_owned(),
                            status.error(),
                            Some(pipeline.manifest_path()),
                        )
                    }
                    None => (
                        "failed".to_owned(),
                        entry.error.clone().or_else(|| {
                            Some(format!(
                                "per-console HLS for Console_{} is not running",
                                output.index
                            ))
                        }),
                        None,
                    ),
                }
            } else {
                (
                    "not-started".to_owned(),
                    Some(
                        "not started because the per-console HLS bound was reached; request \
                         /stream/console-<N>/... to rotate it in"
                            .to_owned(),
                    ),
                    None,
                )
            };
            let output_dir = console_dir_for(&self.inner.config.root, output.index);
            consoles.push(ConsoleHlsStreamStatus {
                console: output.index,
                active: output.index == active,
                status,
                error,
                playlist: manifest_path
                    .is_some()
                    .then(|| format!("/stream/console-{}/stream.m3u8", output.index)),
                manifest_path: manifest_path.map(|path| path.display().to_string()),
                output_dir: Some(output_dir.display().to_string()),
            });
        }

        MultiConsoleHlsStatus {
            enabled: true,
            provider: provider.map(|provider| provider.as_str().to_owned()),
            max_pipelines: self.inner.config.max_pipelines,
            idle_timeout_seconds: self.inner.config.idle_timeout.as_secs(),
            active_console: active,
            limitation: state.limitation.clone(),
            consoles,
        }
    }

    /// Status for one console, when it is advertised.
    pub async fn status_for(&self, console: u32) -> Option<ConsoleHlsStreamStatus> {
        self.status()
            .await
            .consoles
            .into_iter()
            .find(|entry| entry.console == console)
    }
}

#[async_trait]
impl ConsoleSelectionObserver for MultiConsoleHls {
    async fn on_selection(&self, selection: &DisplayOutputSelection) {
        self.sync(selection).await;
    }
}

/// Build the unauthenticated per-console HLS file service.
///
/// `/stream/{file}` follows the active console; `/stream/console-{N}/{file}`
/// addresses one advertised console. Files are read from
/// `<root>/console-<N>/` only, and a path segment containing a separator or a
/// parent component is a typed 404.
pub fn stream_router(manager: MultiConsoleHls) -> Router {
    Router::new()
        .route("/stream/:file", get(active_stream))
        .route("/stream/:console/:file", get(console_stream))
        .with_state(manager)
}

async fn active_stream(
    State(manager): State<MultiConsoleHls>,
    AxumPath(file): AxumPath<String>,
) -> Response {
    if manager
        .inner
        .state
        .lock()
        .await
        .selection
        .outputs
        .is_empty()
    {
        return StatusCode::NOT_FOUND.into_response();
    }
    let console = manager.active_console().await;
    serve_file(&manager, console, &file).await
}

async fn console_stream(
    State(manager): State<MultiConsoleHls>,
    AxumPath((console, file)): AxumPath<(String, String)>,
) -> Response {
    let Some(console) = console
        .strip_prefix("console-")
        .and_then(|index| index.parse::<u32>().ok())
    else {
        return StatusCode::NOT_FOUND.into_response();
    };
    serve_file(&manager, console, &file).await
}

async fn serve_file(manager: &MultiConsoleHls, console: u32, file: &str) -> Response {
    if !valid_file_name(file) {
        return StatusCode::NOT_FOUND.into_response();
    }
    if !manager.is_advertised(console).await {
        return StatusCode::NOT_FOUND.into_response();
    }
    let running = manager.touch(console).await;
    let path = manager.console_dir(console).join(file);
    match tokio::fs::read(&path).await {
        Ok(bytes) => (
            [
                (header::CONTENT_TYPE, content_type(file)),
                (header::CACHE_CONTROL, "no-store"),
            ],
            bytes,
        )
            .into_response(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            if !running && file.ends_with(".m3u8") {
                (
                    StatusCode::SERVICE_UNAVAILABLE,
                    [(header::RETRY_AFTER, "1")],
                    format!("per-console HLS for Console_{console} is not ready"),
                )
                    .into_response()
            } else {
                StatusCode::NOT_FOUND.into_response()
            }
        }
        Err(_) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            "reading the per-console HLS file failed",
        )
            .into_response(),
    }
}

fn valid_file_name(file: &str) -> bool {
    !file.is_empty()
        && file != "."
        && file != ".."
        && !file.contains('/')
        && !file.contains('\\')
        && !file.contains('\0')
}

fn content_type(file: &str) -> &'static str {
    if file.ends_with(".m3u8") {
        "application/vnd.apple.mpegurl"
    } else if file.ends_with(".ts") {
        "video/mp2t"
    } else {
        "application/octet-stream"
    }
}

fn console_dir_for(root: &Path, console: u32) -> PathBuf {
    root.join(format!("console-{console}"))
}

fn console_manifest_path_for(root: &Path, console: u32) -> PathBuf {
    console_dir_for(root, console).join("stream.m3u8")
}

fn output_is_capturable(output: &DisplayOutput, provider: Option<VmConsoleBackend>) -> bool {
    match provider {
        Some(provider) => {
            output.source_type.is_empty()
                || output.source_type.eq_ignore_ascii_case(provider.as_str())
        }
        None => false,
    }
}

fn unsupported_message(provider: Option<VmConsoleBackend>) -> String {
    match provider {
        Some(provider) => format!(
            "the {} capture provider exposes one default console and cannot run independent \
             per-console HLS pipelines; use the single --hls stream and POST /display/select",
            provider.as_str()
        ),
        None => "no capture provider is configured, so per-console HLS cannot start".to_owned(),
    }
}

fn touch_entry(state: &mut ManagerState, console: u32) {
    if let Some(entry) = state.entries.get_mut(&console) {
        entry.last_used = Instant::now();
    }
}

fn least_recently_used_non_active(state: &ManagerState, active: u32) -> Option<u32> {
    state
        .entries
        .iter()
        .filter(|(index, _)| **index != active)
        .min_by_key(|(_, entry)| entry.last_used)
        .map(|(index, _)| *index)
}

async fn stop_entries(entries: Vec<Entry>) {
    for entry in entries {
        stop_entry(entry).await;
    }
}

async fn stop_entry(entry: Entry) {
    if let Some(pipeline) = entry.pipeline {
        let _ = pipeline.stop().await;
    }
}

fn bounded(message: String) -> String {
    if message.chars().count() <= MAX_STATUS_ERROR_CHARS {
        return message;
    }
    let mut truncated: String = message.chars().take(MAX_STATUS_ERROR_CHARS).collect();
    truncated.push('…');
    truncated
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Mutex as StdMutex;

    use axum::body::Body;
    use axum::http::{Request, StatusCode};
    use tower::ServiceExt;

    use super::*;

    fn temp_dir(label: &str) -> PathBuf {
        static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let serial = COUNTER.fetch_add(1, Ordering::Relaxed);
        let directory = std::env::temp_dir().join(format!(
            "ozma-multi-hls-{label}-{}-{serial}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&directory);
        std::fs::create_dir_all(&directory).expect("create test directory");
        directory
    }

    fn output(index: u32, source: &str) -> DisplayOutput {
        DisplayOutput {
            index,
            name: format!("Console_{index}"),
            source_type: source.to_owned(),
            capture_source_id: String::new(),
            width: 640,
            height: 480,
            primary: index == 0,
        }
    }

    fn selection(active: u32, indexes: &[u32], source: &str) -> DisplayOutputSelection {
        DisplayOutputSelection {
            outputs: indexes.iter().map(|index| output(*index, source)).collect(),
            active_console: active,
            limitation: None,
        }
    }

    struct FakePipeline {
        console: u32,
        manifest: PathBuf,
        running: Arc<AtomicBool>,
        stopped: Arc<StdMutex<Vec<u32>>>,
    }

    #[async_trait]
    impl ConsoleHlsPipeline for FakePipeline {
        fn status(&self) -> HlsStatus {
            if self.running.load(Ordering::SeqCst) {
                HlsStatus::running(self.manifest.clone())
            } else {
                HlsStatus::failed("fake pipeline stopped")
            }
        }

        async fn stop(&self) -> Result<(), String> {
            self.running.store(false, Ordering::SeqCst);
            self.stopped.lock().expect("stops").push(self.console);
            Ok(())
        }

        fn manifest_path(&self) -> PathBuf {
            self.manifest.clone()
        }
    }

    struct FakeFactory {
        provider: Option<VmConsoleBackend>,
        started: Arc<StdMutex<Vec<u32>>>,
        stopped: Arc<StdMutex<Vec<u32>>>,
        fail: Arc<StdMutex<HashSet<u32>>>,
    }

    impl FakeFactory {
        fn new(provider: Option<VmConsoleBackend>) -> Self {
            Self {
                provider,
                started: Arc::new(StdMutex::new(Vec::new())),
                stopped: Arc::new(StdMutex::new(Vec::new())),
                fail: Arc::new(StdMutex::new(HashSet::new())),
            }
        }

        fn started(&self) -> Vec<u32> {
            self.started.lock().expect("started").clone()
        }

        fn stopped(&self) -> Vec<u32> {
            self.stopped.lock().expect("stopped").clone()
        }
    }

    #[async_trait]
    impl ConsoleHlsFactory for FakeFactory {
        fn provider(&self) -> Option<VmConsoleBackend> {
            self.provider
        }

        async fn start(
            &self,
            console: u32,
            config: HlsConfig,
        ) -> Result<Arc<dyn ConsoleHlsPipeline>, String> {
            if self.fail.lock().expect("fail").contains(&console) {
                return Err(format!("fake encoder for Console_{console} failed"));
            }
            std::fs::create_dir_all(&config.output_dir).expect("output dir");
            let manifest = config.manifest_path();
            std::fs::write(&manifest, "#EXTM3U\n#EXT-X-VERSION:3\n").expect("manifest");
            self.started.lock().expect("started").push(console);
            Ok(Arc::new(FakePipeline {
                console,
                manifest,
                running: Arc::new(AtomicBool::new(true)),
                stopped: self.stopped.clone(),
            }))
        }
    }

    fn manager(
        factory: Arc<FakeFactory>,
        root: &Path,
        max: usize,
        idle: Duration,
    ) -> MultiConsoleHls {
        MultiConsoleHls::new(
            factory,
            MultiConsoleHlsConfig {
                max_pipelines: max,
                idle_timeout: idle,
                root: root.to_path_buf(),
                base: HlsConfig {
                    output_dir: root.to_path_buf(),
                    ..HlsConfig::default()
                },
            },
        )
    }

    #[tokio::test]
    async fn starts_two_independent_console_pipelines() {
        let root = temp_dir("two");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Dbus)));
        let manager = manager(factory.clone(), &root, 2, Duration::ZERO);
        manager.sync(&selection(0, &[0, 1], "dbus")).await;

        assert_eq!(factory.started(), vec![0, 1]);
        let status = manager.status().await;
        assert_eq!(status.consoles.len(), 2);
        assert_eq!(status.active_console, 0);
        assert_eq!(status.consoles[0].status, "running");
        assert_eq!(status.consoles[1].status, "running");
        assert!(status.consoles[0].active);
        assert!(!status.consoles[1].active);
        assert_eq!(
            status.consoles[1].playlist.as_deref(),
            Some("/stream/console-1/stream.m3u8")
        );
        assert_ne!(
            status.consoles[0].output_dir, status.consoles[1].output_dir,
            "each console writes to its own directory"
        );
        assert!(manager.manifest_path(1).ends_with("console-1/stream.m3u8"));
    }

    #[tokio::test]
    async fn pipeline_bound_rotates_in_the_requested_console() {
        let root = temp_dir("bound");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Dbus)));
        let manager = manager(factory.clone(), &root, 2, Duration::ZERO);
        manager.sync(&selection(0, &[0, 1, 2], "dbus")).await;

        assert_eq!(factory.started(), vec![0, 1]);
        let status = manager.status().await;
        assert!(status
            .limitation
            .as_deref()
            .is_some_and(|note| note.contains("bounded to 2")));
        assert_eq!(status.consoles[2].status, "not-started");

        assert!(manager.touch(2).await);
        assert_eq!(factory.started(), vec![0, 1, 2]);
        assert!(
            factory.stopped().contains(&1),
            "LRU non-active console is rotated out"
        );
        assert!(
            !factory.stopped().contains(&0),
            "active console is never evicted"
        );
        assert_eq!(manager.status().await.consoles[2].status, "running");
    }

    #[tokio::test]
    async fn single_head_provider_starts_nothing_and_reports_the_typed_limitation() {
        let root = temp_dir("single");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Qmp)));
        let manager = manager(factory.clone(), &root, 2, Duration::ZERO);
        manager.sync(&selection(0, &[0, 1], "qmp")).await;

        assert!(factory.started().is_empty());
        let status = manager.status().await;
        assert!(status
            .limitation
            .as_deref()
            .is_some_and(|note| note.contains("one default console")));
        assert!(status
            .consoles
            .iter()
            .all(|console| console.status == "unsupported"));
        assert!(!manager.touch(1).await);
        assert!(factory.started().is_empty());
    }

    #[tokio::test]
    async fn console_removal_and_shutdown_stop_only_the_affected_pipelines() {
        let root = temp_dir("remove");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Dbus)));
        let manager = manager(factory.clone(), &root, 2, Duration::ZERO);
        manager.sync(&selection(0, &[0, 1], "dbus")).await;
        assert_eq!(factory.started(), vec![0, 1]);

        manager.sync(&selection(0, &[0], "dbus")).await;
        assert_eq!(factory.stopped(), vec![1]);

        manager.shutdown().await;
        assert_eq!(factory.stopped(), vec![1, 0]);
    }

    #[tokio::test]
    async fn active_selection_does_not_restart_independent_pipelines() {
        let root = temp_dir("active");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Dbus)));
        let manager = manager(factory.clone(), &root, 2, Duration::ZERO);
        manager.sync(&selection(0, &[0, 1], "dbus")).await;
        manager.sync(&selection(1, &[0, 1], "dbus")).await;

        assert_eq!(factory.started(), vec![0, 1], "no console was restarted");
        assert!(factory.stopped().is_empty());
        let status = manager.status().await;
        assert_eq!(status.active_console, 1);
        assert!(!status.consoles[0].active);
        assert!(status.consoles[1].active);
    }

    #[tokio::test]
    async fn idle_eviction_keeps_the_active_console() {
        let root = temp_dir("idle");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Dbus)));
        let manager = manager(factory.clone(), &root, 2, Duration::from_secs(1));
        manager.sync(&selection(0, &[0, 1], "dbus")).await;

        let evicted = manager
            .evict_idle(Instant::now() + Duration::from_secs(2))
            .await;
        assert_eq!(evicted, 1);
        assert_eq!(factory.stopped(), vec![1]);
        let status = manager.status().await;
        assert_eq!(status.consoles[0].status, "running");
        assert_eq!(status.consoles[1].status, "not-started");
    }

    #[tokio::test]
    async fn start_failure_is_reported_per_console() {
        let root = temp_dir("failure");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Dbus)));
        factory.fail.lock().expect("fail").insert(1);
        let manager = manager(factory.clone(), &root, 2, Duration::ZERO);
        manager.sync(&selection(0, &[0, 1], "dbus")).await;

        let status = manager.status().await;
        assert_eq!(status.consoles[0].status, "running");
        assert_eq!(status.consoles[1].status, "failed");
        assert!(status.consoles[1]
            .error
            .as_deref()
            .is_some_and(|error| error.contains("fake encoder")));
    }

    #[tokio::test]
    async fn stream_router_serves_active_and_per_console_playlists() {
        let root = temp_dir("router");
        let factory = Arc::new(FakeFactory::new(Some(VmConsoleBackend::Dbus)));
        let manager = manager(factory, &root, 2, Duration::ZERO);
        manager.sync(&selection(0, &[0, 1], "dbus")).await;
        std::fs::write(manager.console_dir(0).join("stream.m3u8"), "active").unwrap();
        std::fs::write(manager.console_dir(1).join("stream.m3u8"), "other").unwrap();

        let app = stream_router(manager.clone());
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/stream/stream.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
            "active"
        );

        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri("/stream/console-1/stream.m3u8")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            axum::body::to_bytes(response.into_body(), usize::MAX)
                .await
                .unwrap(),
            "other"
        );

        for uri in [
            "/stream/console-9/stream.m3u8",
            "/stream/console-1/../console-0/stream.m3u8",
        ] {
            let response = app
                .clone()
                .oneshot(Request::builder().uri(uri).body(Body::empty()).unwrap())
                .await
                .unwrap();
            assert_eq!(response.status(), StatusCode::NOT_FOUND, "{uri}");
        }
    }
}
