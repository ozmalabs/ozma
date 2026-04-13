//! Linux PipeWire backend.
//!
//! Uses the `pipewire` crate (0.9.2) to:
//!   - Enumerate audio nodes and links via the registry.
//!   - Monitor graph changes (node/link add/remove, metadata changes).
//!   - Create and destroy links between nodes.
//!
//! The PipeWire main loop runs on a dedicated OS thread so it never blocks
//! the Tokio executor.  All public methods communicate with that thread via
//! `tokio::sync` channels.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex},
    thread,
};

use async_trait::async_trait;
use pipewire::{
    context::{Context, ContextRc},
    core::Core,
    link::Link,
    main_loop::{MainLoop, MainLoopRc},
    properties::properties,
    registry::GlobalObject,
    spa::utils::dict::DictRef,
    types::ObjectType,
};
use tokio::sync::{broadcast, oneshot};
use tracing::{debug, info, warn};

use crate::types::{AudioEvent, AudioLink, AudioNode, GraphSnapshot, LinkRequest};
use crate::AudioRouter;

// ── Internal command sent from async tasks → PW thread ────────────────────

enum PwCommand {
    CreateLink {
        output_node: u32,
        input_node: u32,
        reply: oneshot::Sender<anyhow::Result<AudioLink>>,
    },
    DestroyLink {
        link_id: u32,
        reply: oneshot::Sender<anyhow::Result<()>>,
    },
    Snapshot {
        reply: oneshot::Sender<GraphSnapshot>,
    },
}

// ── Shared graph state (guarded by a std Mutex — accessed from PW thread) ─

#[derive(Default)]
struct GraphState {
    nodes: HashMap<u32, AudioNode>,
    links: HashMap<u32, AudioLink>,
    default_sink: Option<String>,
    default_source: Option<String>,
    /// node.name → id reverse index
    name_to_id: HashMap<String, u32>,
    /// node id → list of (port_id, direction) where direction: "out" | "in"
    ports: HashMap<u32, Vec<(u32, String)>>,
}

impl GraphState {
    fn snapshot(&self) -> GraphSnapshot {
        GraphSnapshot {
            nodes: self.nodes.values().cloned().collect(),
            links: self.links.values().cloned().collect(),
            default_sink: self.default_sink.clone(),
            default_source: self.default_source.clone(),
        }
    }
}

// ── PipeWireRouter ─────────────────────────────────────────────────────────

/// PipeWire-backed [`AudioRouter`] for Linux.
pub struct PipeWireRouter {
    cmd_tx: tokio::sync::mpsc::UnboundedSender<PwCommand>,
    event_tx: broadcast::Sender<AudioEvent>,
    /// Shared graph state — readable from async context without going through
    /// the PW thread (snapshot queries hit this directly).
    state: Arc<Mutex<GraphState>>,
}

impl PipeWireRouter {
    /// Initialise the PipeWire context and start the monitor thread.
    pub async fn new() -> anyhow::Result<Self> {
        pipewire::init();

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::unbounded_channel::<PwCommand>();
        let (event_tx, _) = broadcast::channel::<AudioEvent>(256);
        let state = Arc::new(Mutex::new(GraphState::default()));

        let event_tx_clone = event_tx.clone();
        let state_clone = Arc::clone(&state);

        // The PipeWire main loop must run on its own thread.
        let (ready_tx, ready_rx) = oneshot::channel::<anyhow::Result<()>>();

        thread::Builder::new()
            .name("ozma-pipewire".into())
            .spawn(move || {
                pw_thread_main(cmd_rx, event_tx_clone, state_clone, ready_tx);
            })?;

        // Wait until the PW thread signals it is ready (or failed).
        ready_rx.await??;

        info!("PipeWire audio router ready");
        Ok(Self { cmd_tx, event_tx, state })
    }
}

#[async_trait]
impl AudioRouter for PipeWireRouter {
    async fn snapshot(&self) -> anyhow::Result<GraphSnapshot> {
        // Read directly from shared state — no round-trip to PW thread needed.
        Ok(self.state.lock().unwrap().snapshot())
    }

    async fn create_link(&self, req: LinkRequest) -> anyhow::Result<AudioLink> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(PwCommand::CreateLink {
            output_node: req.output_node_id,
            input_node: req.input_node_id,
            reply: tx,
        })?;
        rx.await?
    }

    async fn destroy_link(&self, link_id: u32) -> anyhow::Result<()> {
        let (tx, rx) = oneshot::channel();
        self.cmd_tx.send(PwCommand::DestroyLink { link_id, reply: tx })?;
        rx.await?
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.event_tx.subscribe()
    }
}

// ── PipeWire thread ────────────────────────────────────────────────────────

fn pw_thread_main(
    cmd_rx: tokio::sync::mpsc::UnboundedReceiver<PwCommand>,
    event_tx: broadcast::Sender<AudioEvent>,
    state: Arc<Mutex<GraphState>>,
    ready_tx: oneshot::Sender<anyhow::Result<()>>,
) {
    let main_loop = match MainLoopRc::new(None) {
        Ok(ml) => ml,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!("PipeWire MainLoop::new: {e}")));
            return;
        }
    };

    let context = match ContextRc::new(&main_loop, None) {
        Ok(ctx) => ctx,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!("PipeWire Context::new: {e}")));
            return;
        }
    };

    let core = match context.connect_rc(None) {
        Ok(c) => c,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!("PipeWire Core::connect: {e}")));
            return;
        }
    };

    let registry = match core.get_registry() {
        Ok(r) => r,
        Err(e) => {
            let _ = ready_tx.send(Err(anyhow::anyhow!("PipeWire get_registry: {e}")));
            return;
        }
    };

    // Clones for use inside registry listener closures.
    let state_add = Arc::clone(&state);
    let state_rem = Arc::clone(&state);
    let event_tx_add = event_tx.clone();
    let event_tx_rem = event_tx.clone();

    // ── Registry listener ──────────────────────────────────────────────────
    let _reg_listener = registry
        .add_listener_local()
        .global(move |global: &GlobalObject<&DictRef>| {
            handle_global(global, &state_add, &event_tx_add);
        })
        .global_remove(move |id| {
            handle_global_remove(id, &state_rem, &event_tx_rem);
        })
        .register();

    // Signal ready before entering the loop.
    let _ = ready_tx.send(Ok(()));

    // ── Command pump ───────────────────────────────────────────────────────
    // We use a non-blocking pipe to wake the PW loop when a command arrives.
    // The read end is registered as a PW I/O source; the write end is held
    // by a small shim that forwards from the mpsc channel.
    //
    // For this initial implementation we use the PW loop's built-in
    // `add_timer` to poll the command channel every 5 ms.  A future
    // improvement is to use `main_loop.add_io()` with a real eventfd.
    let cmd_rx = std::cell::RefCell::new(cmd_rx);
    let core_cmd = core.clone();
    let state_cmd = Arc::clone(&state);

    let timer = main_loop.loop_().add_timer(move |_| {
        // Drain all pending commands without blocking.
        loop {
            match cmd_rx.borrow_mut().try_recv() {
                Ok(cmd) => handle_command(cmd, &core_cmd, &state_cmd),
                Err(tokio::sync::mpsc::error::TryRecvError::Empty) => break,
                Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => break,
            }
        }
    });

    // Arm the timer: fire every 5 ms.
    let _ = timer.update_timer(
        Some(std::time::Duration::from_millis(5)),
        Some(std::time::Duration::from_millis(5)),
    );

    main_loop.run();
}

// ── Command handler (runs on PW thread) ───────────────────────────────────

fn handle_command(cmd: PwCommand, core: &Core, state: &Arc<Mutex<GraphState>>) {
    match cmd {
        PwCommand::Snapshot { reply } => {
            let snap = state.lock().unwrap().snapshot();
            let _ = reply.send(snap);
        }
        PwCommand::CreateLink { output_node, input_node, reply } => {
            let result = create_link_sync(core, state, output_node, input_node);
            let _ = reply.send(result);
        }
        PwCommand::DestroyLink { link_id, reply } => {
            // TODO: destroying a link requires storing the Link proxy from
            // create_link_sync. For now return an error — the link will be
            // cleaned up when the PipeWire session ends.
            let _ = reply.send(Err(anyhow::anyhow!(
                "DestroyLink {link_id}: proxy-based destroy not yet implemented"
            )));
        }
    }
}

// ── Registry global handler ────────────────────────────────────────────────

fn handle_global(
    global: &GlobalObject<&DictRef>,
    state: &Arc<Mutex<GraphState>>,
    event_tx: &broadcast::Sender<AudioEvent>,
) {
    let props = match global.props {
        Some(p) => p,
        None => return,
    };

    match global.type_ {
        ObjectType::Node => handle_node_global(global.id, props, state, event_tx),
        ObjectType::Link => handle_link_global(global.id, props, state, event_tx),
        ObjectType::Port => handle_port_global(global.id, props, state),
        ObjectType::Metadata => {
            let name = props.get("metadata.name").unwrap_or("");
            debug!("PW metadata global: name={name}");
        }
        _ => {}
    }
}

fn handle_node_global(
    id: u32,
    props: &DictRef,
    state: &Arc<Mutex<GraphState>>,
    event_tx: &broadcast::Sender<AudioEvent>,
) {
    let media_class = props.get("media.class").unwrap_or("").to_string();
    if media_class.is_empty() || !media_class.contains("Audio") {
        return;
    }

    let name = props.get("node.name").unwrap_or("").to_string();
    let description = props
        .get("node.description")
        .or_else(|| props.get("node.nick"))
        .unwrap_or(&name)
        .to_string();

    let node = AudioNode {
        id,
        name: name.clone(),
        description,
        media_class,
        is_default: false, // updated when metadata arrives
    };

    {
        let mut g = state.lock().unwrap();
        g.name_to_id.insert(name, id);
        g.nodes.insert(id, node.clone());
    }

    let _ = event_tx.send(AudioEvent::NodeAdded(node));
    debug!("PW node added: id={id}");
}

fn handle_link_global(
    id: u32,
    props: &DictRef,
    state: &Arc<Mutex<GraphState>>,
    event_tx: &broadcast::Sender<AudioEvent>,
) {
    let parse = |key: &str| -> u32 {
        props.get(key).and_then(|v| v.parse().ok()).unwrap_or(0)
    };

    let link = AudioLink {
        id,
        output_node: parse("link.output.node"),
        output_port: parse("link.output.port"),
        input_node: parse("link.input.node"),
        input_port: parse("link.input.port"),
    };

    state.lock().unwrap().links.insert(id, link.clone());
    let _ = event_tx.send(AudioEvent::LinkAdded(link));
    debug!("PW link added: id={id}");
}

fn handle_port_global(
    id: u32,
    props: &DictRef,
    state: &Arc<Mutex<GraphState>>,
) {
    let node_id: u32 = props
        .get("node.id")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if node_id == 0 {
        return;
    }
    let direction = props.get("port.direction").unwrap_or("").to_string();
    debug!("PW port added: id={id} node={node_id} dir={direction}");
    state
        .lock()
        .unwrap()
        .ports
        .entry(node_id)
        .or_default()
        .push((id, direction));
}

fn handle_global_remove(
    id: u32,
    state: &Arc<Mutex<GraphState>>,
    event_tx: &broadcast::Sender<AudioEvent>,
) {
    let mut g = state.lock().unwrap();

    if g.nodes.remove(&id).is_some() {
        g.name_to_id.retain(|_, v| *v != id);
        g.ports.remove(&id);
        let _ = event_tx.send(AudioEvent::NodeRemoved { id });
        debug!("PW node removed: id={id}");
        return;
    }

    if g.links.remove(&id).is_some() {
        let _ = event_tx.send(AudioEvent::LinkRemoved { id });
        debug!("PW link removed: id={id}");
    }
}

// ── Link creation (runs on PW thread) ─────────────────────────────────────

/// Create a PipeWire link between the first matching output port of
/// `output_node_id` and the first matching input port of `input_node_id`.
fn create_link_sync(
    core: &Core,
    state: &Arc<Mutex<GraphState>>,
    output_node_id: u32,
    input_node_id: u32,
) -> anyhow::Result<AudioLink> {
    let (out_port, in_port) = {
        let g = state.lock().unwrap();

        let out_port = g
            .ports
            .get(&output_node_id)
            .and_then(|ports| ports.iter().find(|(_, d)| d == "out").map(|(id, _)| *id))
            .ok_or_else(|| anyhow::anyhow!("no output port on node {output_node_id}"))?;

        let in_port = g
            .ports
            .get(&input_node_id)
            .and_then(|ports| ports.iter().find(|(_, d)| d == "in").map(|(id, _)| *id))
            .ok_or_else(|| anyhow::anyhow!("no input port on node {input_node_id}"))?;

        (out_port, in_port)
    };

    let props = properties! {
        "link.output.node" => output_node_id.to_string(),
        "link.output.port" => out_port.to_string(),
        "link.input.node"  => input_node_id.to_string(),
        "link.input.port"  => in_port.to_string(),
        "object.linger"    => "1",
    };

    // `core.create_object` returns a proxy; the registry global callback
    // will fire and populate the link into GraphState with the real ID.
    let _proxy: Link = core.create_object("link-factory", &props)?;

    // Return a provisional AudioLink — the authoritative ID arrives via
    // AudioEvent::LinkAdded from the registry callback.
    Ok(AudioLink {
        id: 0,
        output_node: output_node_id,
        output_port: out_port,
        input_node: input_node_id,
        input_port: in_port,
    })
}
