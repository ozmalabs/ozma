//! Connectivity probe for a PipeWire daemon/session.
//!
//! The probe connects a core, waits for a sync round-trip, enumerates audio
//! nodes, and reads the `default` metadata object.  It never opens a stream and
//! never blocks without a timeout, so it is safe to call from diagnostics on a
//! machine with no PipeWire session.

use std::{
    cell::RefCell,
    collections::HashMap,
    env,
    rc::Rc,
    sync::mpsc::RecvTimeoutError,
    thread,
    time::{Duration, Instant},
};

use pipewire as pw;
use pw::{
    main_loop::MainLoopRc,
    metadata::{Metadata, MetadataListener},
    properties::properties,
    types::ObjectType,
};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;
use tracing::debug;

use crate::error::PipeWireError;
use crate::types::AudioNode;

/// Actionable failure attached to an unreachable [`PipeWireStatus`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StatusError {
    /// Stable code such as `pipewire.unavailable`.
    pub code: String,
    pub message: String,
    pub remediation: String,
}

/// Result of a PipeWire session probe.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PipeWireStatus {
    /// `true` when a daemon answered the sync round-trip.
    pub reachable: bool,
    /// Remote name/socket used for the connection.
    pub remote: Option<String>,
    /// `default.audio.source` name from PipeWire metadata, when published.
    pub default_source: Option<String>,
    /// `default.audio.sink` name from PipeWire metadata, when published.
    pub default_sink: Option<String>,
    /// Audio source nodes in the graph.
    pub sources: Vec<AudioNode>,
    /// Audio sink nodes in the graph.
    pub sinks: Vec<AudioNode>,
    /// Time spent on the sync round-trip.
    pub probe_micros: u64,
    /// Why the probe failed, when `reachable` is false.
    pub error: Option<StatusError>,
}

impl PipeWireStatus {
    /// True when the graph exposes at least one audio source or sink.
    pub fn has_audio(&self) -> bool {
        !self.sources.is_empty() || !self.sinks.is_empty()
    }

    /// Short human-readable summary for diagnostics.
    pub fn summary(&self) -> String {
        if let Some(error) = &self.error {
            return format!(
                "PipeWire unreachable via {}: {} ({})",
                self.remote.as_deref().unwrap_or("default remote"),
                error.message,
                error.code
            );
        }
        format!(
            "PipeWire reachable via {} ({} source(s), {} sink(s); default source {:?}, default sink {:?})",
            self.remote.as_deref().unwrap_or("default remote"),
            self.sources.len(),
            self.sinks.len(),
            self.default_source,
            self.default_sink
        )
    }

    fn unreachable(remote: Option<String>, error: &PipeWireError) -> Self {
        Self {
            reachable: false,
            remote,
            default_source: None,
            default_sink: None,
            sources: Vec::new(),
            sinks: Vec::new(),
            probe_micros: 0,
            error: Some(StatusError {
                code: error.code().to_string(),
                message: error.to_string(),
                remediation: error.remediation().to_string(),
            }),
        }
    }
}

/// Optional overrides for [`probe_with`].
#[derive(Debug, Clone)]
pub struct ProbeConfig {
    /// PipeWire remote name (normally from `PIPEWIRE_REMOTE`).
    pub remote: Option<String>,
    /// Maximum time to wait for the sync round-trip.
    pub timeout: Duration,
    /// Maximum time to wait for the probe thread to exit after a timeout.
    pub stop_timeout: Duration,
}

impl Default for ProbeConfig {
    fn default() -> Self {
        Self {
            remote: None,
            timeout: Duration::from_secs(2),
            stop_timeout: Duration::from_secs(2),
        }
    }
}

/// Probe with a caller-supplied timeout.
pub async fn probe(timeout: Duration) -> Result<PipeWireStatus, PipeWireError> {
    probe_with(ProbeConfig {
        timeout,
        ..ProbeConfig::default()
    })
    .await
}

/// Probe the PipeWire session, returning an actionable error when unreachable.
pub async fn probe_with(config: ProbeConfig) -> Result<PipeWireStatus, PipeWireError> {
    let timeout = config.timeout;
    let stop_timeout = config.stop_timeout;
    let remote = configured_remote(config.remote.clone());

    let (cmd_tx, cmd_rx) = pw::channel::channel::<ProbeCommand>();
    let (result_tx, result_rx) = oneshot::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel::<()>();
    let thread_config = config.clone();

    let handle = thread::Builder::new()
        .name("ozma-pw-probe".into())
        .spawn(move || {
            probe_thread_main(thread_config, cmd_rx, result_tx);
            drop(done_tx);
        })
        .map_err(|e| PipeWireError::Thread {
            detail: e.to_string(),
        })?;

    match tokio::time::timeout(timeout, result_rx).await {
        Ok(Ok(result)) => {
            let _ = done_rx.recv_timeout(stop_timeout);
            let _ = handle.join();
            result
        }
        Ok(Err(_)) => {
            let _ = handle.join();
            Err(PipeWireError::unavailable(
                "PipeWire probe thread exited without a result",
            ))
        }
        Err(_) => {
            let _ = cmd_tx.send(ProbeCommand::Quit);
            let finished = matches!(
                done_rx.recv_timeout(stop_timeout),
                Ok(()) | Err(RecvTimeoutError::Disconnected)
            );
            if finished {
                let _ = handle.join();
            } else {
                // Never hang diagnostics on a wedged server; detach instead.
                debug!("PipeWire probe thread did not exit; detaching");
            }
            Err(PipeWireError::ProbeTimeout {
                detail: format!(
                    "no sync reply from {} within {:?}",
                    remote.as_deref().unwrap_or("default remote"),
                    timeout
                ),
            })
        }
    }
}

/// Probe and always return a [`PipeWireStatus`], with the failure recorded in
/// `status.error` instead of returning `Err`.
///
/// This is the shape diagnostics wants: a single value that says whether the
/// session is reachable and why not.
pub async fn probe_status(config: ProbeConfig) -> PipeWireStatus {
    let remote = configured_remote(config.remote.clone());
    match probe_with(config).await {
        Ok(status) => status,
        Err(error) => PipeWireStatus::unreachable(remote, &error),
    }
}

fn configured_remote(override_remote: Option<String>) -> Option<String> {
    override_remote
        .or_else(|| env::var("PIPEWIRE_REMOTE").ok())
        .or_else(|| {
            env::var("XDG_RUNTIME_DIR")
                .ok()
                .map(|dir| format!("{dir}/pipewire-0"))
        })
}

enum ProbeCommand {
    Quit,
}

struct MetaHolder {
    // Drop the listener before the proxy it points at.
    _listener: MetadataListener,
    _metadata: Metadata,
}

#[derive(Default)]
struct ProbeState {
    nodes: HashMap<u32, AudioNode>,
    defaults: HashMap<String, String>,
    sync_done: bool,
    error: Option<String>,
    metadata_bound: bool,
    pending_sync: Option<pw::spa::utils::result::AsyncSeq>,
    sync_phase: u8,
}

fn probe_thread_main(
    config: ProbeConfig,
    cmd_rx: pw::channel::Receiver<ProbeCommand>,
    result_tx: oneshot::Sender<Result<PipeWireStatus, PipeWireError>>,
) {
    pw::init();
    let started = Instant::now();

    let main_loop = match MainLoopRc::new(None) {
        Ok(main_loop) => main_loop,
        Err(e) => {
            let _ = result_tx.send(Err(PipeWireError::unavailable(format!(
                "create PipeWire main loop: {e}"
            ))));
            return;
        }
    };

    let quit_loop = main_loop.clone();
    let _cmd_listener = cmd_rx.attach(main_loop.loop_(), move |_| quit_loop.quit());

    let mut context_props = properties! {
        *pw::keys::APP_NAME => "ozma-pipewire-probe",
    };
    if let Some(remote) = config.remote.as_deref() {
        context_props.insert(*pw::keys::REMOTE_NAME, remote);
    }
    let context = match pw::context::ContextRc::new(&main_loop, Some(context_props)) {
        Ok(context) => context,
        Err(e) => {
            let _ = result_tx.send(Err(PipeWireError::unavailable(format!(
                "create PipeWire context: {e}"
            ))));
            return;
        }
    };
    let core = match context.connect_rc(None) {
        Ok(core) => core,
        Err(e) => {
            let _ = result_tx.send(Err(PipeWireError::unavailable(format!(
                "connect to PipeWire: {e}"
            ))));
            return;
        }
    };
    let registry = match core.get_registry_rc() {
        Ok(registry) => registry,
        Err(e) => {
            let _ = result_tx.send(Err(PipeWireError::unavailable(format!(
                "get PipeWire registry: {e}"
            ))));
            return;
        }
    };

    let state = Rc::new(RefCell::new(ProbeState::default()));
    let metadata_holders: Rc<RefCell<Vec<MetaHolder>>> = Rc::new(RefCell::new(Vec::new()));

    let registry_for_bind = registry.clone();
    let state_for_global = Rc::clone(&state);
    let holders_for_global = Rc::clone(&metadata_holders);
    let _registry_listener = registry
        .add_listener_local()
        .global(move |global| {
            let Some(props) = global.props else {
                return;
            };
            match global.type_ {
                ObjectType::Node => {
                    let media_class = props.get("media.class").unwrap_or_default();
                    if !media_class.contains("Audio") {
                        return;
                    }
                    let name = props.get("node.name").unwrap_or_default().to_string();
                    if name.is_empty() {
                        return;
                    }
                    let description = props
                        .get("node.description")
                        .or_else(|| props.get("node.nick"))
                        .unwrap_or(&name)
                        .to_string();
                    if let Ok(mut state) = state_for_global.try_borrow_mut() {
                        state.nodes.insert(
                            global.id,
                            AudioNode {
                                id: global.id,
                                name,
                                description,
                                media_class: media_class.to_string(),
                                is_default: false,
                            },
                        );
                    }
                }
                ObjectType::Metadata => {
                    let already_bound = state_for_global
                        .try_borrow()
                        .map(|state| state.metadata_bound)
                        .unwrap_or(true);
                    if already_bound {
                        return;
                    }
                    let name = props.get("metadata.name").unwrap_or("default");
                    if name != "default" {
                        return;
                    }
                    let state_for_props = Rc::clone(&state_for_global);
                    match registry_for_bind.bind::<Metadata, _>(global) {
                        Ok(metadata) => {
                            let listener = metadata
                                .add_listener_local()
                                .property(move |_subject, key, _type, value| {
                                    if let (Some(key), Some(value)) = (key, value) {
                                        if key == "default.audio.source"
                                            || key == "default.audio.sink"
                                        {
                                            if let Ok(mut state) = state_for_props.try_borrow_mut()
                                            {
                                                state
                                                    .defaults
                                                    .insert(key.to_string(), value.to_string());
                                            }
                                        }
                                    }
                                    0
                                })
                                .register();
                            if let Ok(mut holders) = holders_for_global.try_borrow_mut() {
                                holders.push(MetaHolder {
                                    _listener: listener,
                                    _metadata: metadata,
                                });
                            }
                            if let Ok(mut state) = state_for_global.try_borrow_mut() {
                                state.metadata_bound = true;
                            }
                        }
                        Err(e) => debug!("could not bind PipeWire metadata: {e}"),
                    }
                }
                _ => {}
            }
        })
        .global_remove({
            let state = Rc::clone(&state);
            move |id| {
                if let Ok(mut state) = state.try_borrow_mut() {
                    state.nodes.remove(&id);
                }
            }
        })
        .register();

    let pending = match core.sync(0) {
        Ok(seq) => seq,
        Err(e) => {
            let _ = result_tx.send(Err(PipeWireError::unavailable(format!(
                "PipeWire core sync failed: {e}"
            ))));
            return;
        }
    };
    if let Ok(mut state) = state.try_borrow_mut() {
        state.pending_sync = Some(pending);
    }

    let state_for_done = Rc::clone(&state);
    let quit_on_done = main_loop.clone();
    let core_for_done = core.clone();
    let state_for_error = Rc::clone(&state);
    let quit_on_error = main_loop.clone();
    let _core_listener = core
        .add_listener_local()
        .done(move |id, seq| {
            if id != pw::core::PW_ID_CORE {
                return;
            }
            let mut quit = false;
            let mut next_sync = false;
            if let Ok(mut state) = state_for_done.try_borrow_mut() {
                if state.pending_sync == Some(seq) {
                    // The metadata proxy was bound after the first sync was
                    // queued.  Round-trip once more so its `default.*`
                    // properties have been delivered before we quit.
                    if state.metadata_bound && state.sync_phase == 0 {
                        state.sync_phase = 1;
                        next_sync = true;
                    } else {
                        state.sync_done = true;
                        quit = true;
                    }
                }
            }
            if next_sync {
                match core_for_done.sync(0) {
                    Ok(seq) => {
                        if let Ok(mut state) = state_for_done.try_borrow_mut() {
                            state.pending_sync = Some(seq);
                        }
                    }
                    Err(e) => {
                        if let Ok(mut state) = state_for_done.try_borrow_mut() {
                            state.error = Some(format!("PipeWire metadata sync failed: {e}"));
                        }
                        quit = true;
                    }
                }
            }
            if quit {
                quit_on_done.quit();
            }
        })
        .error(move |_id, _seq, _res, message| {
            if let Ok(mut state) = state_for_error.try_borrow_mut() {
                state.error = Some(message.to_string());
            }
            quit_on_error.quit();
        })
        .register();

    main_loop.run();

    let status = {
        let state = state.borrow();
        if let Some(error) = &state.error {
            let _ = result_tx.send(Err(PipeWireError::unavailable(error.clone())));
            return;
        }
        let default_source = state
            .defaults
            .get("default.audio.source")
            .and_then(|value| parse_default_name(value));
        let default_sink = state
            .defaults
            .get("default.audio.sink")
            .and_then(|value| parse_default_name(value));

        let mut sources: Vec<AudioNode> = state
            .nodes
            .values()
            .filter(|node| node.is_source())
            .cloned()
            .collect();
        let mut sinks: Vec<AudioNode> = state
            .nodes
            .values()
            .filter(|node| node.is_sink())
            .cloned()
            .collect();
        for node in &mut sources {
            node.is_default = default_source.as_deref() == Some(node.name.as_str());
        }
        for node in &mut sinks {
            node.is_default = default_sink.as_deref() == Some(node.name.as_str());
        }
        sources.sort_by(|a, b| a.name.cmp(&b.name));
        sinks.sort_by(|a, b| a.name.cmp(&b.name));

        PipeWireStatus {
            reachable: true,
            remote: configured_remote(config.remote.clone()),
            default_source,
            default_sink,
            sources,
            sinks,
            probe_micros: started.elapsed().as_micros() as u64,
            error: None,
        }
    };

    let _ = result_tx.send(Ok(status));
}

/// Parse `{"name":"alsa_output..."}` (or a bare name) from metadata.
fn parse_default_name(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return None;
    }
    if let Ok(json) = serde_json::from_str::<serde_json::Value>(trimmed) {
        return json
            .get("name")
            .and_then(|name| name.as_str())
            .map(str::to_string);
    }
    Some(trimmed.to_string())
}

#[cfg(test)]
mod tests {
    use super::parse_default_name;
    use crate::types::AudioNode;
    use crate::PipeWireStatus;

    #[test]
    fn parses_metadata_json_and_bare_names() {
        assert_eq!(
            parse_default_name(r#"{"name":"alsa_output.pci-0000.analog-stereo"}"#).as_deref(),
            Some("alsa_output.pci-0000.analog-stereo")
        );
        assert_eq!(
            parse_default_name("alsa_input.usb-mic").as_deref(),
            Some("alsa_input.usb-mic")
        );
        assert_eq!(parse_default_name("  "), None);
        assert_eq!(parse_default_name("{}"), None);
    }

    #[test]
    fn unreachable_status_carries_actionable_error() {
        let error = crate::PipeWireError::unavailable("Host is down");
        let status = PipeWireStatus::unreachable(Some("pipewire-0".to_string()), &error);
        assert!(!status.reachable);
        assert!(!status.has_audio());
        assert!(status.summary().contains("unreachable"));
        let detail = status.error.expect("error is recorded");
        assert_eq!(detail.code, "pipewire.unavailable");
        assert!(!detail.remediation.is_empty());
    }

    #[test]
    fn status_summary_counts_graph_nodes() {
        let node = |id, name: &str, class: &str| AudioNode {
            id,
            name: name.to_string(),
            description: name.to_string(),
            media_class: class.to_string(),
            is_default: false,
        };
        let status = PipeWireStatus {
            reachable: true,
            remote: Some("pipewire-0".to_string()),
            default_source: Some("mic".to_string()),
            default_sink: None,
            sources: vec![node(1, "mic", "Audio/Source")],
            sinks: vec![node(2, "speaker", "Audio/Sink")],
            probe_micros: 100,
            error: None,
        };
        assert!(status.has_audio());
        let summary = status.summary();
        assert!(summary.contains("1 source(s)"));
        assert!(summary.contains("1 sink(s)"));
        assert!(summary.contains("mic"));
    }
}
