//! # ozma-mesh
//!
//! Userspace WireGuard mesh networking + mDNS peer discovery for ozma nodes.
//!
//! ## Crate layout
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`error`] | [`MeshError`] enum and `Result<T>` alias |
//! | [`node`]  | [`MeshNode`], [`WgPublicKey`], [`WgPrivateKey`] |
//! | [`manager`] | [`MeshManager`] — lifecycle, peer table, tunnels, mDNS |
//!
//! ## Quick start
//!
//! ```rust,no_run
//! use ozma_mesh::{MeshManager, MeshNode};
//! use ozma_mesh::error::Result;
//!
//! #[tokio::main]
//! async fn main() -> Result<()> {
//!     // Generate a node identity + WireGuard keypair.
//!     let (node, sk) = MeshNode::generate("my-node", "10.200.1.1", 51820);
//!
//!     // Start the manager (binds UDP socket, registers mDNS).
//!     let mgr = MeshManager::new(node, sk).await?;
//!
//!     // Start the background receive loop.
//!     mgr.start().await?;
//!
//!     // Add a remote peer (its identity is discovered via mDNS or the controller).
//!     let (peer_node, _peer_sk) = MeshNode::generate("peer", "10.200.2.1", 51821);
//!     mgr.add_peer(peer_node).await?;
//!
//!     // Initiate the WireGuard handshake.
//!     mgr.initiate_handshake("peer").await?;
//!
//!     Ok(())
//! }
//! ```

pub mod error;
pub mod manager;
pub mod node;

// Convenience re-exports — `use ozma_mesh::{MeshManager, MeshNode, ...}`.
pub use error::MeshError;
pub use manager::MeshManager;
pub use node::{MeshNode, WgPrivateKey, WgPublicKey};
