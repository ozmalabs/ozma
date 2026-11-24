// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Bounded, true simultaneous per-console WebRTC through the external helper.
//!
//! The default WebRTC path (`--webrtc-helper`) owns one active capture source
//! and one helper process. This module adds an opt-in manager
//! (`--multi-console-webrtc`) that runs one helper per advertised, capturable
//! console:
//!
//! ```text
//! console-0 -> helper 0 -> socket-0 / shm-0 -> /webrtc/offer?console=0
//! console-1 -> helper 1 -> socket-1 / shm-1 -> /webrtc/offer?console=1
//! active    -> the active console's helper (no query keeps the old contract)
//! ```
//!
//! Honesty and bounds:
//!
//! * only D-Bus exposes independent console indexes today; QMP/VNC/Looking
//!   Glass report a typed `unsupported` limitation and start nothing;
//! * at most [`MultiConsoleWebrtcConfig::max_helpers`] helper processes run.
//!   The active console is always preferred; another console rotates in by
//!   stopping the least-recently-used non-active helper;
//! * a non-active helper is stopped after the configured idle timeout without
//!   an offer/candidate for that console (zero disables idle eviction);
//! * selecting a console does not restart the independent helpers: it only
//!   changes which console an offer without a `console`/`id` targets;
//! * every helper gets its own signaling socket and shared-memory framebuffer,
//!   derived from the base `--webrtc-socket`/`--webrtc-shm` paths;
//! * a console removed from `display_outputs` and every helper on shutdown are
//!   stopped with the existing bounded `WebRtcHelper::stop` path.
//!
//! HLS is independent: this manager never touches the HLS pipelines, and a
//! node without WebRTC keeps its HLS-only behaviour.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use ozma_display::vm::{VmConsoleBackend, MAX_VM_CONSOLES};
use serde::Serialize;
use tokio::sync::Mutex;

use crate::display_control::{open_first_source, ConsoleSelectionObserver, ConsoleSourceFactory};
use crate::display_outputs::{DisplayOutput, DisplayOutputSelection};
use crate::webrtc::{WebRtcAnswer, WebRtcConfig, WebRtcError, WebRtcHelper, WebRtcStatus};

/// Default maximum number of simultaneous per-console WebRTC helpers.
pub const DEFAULT_MAX_CONSOLE_WEBRTC: usize = 2;
/// Default idle timeout for a non-active per-console helper.
pub const DEFAULT_CONSOLE_WEBRTC_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
/// Largest status error retained.
const MAX_STATUS_ERROR_CHARS: usize = 512;
/// Bound on remembered helper session ids per console. Old ids are dropped
/// without touching the helper; this only bounds the routing table.
const MAX_SESSIONS_PER_CONSOLE: usize = 64;

/// Bounds for per-console WebRTC.
#[derive(Clone, Debug)]
pub struct MultiConsoleWebrtcConfig {
    /// Maximum simultaneous helpers, including the active console. Clamped to
    /// `1..=MAX_VM_CONSOLES`.
    pub max_helpers: usize,
    /// Stop a non-active helper after this long without an offer/candidate for
    /// that console. Zero disables idle eviction (the `max_helpers` bound still
    /// applies).
    pub idle_timeout: Duration,
}

impl Default for MultiConsoleWebrtcConfig {
    fn default() -> Self {
        Self {
            max_helpers: DEFAULT_MAX_CONSOLE_WEBRTC,
            idle_timeout: DEFAULT_CONSOLE_WEBRTC_IDLE_TIMEOUT,
        }
    }
}

impl MultiConsoleWebrtcConfig {
    fn bounded_max(&self) -> usize {
        self.max_helpers.max(1).min(MAX_VM_CONSOLES as usize)
    }
}

/// One live helper generation, abstracted so tests never spawn GStreamer.
#[async_trait]
pub trait ConsoleWebRtcSession: Send + Sync {
    /// Live helper status.
    fn status(&self) -> WebRtcStatus;
    /// True while the helper socket is connected and the feeder runs.
    fn is_running(&self) -> bool;
    /// Helper process id, when one was spawned.
    fn pid(&self) -> Option<u32>;
    /// Signaling socket for this helper.
    fn socket_path(&self) -> PathBuf;
    /// Shared-memory framebuffer for this helper.
    fn shm_path(&self) -> PathBuf;
    /// Send one SDP offer and return the answer plus optional session id.
    async fn offer_answer(&self, sdp: &str) -> Result<WebRtcAnswer, WebRtcError>;
    /// Forward one trickled remote ICE candidate.
    async fn add_ice_candidate(
        &self,
        session: Option<&str>,
        candidate: &str,
        sdp_mline_index: u32,
    ) -> Result<(), WebRtcError>;
    /// Change this helper's video bitrate.
    async fn set_bitrate(&self, bitrate_kbps: u32) -> Result<(), WebRtcError>;
    /// Stop the helper and its capture source with the helper's bounded wait.
    async fn stop(&self) -> Result<(), String>;
}

/// Injectable factory for per-console helpers.
#[async_trait]
pub trait ConsoleWebRtcFactory: Send + Sync {
    /// Configured capture transport.
    fn provider(&self) -> Option<VmConsoleBackend>;
    /// Configured audio monitor, when the base helper uses one.
    fn audio_source(&self) -> Option<String> {
        None
    }
    /// Start one helper for `console`, using `geometry` when non-zero.
    async fn start(
        &self,
        console: u32,
        geometry: Option<(u32, u32)>,
    ) -> Result<Arc<dyn ConsoleWebRtcSession>, String>;
}

/// Adapter around one [`WebRtcHelper`].
pub struct HelperSession {
    helper: Mutex<WebRtcHelper>,
    status: WebRtcStatus,
    pid: Option<u32>,
    socket_path: PathBuf,
    shm_path: PathBuf,
}

impl HelperSession {
    /// Wrap a started helper.
    pub fn new(helper: WebRtcHelper) -> Self {
        let status = helper.status();
        let pid = status.helper_pid();
        let socket_path = helper.config().socket_path.clone();
        let shm_path = helper.config().shm_path.clone();
        Self {
            helper: Mutex::new(helper),
            status,
            pid,
            socket_path,
            shm_path,
        }
    }
}

#[async_trait]
impl ConsoleWebRtcSession for HelperSession {
    fn status(&self) -> WebRtcStatus {
        self.status.clone()
    }

    fn is_running(&self) -> bool {
        self.status.running()
    }

    fn pid(&self) -> Option<u32> {
        self.pid
    }

    fn socket_path(&self) -> PathBuf {
        self.socket_path.clone()
    }

    fn shm_path(&self) -> PathBuf {
        self.shm_path.clone()
    }

    async fn offer_answer(&self, sdp: &str) -> Result<WebRtcAnswer, WebRtcError> {
        self.helper.lock().await.offer_answer(sdp).await
    }

    async fn add_ice_candidate(
        &self,
        session: Option<&str>,
        candidate: &str,
        sdp_mline_index: u32,
    ) -> Result<(), WebRtcError> {
        self.helper
            .lock()
            .await
            .add_ice_candidate(session, candidate, sdp_mline_index)
            .await
    }

    async fn set_bitrate(&self, bitrate_kbps: u32) -> Result<(), WebRtcError> {
        self.helper.lock().await.set_bitrate(bitrate_kbps).await
    }

    async fn stop(&self) -> Result<(), String> {
        self.helper
            .lock()
            .await
            .stop()
            .await
            .map_err(|error| error.to_string())
    }
}

/// Production factory that opens the console's capture source and spawns the
/// configured helper with per-console socket/shm paths.
pub struct ControllerWebRtcFactory {
    console_factory: Arc<dyn ConsoleSourceFactory>,
    base: WebRtcConfig,
}

impl ControllerWebRtcFactory {
    /// Build a factory for a validated base helper configuration.
    pub fn new(console_factory: Arc<dyn ConsoleSourceFactory>, base: WebRtcConfig) -> Self {
        Self {
            console_factory,
            base,
        }
    }

    /// Configuration one console's helper runs with.
    pub fn config_for(&self, console: u32, geometry: Option<(u32, u32)>) -> WebRtcConfig {
        let mut config = self.base.clone();
        config.socket_path = console_path(&self.base.socket_path, console);
        config.shm_path = console_path(&self.base.shm_path, console);
        if let Some((width, height)) = geometry {
            if width > 0 && height > 0 {
                config.width = width;
                config.height = height;
            }
        }
        config
    }
}

#[async_trait]
impl ConsoleWebRtcFactory for ControllerWebRtcFactory {
    fn provider(&self) -> Option<VmConsoleBackend> {
        self.console_factory.provider()
    }

    fn audio_source(&self) -> Option<String> {
        self.base.audio_source.clone()
    }

    async fn start(
        &self,
        console: u32,
        geometry: Option<(u32, u32)>,
    ) -> Result<Arc<dyn ConsoleWebRtcSession>, String> {
        let factory = self.console_factory.clone();
        let config = self.config_for(console, geometry);
        tokio::task::spawn_blocking(move || {
            let registry = factory.registry(console)?;
            let source = open_first_source(&registry)?;
            let helper = WebRtcHelper::start(source, config).map_err(|error| error.to_string())?;
            Ok(Arc::new(HelperSession::new(helper)) as Arc<dyn ConsoleWebRtcSession>)
        })
        .await
        .map_err(|error| format!("starting the WebRTC helper task failed: {error}"))?
    }
}

struct Entry {
    session: Option<Arc<dyn ConsoleWebRtcSession>>,
    error: Option<String>,
    last_used: Instant,
    /// Successful offers answered by this helper since it started.
    peers: u64,
}

struct ManagerState {
    selection: DisplayOutputSelection,
    entries: HashMap<u32, Entry>,
    /// Helper session id -> console. Used to route `/webrtc/candidate`.
    sessions: HashMap<String, u32>,
    limitation: Option<String>,
}

struct Inner {
    factory: Arc<dyn ConsoleWebRtcFactory>,
    config: MultiConsoleWebrtcConfig,
    state: Mutex<ManagerState>,
}

/// Per-console WebRTC status for one advertised console.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ConsoleWebRtcStatus {
    /// Zero-based `Console_N` index.
    pub console: u32,
    /// True when this console is the active WebRTC console.
    pub active: bool,
    /// `running`, `starting`, `failed`, `not-started`, or `unsupported`.
    pub status: String,
    /// Why the helper is not running, when it is configured and failing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    /// Helper process id, when one was spawned.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    /// Signaling socket for this console's helper (local diagnostics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub socket_path: Option<String>,
    /// Shared-memory framebuffer for this console's helper (local diagnostics).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub shm_path: Option<String>,
    /// Successful offers answered by this helper since it started.
    pub peers: u64,
    /// Helper session ids currently routed to this console.
    pub sessions: usize,
}

/// Per-console WebRTC summary for `/api/v1/connection`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct MultiConsoleWebrtcStatus {
    /// The manager is enabled on this node.
    pub enabled: bool,
    /// Capture transport, when one is configured.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider: Option<String>,
    /// Maximum simultaneous helpers.
    pub max_helpers: usize,
    /// Idle timeout applied to non-active helpers (zero = disabled).
    pub idle_timeout_seconds: u64,
    /// Active WebRTC console.
    pub active_console: u32,
    /// Why some advertised consoles are not served (provider/bound/transport).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limitation: Option<String>,
    /// Per-console helper state.
    pub consoles: Vec<ConsoleWebRtcStatus>,
}

/// Bounded manager for independent per-console WebRTC helpers.
#[derive(Clone)]
pub struct MultiConsoleWebrtc {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for MultiConsoleWebrtc {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("MultiConsoleWebrtc")
            .field("max_helpers", &self.inner.config.max_helpers)
            .field("idle_timeout", &self.inner.config.idle_timeout)
            .finish_non_exhaustive()
    }
}

impl MultiConsoleWebrtc {
    /// Build a manager for an injectable factory (tests/embedding).
    pub fn new(factory: Arc<dyn ConsoleWebRtcFactory>, config: MultiConsoleWebrtcConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                factory,
                config: MultiConsoleWebrtcConfig {
                    max_helpers: config.bounded_max(),
                    ..config
                },
                state: Mutex::new(ManagerState {
                    selection: DisplayOutputSelection::empty(),
                    entries: HashMap::new(),
                    sessions: HashMap::new(),
                    limitation: None,
                }),
            }),
        }
    }

    /// Build a manager that starts real helpers from the node's console source
    /// factory and base `--webrtc-*` configuration.
    pub fn from_console_factory(
        console_factory: Arc<dyn ConsoleSourceFactory>,
        base: WebRtcConfig,
        config: MultiConsoleWebrtcConfig,
    ) -> Self {
        Self::new(
            Arc::new(ControllerWebRtcFactory::new(console_factory, base)),
            config,
        )
    }

    /// True when the configured provider can expose independent consoles.
    pub fn supports_independent_consoles(&self) -> bool {
        matches!(self.inner.factory.provider(), Some(VmConsoleBackend::Dbus))
    }

    /// Configured audio monitor (configuration intent only).
    pub fn audio_source(&self) -> Option<String> {
        self.inner.factory.audio_source()
    }

    /// True when the base helper was configured with an audio monitor.
    pub fn audio_configured(&self) -> bool {
        self.audio_source().is_some()
    }

    /// Active console, or `0` when no outputs are advertised.
    pub async fn active_console(&self) -> u32 {
        self.inner.state.lock().await.selection.active_console
    }

    /// Resolve a `console`/`id` selector against the advertised outputs.
    ///
    /// Accepts a numeric index, `Console_<N>`, `console-<N>`, a console label,
    /// or the console's advertised capture source id.
    pub async fn resolve_console(&self, value: &str) -> Result<u32, String> {
        let value = value.trim();
        if let Some(index) = parse_console_token(value) {
            return Ok(index);
        }
        let state = self.inner.state.lock().await;
        state
            .selection
            .outputs
            .iter()
            .find(|output| {
                output.name.eq_ignore_ascii_case(value)
                    || (!output.capture_source_id.is_empty() && output.capture_source_id == value)
            })
            .map(|output| output.index)
            .ok_or_else(|| format!("`{value}` is not an advertised console"))
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

    /// Number of helpers currently starting/running.
    pub async fn running_helper_count(&self) -> usize {
        self.inner
            .state
            .lock()
            .await
            .entries
            .values()
            .filter(|entry| {
                entry
                    .session
                    .as_ref()
                    .is_some_and(|session| session.is_running())
            })
            .count()
    }

    /// True when at least one helper is starting/running.
    pub async fn has_running_helpers(&self) -> bool {
        self.running_helper_count().await > 0
    }

    /// Reconcile the advertised consoles with the running helpers.
    pub async fn sync(&self, selection: &DisplayOutputSelection) {
        let mut state = self.inner.state.lock().await;
        state.selection = selection.clone();

        let provider = self.inner.factory.provider();
        if !matches!(provider, Some(VmConsoleBackend::Dbus)) {
            let removed: Vec<Entry> = state.entries.drain().map(|(_, entry)| entry).collect();
            state.sessions.clear();
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
                 per-console WebRTC helper",
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
        let max = self.inner.config.max_helpers;
        if desired.len() > max {
            notes.push(format!(
                "{} advertised consoles have a live capture source, but per-console WebRTC \
                 is bounded to {max} helper(s); request /webrtc/offer?console=<N> to rotate a \
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
                .filter_map(|index| {
                    remove_sessions_for(&mut state, index);
                    state.entries.remove(&index)
                })
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
                .map(|entry| entry.session.is_none())
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
                    remove_sessions_for(&mut state, victim);
                    if let Some(entry) = state.entries.remove(&victim) {
                        stop_entry(entry).await;
                    }
                }
            }

            let geometry = selection
                .outputs
                .iter()
                .find(|output| output.index == console)
                .map(|output| (output.width, output.height))
                .filter(|(width, height)| *width > 0 && *height > 0);
            let result = self.inner.factory.start(console, geometry).await;
            let now = Instant::now();
            match result {
                Ok(session) => {
                    state.entries.insert(
                        console,
                        Entry {
                            session: Some(session),
                            error: None,
                            last_used: now,
                            peers: 0,
                        },
                    );
                }
                Err(error) => {
                    state.entries.insert(
                        console,
                        Entry {
                            session: None,
                            error: Some(bounded(error)),
                            last_used: now,
                            peers: 0,
                        },
                    );
                }
            }
        }
        touch_entry(&mut state, selection.active_console);
    }

    /// Resolve and start (if needed) one console's helper.
    async fn ensure_entry(
        &self,
        state: &mut ManagerState,
        console: u32,
    ) -> Result<Arc<dyn ConsoleWebRtcSession>, WebRtcError> {
        let provider = self.inner.factory.provider();
        if !matches!(provider, Some(VmConsoleBackend::Dbus)) {
            return Err(WebRtcError::Unsupported(unsupported_message(provider)));
        }
        let output = state
            .selection
            .outputs
            .iter()
            .find(|output| output.index == console)
            .cloned()
            .ok_or_else(|| {
                WebRtcError::InvalidConfig(format!(
                    "Console_{console} is not advertised by this node"
                ))
            })?;
        if !output_is_capturable(&output, provider) {
            return Err(WebRtcError::Unsupported(unsupported_message(provider)));
        }

        let existing = state
            .entries
            .get(&console)
            .and_then(|entry| entry.session.clone())
            .filter(|session| session.is_running());
        if let Some(session) = existing {
            touch_entry(state, console);
            return Ok(session);
        }

        let max = self.inner.config.max_helpers;
        let missing = state
            .entries
            .get(&console)
            .map(|entry| entry.session.is_none())
            .unwrap_or(true);
        if missing && state.entries.len() >= max {
            let active = state.selection.active_console;
            if let Some(victim) = least_recently_used_non_active(state, active) {
                remove_sessions_for(state, victim);
                if let Some(entry) = state.entries.remove(&victim) {
                    stop_entry(entry).await;
                }
            } else if !state.entries.contains_key(&console) {
                return Err(WebRtcError::NotRunning);
            }
        }

        let geometry =
            (output.width > 0 && output.height > 0).then_some((output.width, output.height));
        let result = self.inner.factory.start(console, geometry).await;
        let now = Instant::now();
        match result {
            Ok(session) => {
                state.entries.insert(
                    console,
                    Entry {
                        session: Some(session.clone()),
                        error: None,
                        last_used: now,
                        peers: 0,
                    },
                );
                Ok(session)
            }
            Err(error) => {
                let message = bounded(error);
                state.entries.insert(
                    console,
                    Entry {
                        session: None,
                        error: Some(message.clone()),
                        last_used: now,
                        peers: 0,
                    },
                );
                Err(WebRtcError::NotRunning)
            }
        }
    }

    async fn target_console(&self, requested: Option<u32>) -> Result<u32, WebRtcError> {
        let state = self.inner.state.lock().await;
        if state.selection.outputs.is_empty() {
            return Err(WebRtcError::NotRunning);
        }
        match requested {
            Some(console) => {
                if state
                    .selection
                    .outputs
                    .iter()
                    .any(|output| output.index == console)
                {
                    Ok(console)
                } else {
                    Err(WebRtcError::InvalidConfig(format!(
                        "Console_{console} is not advertised by this node"
                    )))
                }
            }
            None => Ok(state.selection.active_console),
        }
    }

    /// Proxy one offer to the requested console (or the active console).
    pub async fn offer(
        &self,
        requested: Option<u32>,
        sdp: &str,
    ) -> Result<WebRtcAnswer, WebRtcError> {
        if sdp.trim().is_empty() {
            return Err(WebRtcError::InvalidConfig(
                "the WebRTC offer SDP must not be empty".to_owned(),
            ));
        }
        let console = self.target_console(requested).await?;
        let session = {
            let mut state = self.inner.state.lock().await;
            self.ensure_entry(&mut state, console).await?
        };
        let answer = session.offer_answer(sdp).await?;
        {
            let mut state = self.inner.state.lock().await;
            let same_helper = state
                .entries
                .get(&console)
                .and_then(|entry| entry.session.as_ref())
                .is_some_and(|current| Arc::ptr_eq(current, &session));
            if same_helper {
                if let Some(entry) = state.entries.get_mut(&console) {
                    entry.last_used = Instant::now();
                    entry.peers = entry.peers.saturating_add(1);
                }
                if let Some(id) = answer.session.as_ref() {
                    prune_sessions(&mut state, console);
                    state.sessions.insert(id.clone(), console);
                }
            }
        }
        Ok(answer)
    }

    /// Forward one trickled candidate by console or helper session id.
    pub async fn candidate(
        &self,
        requested: Option<u32>,
        session_id: Option<&str>,
        candidate: &str,
        sdp_mline_index: u32,
    ) -> Result<(), WebRtcError> {
        let session = {
            let mut state = self.inner.state.lock().await;
            let console = match requested {
                Some(console) => {
                    if !state
                        .selection
                        .outputs
                        .iter()
                        .any(|output| output.index == console)
                    {
                        return Err(WebRtcError::InvalidConfig(format!(
                            "Console_{console} is not advertised by this node"
                        )));
                    }
                    console
                }
                None => match session_id.and_then(|id| state.sessions.get(id).copied()) {
                    Some(console) => console,
                    None => {
                        if let Some(id) = session_id {
                            return Err(WebRtcError::InvalidConfig(format!(
                                "unknown WebRTC session `{id}`"
                            )));
                        }
                        state.selection.active_console
                    }
                },
            };
            let entry = state
                .entries
                .get_mut(&console)
                .ok_or_else(|| WebRtcError::NotRunning)?;
            entry.last_used = Instant::now();
            entry.session.clone().ok_or(WebRtcError::NotRunning)?
        };
        session
            .add_ice_candidate(session_id, candidate, sdp_mline_index)
            .await
    }

    /// Change the requested console's bitrate (or the active console's).
    pub async fn set_bitrate(
        &self,
        requested: Option<u32>,
        bitrate_kbps: u32,
    ) -> Result<(), WebRtcError> {
        if bitrate_kbps == 0 {
            return Err(WebRtcError::InvalidConfig(
                "bitrate must be greater than zero".to_owned(),
            ));
        }
        let console = self.target_console(requested).await?;
        let session = {
            let mut state = self.inner.state.lock().await;
            let entry = state
                .entries
                .get_mut(&console)
                .ok_or(WebRtcError::NotRunning)?;
            entry.last_used = Instant::now();
            entry.session.clone().ok_or(WebRtcError::NotRunning)?
        };
        session.set_bitrate(bitrate_kbps).await
    }

    /// Console that owns a helper session id, when known.
    pub async fn console_for_session(&self, session: &str) -> Option<u32> {
        self.inner.state.lock().await.sessions.get(session).copied()
    }

    /// Mark a console as recently used, starting it if it was evicted.
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
        self.ensure_entry(&mut state, console).await.is_ok()
    }

    /// Stop non-active helpers idle for at least the configured timeout.
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
            .filter_map(|index| {
                remove_sessions_for(&mut state, index);
                state.entries.remove(&index)
            })
            .collect();
        let count = entries.len();
        drop(state);
        stop_entries(entries).await;
        count
    }

    /// Stop one console helper without removing the console from the
    /// advertised set. Used by tests and future add/remove control surfaces.
    pub async fn stop_console(&self, console: u32) -> bool {
        let mut state = self.inner.state.lock().await;
        remove_sessions_for(&mut state, console);
        let removed = state.entries.remove(&console);
        drop(state);
        match removed {
            Some(entry) => {
                stop_entry(entry).await;
                true
            }
            None => false,
        }
    }

    /// Stop every helper. Called during node shutdown.
    pub async fn shutdown(&self) {
        let entries: Vec<Entry> = {
            let mut state = self.inner.state.lock().await;
            state.sessions.clear();
            state.entries.drain().map(|(_, entry)| entry).collect()
        };
        stop_entries(entries).await;
    }

    /// Active console status in the shape `/api/v1/connection.webrtc` uses.
    pub async fn active_status(&self) -> WebRtcStatus {
        let state = self.inner.state.lock().await;
        if state.selection.outputs.is_empty() {
            return WebRtcStatus::not_configured();
        }
        let provider = self.inner.factory.provider();
        if !matches!(provider, Some(VmConsoleBackend::Dbus)) {
            return WebRtcStatus::failed(unsupported_message(provider));
        }
        let active = state.selection.active_console;
        match state.entries.get(&active) {
            Some(entry) => match entry.session.as_ref() {
                Some(session) => session.status(),
                None => WebRtcStatus::failed(entry.error.clone().unwrap_or_else(|| {
                    format!("per-console WebRTC for Console_{active} is not running")
                })),
            },
            None => WebRtcStatus::failed(format!(
                "per-console WebRTC for the active Console_{active} is not running"
            )),
        }
    }

    /// Status for every advertised console.
    pub async fn status(&self) -> MultiConsoleWebrtcStatus {
        let state = self.inner.state.lock().await;
        let provider = self.inner.factory.provider();
        let supported = matches!(provider, Some(VmConsoleBackend::Dbus));
        let active = state.selection.active_console;
        let mut consoles = Vec::with_capacity(state.selection.outputs.len());

        for output in &state.selection.outputs {
            let capturable = supported && output_is_capturable(output, provider);
            let entry = state.entries.get(&output.index);
            let sessions = state
                .sessions
                .values()
                .filter(|console| **console == output.index)
                .count();
            let (status, error, pid, socket_path, shm_path, peers) = if !capturable {
                (
                    "unsupported".to_owned(),
                    Some(unsupported_message(provider)),
                    None,
                    None,
                    None,
                    0,
                )
            } else if let Some(entry) = entry {
                match entry.session.as_ref() {
                    Some(session) => {
                        let status = session.status();
                        (
                            status.status_name().to_owned(),
                            status.error(),
                            session.pid(),
                            Some(session.socket_path().display().to_string()),
                            Some(session.shm_path().display().to_string()),
                            entry.peers,
                        )
                    }
                    None => (
                        "failed".to_owned(),
                        entry.error.clone().or_else(|| {
                            Some(format!(
                                "per-console WebRTC for Console_{} is not running",
                                output.index
                            ))
                        }),
                        None,
                        None,
                        None,
                        0,
                    ),
                }
            } else {
                (
                    "not-started".to_owned(),
                    Some(
                        "not started because the per-console WebRTC bound was reached; request \
                         /webrtc/offer?console=<N> to rotate it in"
                            .to_owned(),
                    ),
                    None,
                    None,
                    None,
                    0,
                )
            };
            consoles.push(ConsoleWebRtcStatus {
                console: output.index,
                active: output.index == active,
                status,
                error,
                pid,
                socket_path,
                shm_path,
                peers,
                sessions,
            });
        }

        MultiConsoleWebrtcStatus {
            enabled: true,
            provider: provider.map(|provider| provider.as_str().to_owned()),
            max_helpers: self.inner.config.max_helpers,
            idle_timeout_seconds: self.inner.config.idle_timeout.as_secs(),
            active_console: active,
            limitation: state.limitation.clone(),
            consoles,
        }
    }

    /// Status for one console, when it is advertised.
    pub async fn status_for(&self, console: u32) -> Option<ConsoleWebRtcStatus> {
        self.status()
            .await
            .consoles
            .into_iter()
            .find(|entry| entry.console == console)
    }
}

#[async_trait]
impl ConsoleSelectionObserver for MultiConsoleWebrtc {
    async fn on_selection(&self, selection: &DisplayOutputSelection) {
        self.sync(selection).await;
    }
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
             per-console WebRTC helpers; use the single active WebRTC stream and POST \
             /display/select",
            provider.as_str()
        ),
        None => "no capture provider is configured, so per-console WebRTC cannot start".to_owned(),
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

fn remove_sessions_for(state: &mut ManagerState, console: u32) {
    state.sessions.retain(|_, owner| *owner != console);
}

fn prune_sessions(state: &mut ManagerState, console: u32) {
    let count = state
        .sessions
        .values()
        .filter(|owner| **owner == console)
        .count();
    if count < MAX_SESSIONS_PER_CONSOLE {
        return;
    }
    if let Some(id) = state
        .sessions
        .iter()
        .find(|(_, owner)| **owner == console)
        .map(|(id, _)| id.clone())
    {
        state.sessions.remove(&id);
    }
}

async fn stop_entries(entries: Vec<Entry>) {
    for entry in entries {
        stop_entry(entry).await;
    }
}

async fn stop_entry(entry: Entry) {
    if let Some(session) = entry.session {
        let _ = session.stop().await;
    }
}

fn parse_console_token(value: &str) -> Option<u32> {
    let trimmed = value.trim();
    if let Ok(index) = trimmed.parse::<u32>() {
        return Some(index);
    }
    let rest = trimmed
        .strip_prefix("console-")
        .or_else(|| trimmed.strip_prefix("console_"))
        .or_else(|| trimmed.strip_prefix("Console_"))
        .or_else(|| trimmed.strip_prefix("CONSOLE_"))?;
    rest.parse::<u32>().ok()
}

fn bounded(message: String) -> String {
    if message.chars().count() <= MAX_STATUS_ERROR_CHARS {
        return message;
    }
    let mut truncated: String = message.chars().take(MAX_STATUS_ERROR_CHARS).collect();
    truncated.push('…');
    truncated
}

/// Derive a per-console path from a base file path.
///
/// `.../signal.sock` becomes `.../signal-console-1.sock`; a path without an
/// extension becomes `.../frame-console-1`.
pub fn console_path(base: &Path, console: u32) -> PathBuf {
    let stem = base
        .file_stem()
        .map(|value| value.to_string_lossy().into_owned())
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "webrtc".to_owned());
    let name = match base.extension() {
        Some(extension) if !extension.is_empty() => {
            format!("{stem}-console-{console}.{}", extension.to_string_lossy())
        }
        _ => format!("{stem}-console-{console}"),
    };
    base.with_file_name(name)
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::sync::Mutex as StdMutex;

    use super::*;
    use crate::display_outputs::outputs_from_specs;

    fn selection(active: u32, consoles: &[u32]) -> DisplayOutputSelection {
        let specs: Vec<String> = consoles
            .iter()
            .map(|index| {
                format!(
                    "{index}:Console_{index}:{w}x{h}:{}:dbus",
                    if *index == active { "primary" } else { "" },
                    w = 1280 - (*index as u32 * 160),
                    h = 800 - (*index as u32 * 100),
                )
            })
            .collect();
        outputs_from_specs(&specs, VmConsoleBackend::Dbus, None, Some(active)).expect("selection")
    }

    struct FakeSession {
        status: WebRtcStatus,
        running: AtomicBool,
        stopped: Arc<AtomicUsize>,
        offers: AtomicUsize,
        socket: PathBuf,
        shm: PathBuf,
        sessions: StdMutex<Vec<String>>,
    }

    impl FakeSession {
        fn new(console: u32, stopped: Arc<AtomicUsize>) -> Arc<Self> {
            let status = WebRtcStatus::starting_for_test();
            status.mark_running_for_test(Some(9000 + console));
            Arc::new(Self {
                status,
                running: AtomicBool::new(true),
                stopped,
                offers: AtomicUsize::new(0),
                socket: PathBuf::from(format!("/tmp/fake-console-{console}.sock")),
                shm: PathBuf::from(format!("/tmp/fake-console-{console}.bgra")),
                sessions: StdMutex::new(Vec::new()),
            })
        }
    }

    #[async_trait]
    impl ConsoleWebRtcSession for FakeSession {
        fn status(&self) -> WebRtcStatus {
            self.status.clone()
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }

        fn pid(&self) -> Option<u32> {
            self.status.helper_pid()
        }

        fn socket_path(&self) -> PathBuf {
            self.socket.clone()
        }

        fn shm_path(&self) -> PathBuf {
            self.shm.clone()
        }

        async fn offer_answer(&self, _sdp: &str) -> Result<WebRtcAnswer, WebRtcError> {
            let offer = self.offers.fetch_add(1, Ordering::SeqCst);
            let session = format!("session-{}-{offer}", self.socket.display());
            self.sessions
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(session.clone());
            Ok(WebRtcAnswer {
                sdp: format!("answer for {}", self.socket.display()),
                session: Some(session),
            })
        }

        async fn add_ice_candidate(
            &self,
            _session: Option<&str>,
            _candidate: &str,
            _sdp_mline_index: u32,
        ) -> Result<(), WebRtcError> {
            Ok(())
        }

        async fn set_bitrate(&self, _bitrate_kbps: u32) -> Result<(), WebRtcError> {
            Ok(())
        }

        async fn stop(&self) -> Result<(), String> {
            self.running.store(false, Ordering::SeqCst);
            self.stopped.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    struct FakeFactory {
        provider: Option<VmConsoleBackend>,
        started: StdMutex<Vec<u32>>,
        stopped: Arc<AtomicUsize>,
        fail: StdMutex<HashSet<u32>>,
    }

    impl FakeFactory {
        fn dbus() -> Arc<Self> {
            Arc::new(Self {
                provider: Some(VmConsoleBackend::Dbus),
                started: StdMutex::new(Vec::new()),
                stopped: Arc::new(AtomicUsize::new(0)),
                fail: StdMutex::new(HashSet::new()),
            })
        }

        fn single_head() -> Arc<Self> {
            Arc::new(Self {
                provider: Some(VmConsoleBackend::Qmp),
                started: StdMutex::new(Vec::new()),
                stopped: Arc::new(AtomicUsize::new(0)),
                fail: StdMutex::new(HashSet::new()),
            })
        }

        fn started(&self) -> Vec<u32> {
            self.started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl ConsoleWebRtcFactory for FakeFactory {
        fn provider(&self) -> Option<VmConsoleBackend> {
            self.provider
        }

        async fn start(
            &self,
            console: u32,
            _geometry: Option<(u32, u32)>,
        ) -> Result<Arc<dyn ConsoleWebRtcSession>, String> {
            if self
                .fail
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .contains(&console)
            {
                return Err(format!("fake helper for Console_{console} failed"));
            }
            self.started
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(console);
            Ok(FakeSession::new(console, self.stopped.clone()) as Arc<dyn ConsoleWebRtcSession>)
        }
    }

    fn manager(
        factory: Arc<FakeFactory>,
        max_helpers: usize,
        idle: Duration,
    ) -> MultiConsoleWebrtc {
        MultiConsoleWebrtc::new(
            factory,
            MultiConsoleWebrtcConfig {
                max_helpers,
                idle_timeout: idle,
            },
        )
    }

    #[tokio::test]
    async fn two_consoles_offer_simultaneously() {
        let factory = FakeFactory::dbus();
        let manager = manager(factory.clone(), 2, Duration::from_secs(60));
        manager.sync(&selection(0, &[0, 1])).await;

        let (zero, one) = tokio::join!(
            manager.offer(Some(0), "v=0\r\n"),
            manager.offer(Some(1), "v=0\r\n"),
        );
        let zero = zero.expect("console 0 answer");
        let one = one.expect("console 1 answer");
        assert!(zero.sdp.contains("console-0"));
        assert!(one.sdp.contains("console-1"));
        assert_eq!(manager.running_helper_count().await, 2);
        assert_eq!(
            manager
                .console_for_session(zero.session.as_deref().unwrap())
                .await,
            Some(0)
        );
        assert_eq!(
            manager
                .console_for_session(one.session.as_deref().unwrap())
                .await,
            Some(1)
        );
        let status = manager.status().await;
        assert!(status.enabled);
        assert_eq!(status.max_helpers, 2);
        assert_eq!(status.consoles.len(), 2);
        assert!(status
            .consoles
            .iter()
            .all(|console| console.status == "running"));
        assert!(status.consoles.iter().all(|console| console.peers == 1));
        assert!(status.consoles.iter().all(|console| console.sessions == 1));

        manager.shutdown().await;
        assert_eq!(factory.stopped.load(Ordering::SeqCst), 2);
        assert_eq!(manager.running_helper_count().await, 0);
    }

    #[tokio::test]
    async fn max_bound_rotates_least_recently_used_non_active() {
        let factory = FakeFactory::dbus();
        let manager = manager(factory, 2, Duration::from_secs(60));
        manager.sync(&selection(0, &[0, 1, 2])).await;
        assert_eq!(manager.running_helper_count().await, 2);
        assert_eq!(manager.status().await.consoles.len(), 3);
        assert_eq!(manager.status().await.consoles[2].status, "not-started");

        // Console 2 rotates in; console 1 is the least-recently-used non-active.
        manager.offer(Some(2), "v=0\r\n").await.expect("console 2");
        assert_eq!(manager.running_helper_count().await, 2);
        let status = manager.status().await;
        assert_eq!(status.consoles[0].status, "running");
        assert_eq!(status.consoles[1].status, "not-started");
        assert_eq!(status.consoles[2].status, "running");
    }

    #[tokio::test]
    async fn console_selection_and_default_active() {
        let factory = FakeFactory::dbus();
        let manager = manager(factory, 2, Duration::from_secs(60));
        manager.sync(&selection(0, &[0, 1])).await;

        let active = manager.offer(None, "v=0\r\n").await.expect("active");
        assert!(active.sdp.contains("console-0"));
        let selected = manager.offer(Some(1), "v=0\r\n").await.expect("console 1");
        assert!(selected.sdp.contains("console-1"));

        // Selecting console 1 changes what a bare offer targets, without
        // restarting either helper.
        manager.sync(&selection(1, &[0, 1])).await;
        assert_eq!(manager.active_console().await, 1);
        assert_eq!(
            manager
                .status()
                .await
                .consoles
                .iter()
                .find(|console| console.console == 1)
                .unwrap()
                .peers,
            1
        );
        let after = manager.offer(None, "v=0\r\n").await.expect("new active");
        assert!(after.sdp.contains("console-1"));
    }

    #[tokio::test]
    async fn add_remove_and_stop_cleanup() {
        let factory = FakeFactory::dbus();
        let manager = manager(factory.clone(), 2, Duration::from_secs(60));
        manager.sync(&selection(0, &[0, 1])).await;
        let answer = manager.offer(Some(1), "v=0\r\n").await.expect("console 1");
        let session = answer.session.expect("session");
        assert_eq!(manager.console_for_session(&session).await, Some(1));

        // Removing console 1 stops its helper and forgets its sessions.
        manager.sync(&selection(0, &[0])).await;
        assert_eq!(manager.running_helper_count().await, 1);
        assert_eq!(manager.console_for_session(&session).await, None);
        assert_eq!(factory.stopped.load(Ordering::SeqCst), 1);

        assert!(manager.stop_console(0).await);
        assert_eq!(manager.running_helper_count().await, 0);
        assert!(!manager.stop_console(0).await);
    }

    #[tokio::test]
    async fn single_head_is_typed_unsupported() {
        let factory = FakeFactory::single_head();
        let manager = manager(factory.clone(), 2, Duration::from_secs(60));
        manager.sync(&selection(0, &[0])).await;
        assert!(!manager.supports_independent_consoles());
        assert!(factory.started().is_empty());
        let status = manager.status().await;
        assert_eq!(status.consoles[0].status, "unsupported");
        assert!(status.limitation.unwrap().contains("per-console WebRTC"));
        match manager.offer(Some(0), "v=0\r\n").await {
            Err(WebRtcError::Unsupported(message)) => {
                assert!(message.contains("per-console WebRTC"));
            }
            other => panic!("expected typed unsupported, got {other:?}"),
        }
        assert_eq!(manager.active_status().await.status_name(), "failed");
    }

    #[tokio::test]
    async fn idle_eviction_and_per_console_status() {
        let factory = FakeFactory::dbus();
        let manager = manager(factory, 2, Duration::from_secs(10));
        manager.sync(&selection(0, &[0, 1])).await;
        manager.offer(Some(1), "v=0\r\n").await.expect("console 1");

        let status = manager.status().await;
        let one = status
            .consoles
            .iter()
            .find(|console| console.console == 1)
            .unwrap();
        assert!(!one.active);
        assert_eq!(one.status, "running");
        assert_eq!(one.pid, Some(9001));
        assert_eq!(one.sessions, 1);
        assert!(one.socket_path.as_deref().unwrap().contains("console-1"));
        assert!(one.shm_path.as_deref().unwrap().contains("console-1"));

        // The active console is never idle-evicted; console 1 is.
        std::thread::sleep(Duration::from_millis(5));
        let later = Instant::now() + Duration::from_secs(30);
        assert_eq!(manager.evict_idle(later).await, 1);
        let status = manager.status().await;
        assert_eq!(
            status
                .consoles
                .iter()
                .find(|console| console.console == 0)
                .unwrap()
                .status,
            "running"
        );
        assert_eq!(
            status
                .consoles
                .iter()
                .find(|console| console.console == 1)
                .unwrap()
                .status,
            "not-started"
        );
    }

    #[tokio::test]
    async fn candidate_and_bitrate_route_by_console_and_session() {
        let factory = FakeFactory::dbus();
        let manager = manager(factory, 2, Duration::from_secs(60));
        manager.sync(&selection(0, &[0, 1])).await;
        let answer = manager.offer(Some(1), "v=0\r\n").await.expect("console 1");
        let session = answer.session.unwrap();
        manager
            .candidate(
                None,
                Some(&session),
                "candidate:1 1 UDP 1 127.0.0.1 9 typ host",
                0,
            )
            .await
            .expect("session routing");
        manager
            .candidate(Some(0), None, "candidate:1 1 UDP 1 127.0.0.1 9 typ host", 0)
            .await
            .expect("explicit routing");
        manager.set_bitrate(Some(1), 2_000).await.expect("bitrate");
        assert!(manager
            .candidate(
                None,
                Some("missing"),
                "candidate:1 1 UDP 1 127.0.0.1 9 typ host",
                0
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn console_path_derives_siblings() {
        assert_eq!(
            console_path(Path::new("/tmp/run/signal.sock"), 2),
            PathBuf::from("/tmp/run/signal-console-2.sock")
        );
        assert_eq!(
            console_path(Path::new("/dev/shm/frame"), 0),
            PathBuf::from("/dev/shm/frame-console-0")
        );
    }
}
