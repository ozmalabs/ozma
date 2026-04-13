//! PipeWire audio routing for Ozma Desktop Agent
//!
//! This module provides Rust bindings for PipeWire audio routing functionality,
//! including creating/destroying virtual sinks and monitoring audio graph changes.

use pipewire as pw;
use spa::pod::serialize::PodSerializer;
use spa::pod::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// Audio node information
#[derive(Debug, Clone)]
pub struct AudioNode {
    pub id: u32,
    pub name: String,
    pub description: String,
    pub media_class: String,
    pub is_default: bool,
}

/// PipeWire audio manager
pub struct PipeWireAudioManager {
    context: pw::Context,
    core: pw::Core,
    registry: pw::Registry,
    nodes: Arc<Mutex<HashMap<u32, AudioNode>>>,
}

impl PipeWireAudioManager {
    /// Create a new PipeWire audio manager
    pub fn new() -> Result<Self, Box<dyn std::error::Error>> {
        // Initialize PipeWire
        let context = pw::Context::new(&pw::MainLoop::new(None)?)?;
        let core = context.connect(None)?;
        let registry = core.get_registry()?;
        
        let manager = Self {
            context,
            core,
            registry,
            nodes: Arc::new(Mutex::new(HashMap::new())),
        };
        
        Ok(manager)
    }
    
    /// Start monitoring audio graph changes
    pub fn start_monitoring(&mut self) -> Result<(), Box<dyn std::error::Error>> {
        let nodes = self.nodes.clone();
        
        // Listen for global object events
        self.registry.add_listener_local(move |event| match event {
            pw::registry::Event::Global(global) => {
                // Handle new global objects
                if global.type_ == pw::types::Node {
                    // Parse node properties
                    let props = global.props.as_ref();
                    if let Some(props) = props {
                        let media_class = props.get("media.class").unwrap_or("").to_string();
                        if media_class.contains("Audio") {
                            let node = AudioNode {
                                id: global.id,
                                name: props.get("node.name").unwrap_or("").to_string(),
                                description: props.get("node.description")
                                    .or_else(|| props.get("node.nick"))
                                    .unwrap_or(props.get("node.name").unwrap_or(""))
                                    .to_string(),
                                media_class,
                                is_default: false, // Will be updated when we get metadata
                            };
                            
                            let mut nodes_lock = nodes.lock().unwrap();
                            nodes_lock.insert(global.id, node);
                        }
                    }
                }
            }
            pw::registry::Event::GlobalRemove(id) => {
                // Handle removed global objects
                let mut nodes_lock = nodes.lock().unwrap();
                nodes_lock.remove(id);
            }
        });
        
        Ok(())
    }
    
    /// List available audio sources and sinks
    pub fn list_audio_nodes(&self) -> Result<Vec<AudioNode>, Box<dyn std::error::Error>> {
        let nodes_lock = self.nodes.lock().unwrap();
        Ok(nodes_lock.values().cloned().collect())
    }
    
    /// Create a virtual null sink
    pub fn create_null_sink(&self, name: &str, description: &str) -> Result<u32, Box<dyn std::error::Error>> {
        // Create a null sink node
        let node = self.core.create_object(
            "adapter",
            &pw::properties::from([
                ("media.class", "Audio/Sink"),
                ("node.name", name),
                ("node.description", description),
            ]),
        )?;
        
        Ok(node.id())
    }
    
    /// Create a link between two audio nodes
    pub fn create_link(&self, output_node: u32, input_node: u32) -> Result<(), Box<dyn std::error::Error>> {
        // Create a link between nodes
        let _link = self.core.create_object(
            "link",
            &pw::properties::from([
                ("link.output.node", &output_node.to_string()),
                ("link.input.node", &input_node.to_string()),
            ]),
        )?;
        
        Ok(())
    }
    
    /// Destroy a link
    pub fn destroy_link(&self, link_id: u32) -> Result<(), Box<dyn std::error::Error>> {
        // Destroy the link
        // In a real implementation, you would need to keep track of link IDs
        // and call the appropriate PipeWire API to destroy the link
        Ok(())
    }
    
    /// Get the default audio sink
    pub fn get_default_sink(&self) -> Option<AudioNode> {
        let nodes_lock = self.nodes.lock().unwrap();
        nodes_lock.values()
            .find(|node| node.is_default && node.media_class.contains("Sink"))
            .cloned()
    }
    
    /// Get the default audio source
    pub fn get_default_source(&self) -> Option<AudioNode> {
        let nodes_lock = self.nodes.lock().unwrap();
        nodes_lock.values()
            .find(|node| node.is_default && node.media_class.contains("Source"))
            .cloned()
    }
}

impl Drop for PipeWireAudioManager {
    fn drop(&mut self) {
        // Clean up PipeWire resources
        // The context and core will be automatically dropped
    }
}
