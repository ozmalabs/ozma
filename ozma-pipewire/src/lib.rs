//! # ozma-pipewire
//!
//! PipeWire audio routing for the Ozma agent.
//!
//! ## Platform support
//!
//! | Platform | Backend           | Link management    |
//! |----------|-------------------|--------------------|
//! | Linux    | PipeWire (native) | ✓                  |
//! | Windows  | CPAL              | ✗ (enumerate only) |
//! | macOS    | CPAL              | ✗ (enumerate only) |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use ozma_pipewire::build_router;
//!
//! #[tokio::main]
//! async fn main() {
//!     let router = build_router().await.expect("audio router init failed");
//!     let snap = router.snapshot().await.expect("snapshot failed");
//!     for node in &snap.nodes {
//!         println!("{:?}", node);
//!     }
//! }
//! ```

pub mod types;

#[cfg(target_os = "linux")]
mod pipewire_backend;

#[cfg(not(target_os = "linux"))]
mod cpal_backend;

use async_trait::async_trait;
use std::sync::Arc;
use tokio::sync::broadcast;

pub use types::{AudioEvent, AudioLink, AudioNode, GraphSnapshot, LinkRequest};

/// Core trait implemented by every audio routing backend.
#[async_trait]
pub trait AudioRouter: Send + Sync {
    /// Return a point-in-time snapshot of the audio graph.
    async fn snapshot(&self) -> anyhow::Result<GraphSnapshot>;

    /// Create a link between two nodes.
    ///
    /// Returns the [`AudioLink`] that was created.
    /// Returns an error on platforms that do not support link management.
    async fn create_link(&self, req: LinkRequest) -> anyhow::Result<AudioLink>;

    /// Destroy a previously created link by its ID.
    async fn destroy_link(&self, link_id: u32) -> anyhow::Result<()>;

    /// Subscribe to live graph-change events.
    ///
    /// The returned receiver will receive [`AudioEvent`]s as they occur.
    /// Lagged receivers will see a [`broadcast::error::RecvError::Lagged`] error
    /// and should call [`snapshot`] to resync.
    fn subscribe(&self) -> broadcast::Receiver<AudioEvent>;
}

/// Construct the best available [`AudioRouter`] for the current platform.
///
/// On Linux this returns a `PipeWireRouter`; on other platforms it returns
/// a `CpalRouter`.
pub async fn build_router() -> anyhow::Result<Arc<dyn AudioRouter>> {
    #[cfg(target_os = "linux")]
    {
        let r = pipewire_backend::PipeWireRouter::new().await?;
        return Ok(Arc::new(r));
    }

    #[cfg(not(target_os = "linux"))]
    {
        let r = cpal_backend::CpalRouter::new().await?;
        return Ok(Arc::new(r));
    }
}
