//! Shared types for the ozma audio routing layer.

use serde::{Deserialize, Serialize};

/// A single audio node visible in the graph (source or sink).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioNode {
    /// Numeric graph ID (PipeWire object ID on Linux, index on other platforms).
    pub id: u32,
    /// `node.name` property — stable identifier used for routing.
    pub name: String,
    /// `node.description` / `node.nick` — human-readable label.
    pub description: String,
    /// PipeWire `media.class` string, e.g. `"Audio/Sink"`, `"Audio/Source"`.
    pub media_class: String,
    /// Whether this node is the current system default for its class.
    pub is_default: bool,
}

impl AudioNode {
    /// Returns `true` if this node is an audio sink.
    pub fn is_sink(&self) -> bool {
        self.media_class.contains("Sink")
    }

    /// Returns `true` if this node is an audio source.
    pub fn is_source(&self) -> bool {
        self.media_class.contains("Source")
    }
}

/// A directed link between two ports in the audio graph.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AudioLink {
    /// PipeWire link object ID.
    pub id: u32,
    /// Output (source) node ID.
    pub output_node: u32,
    /// Output port ID.
    pub output_port: u32,
    /// Input (sink) node ID.
    pub input_node: u32,
    /// Input port ID.
    pub input_port: u32,
}

/// Events emitted by the audio graph monitor.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AudioEvent {
    /// A new node appeared in the graph.
    NodeAdded(AudioNode),
    /// A node was removed from the graph.
    NodeRemoved { id: u32 },
    /// A new link was created.
    LinkAdded(AudioLink),
    /// A link was destroyed.
    LinkRemoved { id: u32 },
    /// The default sink or source changed.
    DefaultChanged {
        media_class: String,
        node_name: String,
    },
}

/// Request to create a link between two nodes.
///
/// The backend resolves the first matching output port on `output_node`
/// and the first matching input port on `input_node`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LinkRequest {
    pub output_node_id: u32,
    pub input_node_id: u32,
}

/// Summary of the current audio graph state — returned by
/// [`AudioRouter::snapshot`].
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GraphSnapshot {
    pub nodes: Vec<AudioNode>,
    pub links: Vec<AudioLink>,
    pub default_sink: Option<String>,
    pub default_source: Option<String>,
}
