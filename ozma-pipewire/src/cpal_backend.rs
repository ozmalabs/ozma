//! CPAL fallback backend for Windows and macOS.
//!
//! CPAL does not expose a graph model with explicit links, so this backend
//! only supports device enumeration.  `create_link` and `destroy_link`
//! return `Err` with a clear message.

use async_trait::async_trait;
use cpal::traits::{DeviceTrait, HostTrait};
use tokio::sync::broadcast;
use tracing::info;

use crate::types::{AudioEvent, AudioLink, AudioNode, GraphSnapshot, LinkRequest};
use crate::AudioRouter;

/// CPAL-backed [`AudioRouter`] for Windows and macOS.
pub struct CpalRouter {
    event_tx: broadcast::Sender<AudioEvent>,
}

impl CpalRouter {
    pub async fn new() -> anyhow::Result<Self> {
        let (event_tx, _) = broadcast::channel(64);
        info!("CPAL audio router ready (enumerate-only)");
        Ok(Self { event_tx })
    }

    fn enumerate_nodes() -> anyhow::Result<(Vec<AudioNode>, Option<String>, Option<String>)> {
        let host = cpal::default_host();

        let default_out = host
            .default_output_device()
            .and_then(|d| d.name().ok());
        let default_in = host
            .default_input_device()
            .and_then(|d| d.name().ok());

        let mut nodes = Vec::new();
        let mut id: u32 = 0;

        for device in host.output_devices()? {
            let name = device.name().unwrap_or_else(|_| format!("output-{id}"));
            nodes.push(AudioNode {
                id,
                is_default: Some(&name) == default_out.as_ref(),
                description: name.clone(),
                name,
                media_class: "Audio/Sink".into(),
            });
            id += 1;
        }

        for device in host.input_devices()? {
            let name = device.name().unwrap_or_else(|_| format!("input-{id}"));
            nodes.push(AudioNode {
                id,
                is_default: Some(&name) == default_in.as_ref(),
                description: name.clone(),
                name,
                media_class: "Audio/Source".into(),
            });
            id += 1;
        }

        Ok((nodes, default_out, default_in))
    }
}

#[async_trait]
impl AudioRouter for CpalRouter {
    async fn snapshot(&self) -> anyhow::Result<GraphSnapshot> {
        let (nodes, default_sink, default_source) =
            tokio::task::spawn_blocking(Self::enumerate_nodes).await??;
        Ok(GraphSnapshot {
            nodes,
            links: vec![],
            default_sink,
            default_source,
        })
    }

    async fn create_link(&self, _req: LinkRequest) -> anyhow::Result<AudioLink> {
        anyhow::bail!("link management is not supported on this platform (CPAL backend)")
    }

    async fn destroy_link(&self, _link_id: u32) -> anyhow::Result<()> {
        anyhow::bail!("link management is not supported on this platform (CPAL backend)")
    }

    fn subscribe(&self) -> broadcast::Receiver<AudioEvent> {
        self.event_tx.subscribe()
    }
}
