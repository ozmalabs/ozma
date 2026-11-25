// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Runtime VM console selection.
//!
//! Registration advertises every `Console_N` in `display_outputs`, and QEMU's
//! D-Bus display interface can attach to any of them. Before this module the
//! only way to change which console had a live capture source was to restart
//! the node with a different `--capture-console`.
//!
//! [`DisplayControl`] keeps the active console in shared state and performs a
//! bounded transaction on selection:
//!
//! 1. validate the requested index against the advertised outputs;
//! 2. reject providers that expose one default head (QMP, VNC, Looking Glass);
//! 3. rebuild the capture registry for the selected `Console_N`;
//! 4. restart the running HLS and WebRTC pipelines from the new registry;
//! 5. publish the new active console and capture source id.
//!
//! A pipeline failure stops the transaction and restores the previous registry,
//! console, and pipelines where possible. The HTTP layer reports the typed
//! failure and never returns a success-shaped body for a switch that did not
//! happen. Single-head providers stay honestly unsupported: their advertised
//! limitation is preserved and a different console is a typed 409.
//!
//! Non-active consoles are still available to controller previews through the
//! separate bounded [`crate::display_broker::ConsoleBroker`]; it caches D-Bus
//! sources for `?console=` snapshots/info without swapping this control's active
//! registry or disturbing HLS/WebRTC.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use async_trait::async_trait;
use axum::http::StatusCode;
use ozma_capture::CaptureRegistry;
use ozma_display::vm::VmConsoleBackend;
use ozma_media::CaptureSource;
use ozma_protocol::{codes, ApiError, CaptureCapability};
use ozma_qmp::SharedQmpClient;
use serde::{Deserialize, Serialize};

use crate::config::{NodeConfig, NodeProfile};
use crate::controller_compat::DisplayCaptureStatus;
use crate::display_outputs::{DisplayOutput, DisplayOutputSelection};
use crate::hls::HlsController;
use crate::webrtc::{WebRtcConfig, WebRtcControl, WebRtcHelper};

/// Shared capture registry slot used by HLS, WebRTC, and snapshots.
///
/// The slot is swapped only after every running pipeline restarted
/// successfully, so a failed switch does not leave future generations pointing
/// at the wrong console.
pub type RegistrySlot = Arc<RwLock<CaptureRegistry>>;

/// Rebuild the capture registry and display outputs for a console index.
pub trait ConsoleSourceFactory: Send + Sync {
    /// Configured capture transport, or `None` when no VM display is configured.
    fn provider(&self) -> Option<VmConsoleBackend>;

    /// Console index the node started with.
    fn default_console(&self) -> u32;

    /// True for providers that can target another `Console_N` at runtime.
    fn supports_console_switch(&self) -> bool;

    /// Build the registry for `console`.
    fn registry(&self, console: u32) -> Result<CaptureRegistry, String>;

    /// Recompute the controller-facing outputs for `console`.
    fn selection(&self, console: u32) -> Result<DisplayOutputSelection, String>;

    /// Explicit source id (`--capture-device`) this factory must select.
    ///
    /// `None` keeps the historical first-enumerated source behaviour. A
    /// configured id that discovery cannot find is a typed failure in
    /// [`open_selected_source`]; it never falls back to another device.
    fn source_id(&self) -> Option<&str> {
        None
    }
}

/// One pipeline that owns a capture source and can be restarted.
#[async_trait]
pub trait SwitchablePipeline: Send + Sync {
    /// Stable name used in the select response (`hls` or `webrtc`).
    fn name(&self) -> &'static str;

    /// True while the pipeline currently owns a live source.
    fn is_running(&self) -> bool;

    /// Stop the current source and start a fresh one from `registry`.
    ///
    /// The implementation must stop its old generation before opening the new
    /// source so capture devices/sockets are not held twice. A failure must
    /// leave the pipeline stopped rather than claiming a live stream; the
    /// caller performs rollback.
    async fn restart_from(
        &self,
        registry: &CaptureRegistry,
        output: Option<&DisplayOutput>,
    ) -> Result<(), String>;
}

/// Observer notified after a successful console selection.
///
/// Runtime multi-console HLS is not a restart-on-switch pipeline: each
/// advertised console keeps its own independent encoder. The observer lets the
/// selector publish the new active console and advertised set without stopping
/// those pipelines, while still allowing them to start/stop when a console is
/// added or removed.
#[async_trait]
pub trait ConsoleSelectionObserver: Send + Sync {
    /// Apply a committed selection. Failures are logged by the caller and must
    /// not change the already-committed active console.
    async fn on_selection(&self, selection: &DisplayOutputSelection);
}

/// Request body for `POST /display/select`.
#[derive(Clone, Debug, Default, Deserialize)]
pub struct SelectConsoleRequest {
    /// Zero-based `Console_N` index.
    #[serde(default)]
    pub index: Option<u32>,
    /// Console index, label, or capture source id.
    #[serde(default)]
    pub id: Option<ConsoleId>,
}

/// A console identifier accepted by `{id}`.
#[derive(Clone, Debug, Deserialize)]
#[serde(untagged)]
pub enum ConsoleId {
    /// Numeric console index.
    Index(u64),
    /// Output label, `Console_N`, or capture source id.
    Name(String),
}

/// Pipelines restarted by one successful selection.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize)]
pub struct RestartedPipelines {
    /// HLS restarted from the new console.
    pub hls: bool,
    /// WebRTC helper restarted from the new console.
    pub webrtc: bool,
}

/// Successful `POST /display/select` response.
#[derive(Clone, Debug, Serialize)]
pub struct SelectConsoleResponse {
    /// Always true on a 200 response.
    pub ok: bool,
    /// Console now selected for live capture.
    pub active_console: u32,
    /// Console selected before this request.
    pub previous_console: u32,
    /// Configured capture transport.
    pub provider: Option<String>,
    /// Updated controller-facing outputs.
    pub display_outputs: serde_json::Value,
    /// Updated live capture status and limitation.
    pub display_capture: DisplayCaptureStatus,
    /// False when the requested console was already active (no restart).
    pub changed: bool,
    /// Pipelines that were actually restarted.
    pub restarted: RestartedPipelines,
    /// Honest description when more consoles exist than live capture sources.
    pub limitation: Option<String>,
}

/// Typed failure from a console selection.
#[derive(Clone, Debug)]
pub struct DisplaySelectError {
    /// HTTP status to return.
    pub status: StatusCode,
    /// Stable error body.
    pub error: ApiError,
}

impl DisplaySelectError {
    fn invalid(message: impl Into<String>, details: serde_json::Value) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            error: ApiError::new(codes::INVALID_REQUEST, message)
                .with_details(details)
                .with_remediation(
                    "Send {\"index\": <Console_N>} or {\"id\": \"<label>\"} using an index from \
                     GET /api/v1/connection.display_outputs.",
                ),
        }
    }

    fn unknown(index: Option<u32>, available: &[u32]) -> Self {
        let message = match index {
            Some(index) => format!("Console_{index} is not among the advertised display outputs."),
            None => "the requested console is not among the advertised display outputs.".to_owned(),
        };
        Self {
            status: StatusCode::NOT_FOUND,
            error: ApiError::new(codes::NOT_FOUND, message)
                .with_details(serde_json::json!({
                    "requested": index,
                    "available": available,
                }))
                .with_remediation(
                    "Use an index advertised in GET /api/v1/connection.display_outputs.",
                ),
        }
    }

    fn single_head(provider: VmConsoleBackend, active: u32, requested: u32) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            error: ApiError::new(
                codes::CONFLICT,
                format!(
                    "the {provider} capture provider exposes one default console and cannot \
                     switch to Console_{requested} at runtime"
                ),
            )
            .with_details(serde_json::json!({
                "provider": provider.as_str(),
                "active_console": active,
                "requested_console": requested,
                "single_head": true,
            }))
            .with_remediation(format!(
                "Restart the node with --capture-console {requested} (--dbus-console for \
                 D-Bus) to capture that console with the {provider} provider, or configure \
                 --capture-provider dbus for runtime switching."
            )),
        }
    }

    fn unavailable() -> Self {
        Self {
            status: StatusCode::NOT_IMPLEMENTED,
            error: ApiError::new(
                codes::UNSUPPORTED,
                "no VM console capture source is configured, so no console can be selected",
            )
            .with_remediation(
                "Configure --profile vm with --capture-provider dbus (or a single-head \
                 provider) before selecting a console.",
            ),
        }
    }

    fn rebuild(message: String) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            error: ApiError::internal(format!(
                "rebuilding the capture registry for the requested console failed: {message}"
            ))
            .with_remediation(
                "The previous console and pipeline were left in place. Check that the QMP \
                 socket and D-Bus display are reachable, then retry.",
            ),
        }
    }

    fn pipeline(
        pipeline: &'static str,
        message: String,
        rolled_back: bool,
        rollback_errors: &[String],
    ) -> Self {
        let mut details = serde_json::json!({
            "pipeline": pipeline,
            "error": message,
            "rolled_back": rolled_back,
        });
        if !rollback_errors.is_empty() {
            details["rollback_errors"] = serde_json::json!(rollback_errors);
        }
        let remediation = if rolled_back {
            "The previous console and pipeline were restored. Check the node log for the \
             capture failure before retrying."
        } else {
            "The previous console could not be fully restored. Check the node log; a later \
             selection or node restart may be required to recover the stream."
        };
        Self {
            status: StatusCode::BAD_GATEWAY,
            error: ApiError::new(
                codes::CAPTURE_UNAVAILABLE,
                format!(
                    "restarting the {pipeline} pipeline for the requested console failed: \
                     {message}"
                ),
            )
            .with_details(details)
            .with_remediation(remediation)
            .retryable(),
        }
    }
}

/// Live display state for `/api/v1/connection`.
#[derive(Clone, Debug, Serialize)]
pub struct DisplayStatus {
    /// Controller-facing display outputs.
    pub display_outputs: serde_json::Value,
    /// Active console/provider/limitation.
    pub display_capture: DisplayCaptureStatus,
}

#[derive(Clone)]
struct State {
    selection: DisplayOutputSelection,
    provider: Option<VmConsoleBackend>,
    /// Monotonic generation, used by tests/logs to prove a switch happened.
    generation: u64,
}

/// Shared, cloneable runtime console selector.
#[derive(Clone)]
pub struct DisplayControl {
    state: Arc<Mutex<State>>,
    switch_lock: Arc<tokio::sync::Mutex<()>>,
    factory: Arc<dyn ConsoleSourceFactory>,
    registry: RegistrySlot,
    pipelines: Arc<Vec<Arc<dyn SwitchablePipeline>>>,
    observers: Arc<Vec<Arc<dyn ConsoleSelectionObserver>>>,
    generation: Arc<AtomicU64>,
}

impl std::fmt::Debug for DisplayControl {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        f.debug_struct("DisplayControl")
            .field("active_console", &state.selection.active_console)
            .field("provider", &state.provider)
            .field("pipelines", &self.pipelines.len())
            .finish()
    }
}

impl DisplayControl {
    /// Build a selector and validate the initial outputs.
    pub fn new(
        factory: Arc<dyn ConsoleSourceFactory>,
        registry: RegistrySlot,
        pipelines: Vec<Arc<dyn SwitchablePipeline>>,
    ) -> Result<Self, String> {
        let provider = factory.provider();
        let console = factory.default_console();
        let selection = factory.selection(console)?;
        Ok(Self {
            state: Arc::new(Mutex::new(State {
                selection,
                provider,
                generation: 0,
            })),
            switch_lock: Arc::new(tokio::sync::Mutex::new(())),
            factory,
            registry,
            pipelines: Arc::new(pipelines),
            observers: Arc::new(Vec::new()),
            generation: Arc::new(AtomicU64::new(0)),
        })
    }

    /// Attach an observer notified after every committed selection.
    ///
    /// Used by the bounded per-console HLS manager: a switch updates which
    /// console `/stream/*` serves and which pipelines are advertised, but does
    /// not restart the independent per-console encoders.
    pub fn with_observer(mut self, observer: Arc<dyn ConsoleSelectionObserver>) -> Self {
        Arc::make_mut(&mut self.observers).push(observer);
        self
    }

    /// Live display state, safe to call from any task.
    pub fn status(&self) -> DisplayStatus {
        let state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let active =
            (!state.selection.outputs.is_empty()).then_some(state.selection.active_console);
        DisplayStatus {
            display_outputs: state.selection.to_value(),
            display_capture: DisplayCaptureStatus::new(
                active,
                state.provider.map(|provider| provider.as_str().to_owned()),
                state.selection.limitation.clone(),
            ),
        }
    }

    /// Active console index.
    pub fn active_console(&self) -> u32 {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .selection
            .active_console
    }

    /// Snapshot of the current advertised outputs and active console.
    pub fn selection(&self) -> DisplayOutputSelection {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .selection
            .clone()
    }

    /// Monotonic successful-switch generation (0 before any switch).
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Select a console and restart running pipelines.
    pub async fn select(
        &self,
        request: &SelectConsoleRequest,
    ) -> Result<SelectConsoleResponse, DisplaySelectError> {
        let _guard = self.switch_lock.lock().await;

        let (selection, provider) = {
            let state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            (state.selection.clone(), state.provider)
        };
        let available: Vec<u32> = selection
            .outputs
            .iter()
            .map(|output| output.index)
            .collect();
        let requested = match self.resolve_index(request, &selection) {
            Some(index) => index,
            None if request.index.is_some() || request.id.is_some() => {
                return Err(DisplaySelectError::unknown(None, &available));
            }
            None => {
                return Err(DisplaySelectError::invalid(
                    "a console `index` or `id` is required",
                    serde_json::json!({ "available": available }),
                ));
            }
        };

        if !available.contains(&requested) {
            return Err(DisplaySelectError::unknown(Some(requested), &available));
        }

        let previous = selection.active_console;
        if requested == previous {
            let response = self.response(
                &selection,
                provider,
                previous,
                false,
                RestartedPipelines::default(),
            );
            self.notify_observers(&selection).await;
            return Ok(response);
        }

        let Some(provider) = provider else {
            return Err(DisplaySelectError::unavailable());
        };
        if !self.factory.supports_console_switch() {
            return Err(DisplaySelectError::single_head(
                provider, previous, requested,
            ));
        }

        let new_selection = self
            .factory
            .selection(requested)
            .map_err(DisplaySelectError::rebuild)?;
        if !new_selection
            .outputs
            .iter()
            .any(|output| output.index == requested)
        {
            return Err(DisplaySelectError::unknown(Some(requested), &available));
        }
        let new_registry = self
            .factory
            .registry(requested)
            .map_err(DisplaySelectError::rebuild)?;
        let old_registry = self
            .registry
            .read()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();

        let old_output = output_for(&selection, previous);
        let new_output = output_for(&new_selection, requested);
        let running: Vec<&Arc<dyn SwitchablePipeline>> = self
            .pipelines
            .iter()
            .filter(|pipeline| pipeline.is_running())
            .collect();

        let mut restarted: Vec<&'static str> = Vec::new();
        for pipeline in &running {
            match pipeline
                .restart_from(&new_registry, new_output.as_ref())
                .await
            {
                Ok(()) => restarted.push(pipeline.name()),
                Err(error) => {
                    let name = pipeline.name();
                    let mut rollback_errors = Vec::new();
                    if let Err(rollback) = pipeline
                        .restart_from(&old_registry, old_output.as_ref())
                        .await
                    {
                        rollback_errors.push(format!("{name}: {rollback}"));
                    }
                    for done in restarted.iter().rev() {
                        if let Some(pipeline) = self
                            .pipelines
                            .iter()
                            .find(|candidate| candidate.name() == *done)
                        {
                            if let Err(rollback) = pipeline
                                .restart_from(&old_registry, old_output.as_ref())
                                .await
                            {
                                rollback_errors.push(format!("{done}: {rollback}"));
                            }
                        }
                    }
                    let rolled_back = rollback_errors.is_empty();
                    tracing::warn!(
                        pipeline = name,
                        requested = requested,
                        previous = previous,
                        error = %error,
                        rolled_back,
                        rollback_errors = ?rollback_errors,
                        "display console switch failed; previous console restored where possible"
                    );
                    return Err(DisplaySelectError::pipeline(
                        name,
                        error,
                        rolled_back,
                        &rollback_errors,
                    ));
                }
            }
        }

        *self
            .registry
            .write()
            .unwrap_or_else(|poisoned| poisoned.into_inner()) = new_registry;

        {
            let mut state = self
                .state
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner());
            state.selection = new_selection.clone();
            state.generation = state.generation.saturating_add(1);
        }
        let generation = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        tracing::info!(
            console = requested,
            previous = previous,
            provider = provider.as_str(),
            generation,
            restarted = ?restarted,
            "display console switched"
        );

        let mut flags = RestartedPipelines::default();
        for name in restarted {
            match name {
                "hls" => flags.hls = true,
                "webrtc" => flags.webrtc = true,
                _ => {}
            }
        }
        self.notify_observers(&new_selection).await;
        Ok(self.response(&new_selection, Some(provider), previous, true, flags))
    }

    /// Publish a committed selection to observers.
    ///
    /// The console switch itself has already succeeded; an observer failure is
    /// logged and must not roll back the active console or fabricate an error
    /// for the caller.
    async fn notify_observers(&self, selection: &DisplayOutputSelection) {
        for observer in self.observers.iter() {
            observer.on_selection(selection).await;
        }
    }

    fn response(
        &self,
        selection: &DisplayOutputSelection,
        provider: Option<VmConsoleBackend>,
        previous: u32,
        changed: bool,
        restarted: RestartedPipelines,
    ) -> SelectConsoleResponse {
        SelectConsoleResponse {
            ok: true,
            active_console: selection.active_console,
            previous_console: previous,
            provider: provider.map(|provider| provider.as_str().to_owned()),
            display_outputs: selection.to_value(),
            display_capture: DisplayCaptureStatus::new(
                (!selection.outputs.is_empty()).then_some(selection.active_console),
                provider.map(|provider| provider.as_str().to_owned()),
                selection.limitation.clone(),
            ),
            limitation: selection.limitation.clone(),
            changed,
            restarted,
        }
    }

    fn resolve_index(
        &self,
        request: &SelectConsoleRequest,
        selection: &DisplayOutputSelection,
    ) -> Option<u32> {
        if let Some(index) = request.index {
            return Some(index);
        }
        let id = request.id.as_ref()?;
        match id {
            ConsoleId::Index(index) => u32::try_from(*index).ok(),
            ConsoleId::Name(name) => {
                let name = name.trim();
                if name.is_empty() {
                    return None;
                }
                if let Ok(index) = name.parse::<u32>() {
                    return Some(index);
                }
                if let Some(rest) = name
                    .strip_prefix("Console_")
                    .or_else(|| name.strip_prefix("console_"))
                {
                    if let Ok(index) = rest.parse::<u32>() {
                        return Some(index);
                    }
                }
                selection
                    .outputs
                    .iter()
                    .find(|output| {
                        output.name.eq_ignore_ascii_case(name)
                            || (!output.capture_source_id.is_empty()
                                && output.capture_source_id.eq_ignore_ascii_case(name))
                    })
                    .map(|output| output.index)
            }
        }
    }
}

fn output_for(selection: &DisplayOutputSelection, index: u32) -> Option<DisplayOutput> {
    selection
        .outputs
        .iter()
        .find(|output| output.index == index)
        .cloned()
        .or_else(|| {
            (!selection.outputs.is_empty()).then(|| DisplayOutput {
                index,
                name: format!("Console_{index}"),
                source_type: "unknown".to_owned(),
                capture_source_id: String::new(),
                width: 0,
                height: 0,
                primary: false,
            })
        })
}

/// Production factory backed by [`NodeConfig`] and the node's shared QMP client.
pub struct ConfigConsoleFactory {
    config: NodeConfig,
    shared: Option<SharedQmpClient>,
}

impl ConfigConsoleFactory {
    /// Build a factory for a validated node configuration.
    pub fn new(config: NodeConfig, shared: Option<SharedQmpClient>) -> Self {
        Self { config, shared }
    }

    fn with_console(&self, console: u32) -> NodeConfig {
        let mut config = self.config.clone();
        config.dbus_console = console;
        config.capture_console = Some(console);
        config
    }
}

impl ConsoleSourceFactory for ConfigConsoleFactory {
    fn provider(&self) -> Option<VmConsoleBackend> {
        self.config
            .capture_display_source()
            .ok()
            .flatten()
            .map(|source| source.backend)
    }

    fn default_console(&self) -> u32 {
        self.config
            .capture_display_source()
            .ok()
            .flatten()
            .map(|source| source.console_index)
            .or(self.config.capture_console)
            .unwrap_or(0)
    }

    fn supports_console_switch(&self) -> bool {
        matches!(self.provider(), Some(VmConsoleBackend::Dbus))
    }

    fn registry(&self, console: u32) -> Result<CaptureRegistry, String> {
        let config = self.with_console(console);
        Ok(crate::platform::capture_registry_with_qmp_shared(
            &config,
            self.shared.as_ref(),
        ))
    }

    fn selection(&self, console: u32) -> Result<DisplayOutputSelection, String> {
        self.with_console(console).resolve_display_outputs()
    }

    fn source_id(&self) -> Option<&str> {
        (self.config.profile != NodeProfile::Vm)
            .then_some(self.config.capture_device.as_deref())
            .flatten()
    }
}

/// HLS pipeline adapter for console switching.
pub struct HlsPipelineSwitch {
    controller: HlsController,
}

impl HlsPipelineSwitch {
    /// Wrap a restartable HLS controller.
    pub fn new(controller: HlsController) -> Arc<Self> {
        Arc::new(Self { controller })
    }
}

#[async_trait]
impl SwitchablePipeline for HlsPipelineSwitch {
    fn name(&self) -> &'static str {
        "hls"
    }

    fn is_running(&self) -> bool {
        self.controller.is_running()
    }

    async fn restart_from(
        &self,
        registry: &CaptureRegistry,
        _output: Option<&DisplayOutput>,
    ) -> Result<(), String> {
        // Stop the old generation before opening the new source: capture
        // backends release their socket/device in `stop`, and the switch must
        // not hold two consoles at once. The caller rolls back with the old
        // registry if the new source cannot start. The replacement generation
        // appends to the existing playlist with `#EXT-X-DISCONTINUITY`, so a
        // console switch does not reset the HLS media sequence.
        self.controller
            .stop()
            .await
            .map_err(|error| error.to_string())?;
        let source = open_first_source(registry)?;
        self.controller
            .start_restarted_with_source(source)
            .await
            .map(|_| ())
            .map_err(|error| error.to_string())
    }
}

/// WebRTC helper adapter for console switching.
pub struct WebRtcPipelineSwitch {
    control: WebRtcControl,
    config: Mutex<WebRtcConfig>,
}

impl WebRtcPipelineSwitch {
    /// Wrap the shared WebRTC control and its helper configuration.
    pub fn new(control: WebRtcControl, config: WebRtcConfig) -> Arc<Self> {
        Arc::new(Self {
            control,
            config: Mutex::new(config),
        })
    }
}

#[async_trait]
impl SwitchablePipeline for WebRtcPipelineSwitch {
    fn name(&self) -> &'static str {
        "webrtc"
    }

    fn is_running(&self) -> bool {
        self.control.is_running()
    }

    async fn restart_from(
        &self,
        registry: &CaptureRegistry,
        output: Option<&DisplayOutput>,
    ) -> Result<(), String> {
        let mut config = self
            .config
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone();
        // A D-Bus source reports 0x0 until its first scanout. When the console
        // specification carries real geometry, use it so the helper's frame
        // size matches the new console instead of the previous one.
        if let Some(output) = output {
            if output.width > 0 && output.height > 0 {
                config.width = output.width;
                config.height = output.height;
            }
        }

        self.control.shutdown().await;
        let source = match open_first_source(registry) {
            Ok(source) => source,
            Err(error) => {
                self.control.mark_failed(error.clone());
                return Err(error);
            }
        };
        match WebRtcHelper::start(source, config.clone()) {
            Ok(helper) => {
                self.control.replace_helper(helper);
                *self
                    .config
                    .lock()
                    .unwrap_or_else(|poisoned| poisoned.into_inner()) = config;
                Ok(())
            }
            Err(error) => {
                let message =
                    format!("WebRTC helper did not start for the selected console: {error}");
                self.control.mark_failed(message.clone());
                Err(message)
            }
        }
    }
}

/// Open the first advertised capture source.
///
/// This mirrors the node's HLS/WebRTC startup path: enumerate without opening,
/// then open exactly the selected capability. Providers keep their own honest
/// errors; no source is fabricated.
pub fn open_first_source(registry: &CaptureRegistry) -> Result<Box<dyn CaptureSource>, String> {
    open_selected_source(registry, None)
}

/// Open the source selected by `requested`, else the first advertised source.
///
/// `requested` is a capture source id (a V4L2 device path such as
/// `/dev/video42`) or the advertised source name. When it is set, discovery
/// must contain an exact match: a missing device is a failure, not an
/// invitation to open the first enumerated camera.
pub fn open_selected_source(
    registry: &CaptureRegistry,
    requested: Option<&str>,
) -> Result<Box<dyn CaptureSource>, String> {
    let sources = registry.enumerate().map_err(|error| {
        format!(
            "capture discovery failed: {} ({})",
            error.message, error.code
        )
    })?;
    let source = match requested.map(str::trim).filter(|id| !id.is_empty()) {
        Some(requested) => sources
            .into_iter()
            .find(|capability| capability_matches(capability, requested))
            .ok_or_else(|| {
                format!(
                    "requested capture source `{requested}` is not available; refusing to fall \
                     back to another capture device"
                )
            })?,
        None => sources
            .into_iter()
            .next()
            .ok_or_else(|| "no capture source is available for the selected console".to_owned())?,
    };
    let id = source.id.clone();
    registry
        .open(&source)
        .map_err(|error| format!("opening capture source `{id}` failed: {}", error.message))
}

/// True when a discovered source is the one named by `--capture-device`.
pub fn capability_matches(capability: &CaptureCapability, requested: &str) -> bool {
    capability.id == requested || capability.name == requested
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_capture::FakeCaptureProvider;
    use ozma_display::vm::VmConsoleBackend;
    use ozma_media::{FrameFormat, PixelFormat};
    use ozma_protocol::CaptureCapability;
    use serde_json::json;
    use std::sync::atomic::AtomicBool;

    fn multi_source_registry() -> CaptureRegistry {
        CaptureRegistry::new()
            .with(
                FakeCaptureProvider::new()
                    .with_identity("/dev/video0", "Creative Live! Cam")
                    .with_format(FrameFormat::rgb24(64, 48)),
            )
            .with(
                FakeCaptureProvider::new()
                    .with_identity("/dev/video42", "ozma-capture-test")
                    .with_format(FrameFormat::rgb24(32, 24)),
            )
    }

    #[test]
    fn selected_capture_device_is_opened_by_path_or_name() {
        let registry = multi_source_registry();
        let source = open_selected_source(&registry, Some("/dev/video42")).expect("selected path");
        assert_eq!(source.id(), "/dev/video42");
        assert_eq!(source.format().pixel_format, PixelFormat::Rgb24);

        let by_name = open_selected_source(&registry, Some("ozma-capture-test")).expect("name");
        assert_eq!(by_name.id(), "/dev/video42");

        // No selector keeps the historical first-source behaviour.
        let first = open_first_source(&registry).expect("first");
        assert_eq!(first.id(), "/dev/video0");
    }

    #[test]
    fn missing_capture_device_is_typed_and_never_falls_back_to_the_first_camera() {
        let registry = multi_source_registry();
        let error = open_selected_source(&registry, Some("/dev/video99"))
            .err()
            .expect("missing device");
        assert!(
            error.contains("/dev/video99") && error.contains("refusing to fall back"),
            "{error}"
        );
        assert!(
            !error.contains("/dev/video0"),
            "error must name the requested device, not the fallback: {error}"
        );
    }

    fn outputs(current: u32) -> DisplayOutputSelection {
        let specs = vec![
            "0:Console_0:1920x1080:primary:dbus".to_owned(),
            "1:Console_1:1280x1024:dbus".to_owned(),
        ];
        crate::display_outputs::outputs_from_specs(
            &specs,
            VmConsoleBackend::Dbus,
            Some("dbus0"),
            Some(current),
        )
        .expect("valid outputs")
    }

    fn console_registry(console: u32) -> CaptureRegistry {
        CaptureRegistry::new().with(
            FakeCaptureProvider::new()
                .with_identity(format!("console-{console}"), format!("Console_{console}")),
        )
    }

    struct FakeFactory {
        provider: Option<VmConsoleBackend>,
        outputs: Vec<u32>,
        fail_registry: Vec<u32>,
        registries: Mutex<Vec<u32>>,
    }

    impl FakeFactory {
        fn dbus() -> Arc<Self> {
            Arc::new(Self {
                provider: Some(VmConsoleBackend::Dbus),
                outputs: vec![0, 1],
                fail_registry: Vec::new(),
                registries: Mutex::new(Vec::new()),
            })
        }

        fn single_head(provider: VmConsoleBackend) -> Arc<Self> {
            Arc::new(Self {
                provider: Some(provider),
                outputs: vec![0, 1],
                fail_registry: Vec::new(),
                registries: Mutex::new(Vec::new()),
            })
        }

        fn collected(&self) -> Vec<u32> {
            self.registries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

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
            if self.fail_registry.contains(&console) {
                return Err(format!("synthetic registry failure for Console_{console}"));
            }
            self.registries
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(console);
            Ok(console_registry(console))
        }

        fn selection(&self, console: u32) -> Result<DisplayOutputSelection, String> {
            if !self.outputs.contains(&console) {
                return Err(format!("Console_{console} is not advertised"));
            }
            Ok(outputs(console))
        }
    }

    struct FakePipeline {
        name: &'static str,
        running: AtomicBool,
        fail_console: Option<u32>,
        calls: Mutex<Vec<u32>>,
    }

    impl FakePipeline {
        fn new(name: &'static str, running: bool) -> Arc<Self> {
            Arc::new(Self {
                name,
                running: AtomicBool::new(running),
                fail_console: None,
                calls: Mutex::new(Vec::new()),
            })
        }

        fn failing(name: &'static str, console: u32) -> Arc<Self> {
            Arc::new(Self {
                name,
                running: AtomicBool::new(true),
                fail_console: Some(console),
                calls: Mutex::new(Vec::new()),
            })
        }

        fn calls(&self) -> Vec<u32> {
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .clone()
        }
    }

    #[async_trait]
    impl SwitchablePipeline for FakePipeline {
        fn name(&self) -> &'static str {
            self.name
        }

        fn is_running(&self) -> bool {
            self.running.load(Ordering::SeqCst)
        }

        async fn restart_from(
            &self,
            registry: &CaptureRegistry,
            _output: Option<&DisplayOutput>,
        ) -> Result<(), String> {
            let capabilities: Vec<CaptureCapability> =
                registry.enumerate().map_err(|error| error.message)?;
            let id = capabilities
                .first()
                .map(|capability| capability.id.clone())
                .ok_or_else(|| "no source".to_owned())?;
            let console = id
                .strip_prefix("console-")
                .and_then(|value| value.parse::<u32>().ok())
                .ok_or_else(|| format!("unexpected source id {id}"))?;
            self.calls
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner())
                .push(console);
            if self.fail_console == Some(console) {
                self.running.store(false, Ordering::SeqCst);
                return Err(format!("synthetic pipeline failure for Console_{console}"));
            }
            self.running.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    fn control(
        factory: Arc<FakeFactory>,
        pipelines: Vec<Arc<dyn SwitchablePipeline>>,
    ) -> (DisplayControl, RegistrySlot) {
        let slot = Arc::new(RwLock::new(console_registry(0)));
        let control = DisplayControl::new(factory, slot.clone(), pipelines).expect("control");
        (control, slot)
    }

    #[tokio::test]
    async fn selects_a_valid_console_and_restarts_running_pipelines() {
        let factory = FakeFactory::dbus();
        let hls = FakePipeline::new("hls", true);
        let webrtc = FakePipeline::new("webrtc", true);
        let (control, slot) = control(factory.clone(), vec![hls.clone(), webrtc.clone()]);

        let response = control
            .select(&SelectConsoleRequest {
                index: Some(1),
                id: None,
            })
            .await
            .expect("select");

        assert!(response.ok);
        assert!(response.changed);
        assert_eq!(response.previous_console, 0);
        assert_eq!(response.active_console, 1);
        assert_eq!(response.provider.as_deref(), Some("dbus"));
        assert_eq!(
            response.restarted,
            RestartedPipelines {
                hls: true,
                webrtc: true
            }
        );
        assert_eq!(hls.calls(), vec![1]);
        assert_eq!(webrtc.calls(), vec![1]);
        assert_eq!(factory.collected(), vec![1]);
        assert_eq!(control.active_console(), 1);
        assert_eq!(control.generation(), 1);

        let outputs = response.display_outputs.as_array().expect("array");
        assert_eq!(outputs[0]["capture_source_id"], "");
        assert_eq!(outputs[1]["capture_source_id"], "dbus0");
        assert!(response
            .limitation
            .as_deref()
            .is_some_and(|note| note.contains("Console_1")));

        let live = slot.read().unwrap();
        assert_eq!(
            live.enumerate().unwrap()[0].id,
            "console-1",
            "the shared registry must point at the selected console"
        );
    }

    #[tokio::test]
    async fn selecting_the_active_console_is_idempotent() {
        let factory = FakeFactory::dbus();
        let hls = FakePipeline::new("hls", true);
        let (control, _) = control(factory, vec![hls.clone()]);

        let response = control
            .select(&SelectConsoleRequest {
                index: Some(0),
                id: None,
            })
            .await
            .expect("select");
        assert!(!response.changed);
        assert_eq!(response.active_console, 0);
        assert_eq!(response.restarted, RestartedPipelines::default());
        assert!(hls.calls().is_empty());
        assert_eq!(control.generation(), 0);
    }

    #[tokio::test]
    async fn resolves_a_console_by_label_or_source_id() {
        let factory = FakeFactory::dbus();
        let (first, _) = control(factory, Vec::new());

        let response = first
            .select(&SelectConsoleRequest {
                index: None,
                id: Some(ConsoleId::Name("Console_1".to_owned())),
            })
            .await
            .expect("select by label");
        assert_eq!(response.active_console, 1);

        // A bare index string and a numeric id are accepted too. The active
        // capture source id (`dbus0`) only identifies the active console: the
        // other advertised consoles do not claim a source id.
        let factory = FakeFactory::dbus();
        let (second, _) = control(factory, Vec::new());
        let response = second
            .select(&SelectConsoleRequest {
                index: None,
                id: Some(ConsoleId::Index(1)),
            })
            .await
            .expect("select by numeric id");
        assert_eq!(response.active_console, 1);

        let factory = FakeFactory::dbus();
        let (third, _) = control(factory, Vec::new());
        let response = third
            .select(&SelectConsoleRequest {
                index: None,
                id: Some(ConsoleId::Name("1".to_owned())),
            })
            .await
            .expect("select by index string");
        assert_eq!(response.active_console, 1);
    }

    #[tokio::test]
    async fn rejects_an_unadvertised_console_without_touching_state() {
        let factory = FakeFactory::dbus();
        let hls = FakePipeline::new("hls", true);
        let (control, slot) = control(factory.clone(), vec![hls.clone()]);

        let error = control
            .select(&SelectConsoleRequest {
                index: Some(9),
                id: None,
            })
            .await
            .expect_err("unknown console");
        assert_eq!(error.status, StatusCode::NOT_FOUND);
        assert_eq!(error.error.code, codes::NOT_FOUND);
        assert_eq!(error.error.details["available"], json!([0, 1]));
        assert_eq!(control.active_console(), 0);
        assert!(hls.calls().is_empty());
        assert_eq!(slot.read().unwrap().enumerate().unwrap()[0].id, "console-0");

        let error = control
            .select(&SelectConsoleRequest {
                index: None,
                id: Some(ConsoleId::Name("not-a-console".to_owned())),
            })
            .await
            .expect_err("unknown id");
        assert_eq!(error.status, StatusCode::NOT_FOUND);
    }

    #[tokio::test]
    async fn requires_an_index_or_id() {
        let factory = FakeFactory::dbus();
        let (control, _) = control(factory, Vec::new());
        let error = control
            .select(&SelectConsoleRequest::default())
            .await
            .expect_err("empty request");
        assert_eq!(error.status, StatusCode::BAD_REQUEST);
        assert_eq!(error.error.code, codes::INVALID_REQUEST);
    }

    #[tokio::test]
    async fn single_head_providers_reject_another_console_with_a_typed_409() {
        for provider in [
            VmConsoleBackend::Qmp,
            VmConsoleBackend::Vnc,
            VmConsoleBackend::LookingGlass,
        ] {
            let factory = FakeFactory::single_head(provider);
            let hls = FakePipeline::new("hls", true);
            let (control, slot) = control(factory, vec![hls.clone()]);

            let error = control
                .select(&SelectConsoleRequest {
                    index: Some(1),
                    id: None,
                })
                .await
                .expect_err("single head");
            assert_eq!(error.status, StatusCode::CONFLICT);
            assert_eq!(error.error.code, codes::CONFLICT);
            assert_eq!(error.error.details["provider"], provider.as_str());
            assert_eq!(error.error.details["single_head"], true);
            assert!(hls.calls().is_empty());
            assert_eq!(control.active_console(), 0);
            assert_eq!(slot.read().unwrap().enumerate().unwrap()[0].id, "console-0");
        }
    }

    #[tokio::test]
    async fn a_pipeline_failure_rolls_back_the_registry_and_pipelines() {
        let factory = FakeFactory::dbus();
        let hls = FakePipeline::failing("hls", 1);
        let (control, slot) = control(factory, vec![hls.clone()]);

        let error = control
            .select(&SelectConsoleRequest {
                index: Some(1),
                id: None,
            })
            .await
            .expect_err("pipeline failure");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(error.error.code, codes::CAPTURE_UNAVAILABLE);
        assert_eq!(error.error.details["pipeline"], "hls");
        assert_eq!(error.error.details["rolled_back"], true);
        assert_eq!(hls.calls(), vec![1, 0], "new console, then rollback");
        assert_eq!(control.active_console(), 0);
        assert_eq!(control.generation(), 0);
        assert_eq!(slot.read().unwrap().enumerate().unwrap()[0].id, "console-0");
    }

    #[tokio::test]
    async fn a_second_pipeline_failure_rolls_back_the_first_pipeline() {
        let factory = FakeFactory::dbus();
        let hls = FakePipeline::new("hls", true);
        let webrtc = FakePipeline::failing("webrtc", 1);
        let (control, slot) = control(factory, vec![hls.clone(), webrtc.clone()]);

        let error = control
            .select(&SelectConsoleRequest {
                index: Some(1),
                id: None,
            })
            .await
            .expect_err("webrtc failure");
        assert_eq!(error.status, StatusCode::BAD_GATEWAY);
        assert_eq!(hls.calls(), vec![1, 0]);
        assert_eq!(webrtc.calls(), vec![1, 0]);
        assert_eq!(control.active_console(), 0);
        assert_eq!(slot.read().unwrap().enumerate().unwrap()[0].id, "console-0");
    }

    #[tokio::test]
    async fn a_registry_rebuild_failure_keeps_the_old_console() {
        let factory = Arc::new(FakeFactory {
            provider: Some(VmConsoleBackend::Dbus),
            outputs: vec![0, 1],
            fail_registry: vec![1],
            registries: Mutex::new(Vec::new()),
        });
        let hls = FakePipeline::new("hls", true);
        let (control, slot) = control(factory, vec![hls.clone()]);

        let error = control
            .select(&SelectConsoleRequest {
                index: Some(1),
                id: None,
            })
            .await
            .expect_err("registry failure");
        assert_eq!(error.status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(error.error.message.contains("synthetic registry failure"));
        assert!(hls.calls().is_empty());
        assert_eq!(control.active_console(), 0);
        assert_eq!(slot.read().unwrap().enumerate().unwrap()[0].id, "console-0");
    }

    #[test]
    fn status_reports_the_limitation_and_active_capture_id() {
        let factory = FakeFactory::dbus();
        let (control, _) = control(factory, Vec::new());
        let status = control.status();
        assert_eq!(status.display_capture.active_console, Some(0));
        assert_eq!(status.display_capture.provider.as_deref(), Some("dbus"));
        assert!(status
            .display_capture
            .limitation
            .as_deref()
            .is_some_and(|note| note.contains("Console_0")));
        let outputs = status.display_outputs.as_array().expect("outputs");
        assert_eq!(outputs[0]["capture_source_id"], "dbus0");
        assert_eq!(outputs[1]["capture_source_id"], "");
    }
}
