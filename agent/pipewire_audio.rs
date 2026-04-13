//! PipeWire audio management for Ozma Desktop Agent
//!
//! This module provides Rust bindings for PipeWire audio routing, using the official
//! pipewire crate. It handles:
//! - Creating/destroying virtual audio sinks
//! - Monitoring audio graph changes
//! - Listing audio sources and sinks
//! - Creating links between audio nodes

use pipewire as pw;
use pw::prelude::*;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use anyhow::Result;

/// Information about an audio node (source or sink)
#[derive(Debug, Clone)]
pub struct AudioNode {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub media_class: String,
    pub is_default: bool,
}

/// Manages PipeWire audio operations
pub struct PipeWireAudioManager {
    /// PipeWire main loop
    _main_loop: pw::MainLoop,
    /// PipeWire context
    _context: pw::Context,
    /// PipeWire core
    core: pw::Core,
    /// Registry for monitoring nodes
    _registry: pw::Registry,
    /// Map of audio nodes
    nodes: Arc<Mutex<HashMap<u32, AudioNode>>>,
    /// Default sink name
    default_sink: Arc<Mutex<String>>,
    /// Default source name
    default_source: Arc<Mutex<String>>,
    /// Created objects for cleanup
    created_objects: Arc<Mutex<Vec<pw::proxy::Proxy>>>,
}

impl PipeWireAudioManager {
    /// Create a new PipeWire audio manager
    pub fn new() -> Result<Self> {
        let main_loop = pw::MainLoop::new(None)?;
        let context = pw::Context::new(&main_loop)?;
        let core = context.connect(None)?;
        
        let registry = core.get_registry()?;
        let nodes = Arc::new(Mutex::new(HashMap::new()));
        let default_sink = Arc::new(Mutex::new(String::new()));
        let default_source = Arc::new(Mutex::new(String::new()));
        let created_objects = Arc::new(Mutex::new(Vec::new()));
        
        let nodes_clone = nodes.clone();
        let default_sink_clone = default_sink.clone();
        let default_source_clone = default_source.clone();
        
        // Set up registry callbacks to monitor audio nodes
        let _listener = registry
            .add_listener_local()
            .global(move |global| {
                if global.type_ == pw::types::ObjectType::Node {
                    // This is a node, store its information
                    if let Some(props) = global.props.as_ref() {
                        let media_class = props.get("media.class").unwrap_or("").to_string();
                        if media_class.contains("Audio") {
                            let name = props.get("node.name").unwrap_or("").to_string();
                            let description = props.get("node.description")
                                .or_else(|| props.get("node.nick"))
                                .unwrap_or(props.get("node.name").unwrap_or(""))
                                .to_string();
                            
                            let mut nodes_lock = nodes_clone.lock().unwrap();
                            nodes_lock.insert(global.id, AudioNode {
                                id: global.id,
                                name: name.clone(),
                                description,
                                media_class: media_class.clone(),
                                is_default: false,
                            });
                        }
                    }
                } else if global.type_ == pw::types::ObjectType::Metadata {
                    // Handle metadata for default sink/source tracking
                    // This would need to be expanded to properly track defaults
                }
            })
            .global_remove(move |id| {
                // Remove node from our tracking
                let mut nodes_lock = nodes_clone.lock().unwrap();
                nodes_lock.remove(&id);
            })
            .register();
        
        Ok(Self {
            _main_loop: main_loop,
            _context: context,
            core,
            _registry: registry,
            nodes,
            default_sink,
            default_source,
            created_objects,
        })
    }
    
    /// Start monitoring the PipeWire graph for changes
    pub fn start_monitoring(&self) {
        // The callbacks are already set up in new(), so monitoring is active
    }
    
    /// List all audio nodes (sources and sinks)
    pub fn list_audio_nodes(&self) -> Vec<AudioNode> {
        let nodes_lock = self.nodes.lock().unwrap();
        let default_sink_lock = self.default_sink.lock().unwrap();
        let default_source_lock = self.default_source.lock().unwrap();
        
        nodes_lock.values().map(|node| {
            let is_default = (node.name == *default_sink_lock && node.media_class.contains("Sink")) ||
                           (node.name == *default_source_lock && node.media_class.contains("Source"));
            AudioNode {
                id: node.id,
                name: node.name.clone(),
                description: node.description.clone(),
                media_class: node.media_class.clone(),
                is_default,
            }
        }).collect()
    }
    
    /// Create a null sink (virtual audio output)
    pub fn create_null_sink(&self, name: &str, description: &str) -> Result<u32> {
        let props = pw::properties::Properties::new();
        props.set("node.name", &format!("ozma-{}", name));
        props.set("node.description", description);
        props.set("media.class", "Audio/Sink");
        props.set("factory.name", "support.null-audio-sink");
        
        // Create the null sink
        let node = self.core.create_object::<pw::node::Node>("adapter", &props)?;
        
        // Store the created object for cleanup
        {
            let mut created_objects = self.created_objects.lock().unwrap();
            created_objects.push(node.clone().upcast());
        }
        
        Ok(node.id())
    }
    
    /// Create a link between two audio nodes
    pub fn create_link(&self, output_node: u32, input_node: u32) -> Result<u32> {
        let props = pw::properties::Properties::new();
        props.set("link.output.node", &output_node.to_string());
        props.set("link.input.node", &input_node.to_string());
        props.set("object.session", "true");
        
        // Create the link
        let link = self.core.create_object::<pw::link::Link>("link_factory", &props)?;
        
        // Store the created object for cleanup
        {
            let mut created_objects = self.created_objects.lock().unwrap();
            created_objects.push(link.clone().upcast());
        }
        
        Ok(link.id())
    }
    
    /// Destroy a link between audio nodes
    pub fn destroy_link(&self, link_id: u32) -> Result<()> {
        // Find and remove the link from created_objects
        let mut created_objects = self.created_objects.lock().unwrap();
        if let Some(pos) = created_objects.iter().position(|obj| {
            // This is a simplified check - in practice you'd need to check the object type and ID
            true // This should be replaced with proper ID checking
        }) {
            let _link = created_objects.remove(pos);
            // The link will be destroyed when dropped
        }
        Ok(())
    }
    
    /// Update the default sink/source information
    pub fn update_defaults(&self, sink_name: &str, source_name: &str) {
        let mut default_sink_lock = self.default_sink.lock().unwrap();
        *default_sink_lock = sink_name.to_string();
        
        let mut default_source_lock = self.default_source.lock().unwrap();
        *default_source_lock = source_name.to_string();
    }
}

impl Drop for PipeWireAudioManager {
    fn drop(&mut self) {
        // Clean up created objects
        let created_objects = self.created_objects.lock().unwrap();
        for obj in created_objects.iter() {
            // Objects will be automatically cleaned up when dropped
            // In some cases you might want to explicitly destroy them
        }
    }
}
