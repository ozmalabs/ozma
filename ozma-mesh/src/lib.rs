//! # ozma-mesh
//!
//! WireGuard mesh networking (userspace boringtun or Linux kernel) + mDNS peer
//! discovery for ozma nodes.
//!
//! ## Crate layout
//!
//! | Module | Contents |
//! |--------|----------|
//! | [`error`] | [`MeshError`] enum and `Result<T>` alias |
//! | [`node`]  | [`MeshNode`], [`WgPublicKey`], [`WgPrivateKey`] |
//! | [`discovery`] | mDNS browse, advertisement validation, dedupe/expiry |
//! | [`manager`] | [`MeshManager`] — userspace lifecycle, peer table, mDNS |
//! | [`kernel`] | [`KernelWireguard`] — Linux `wireguard` link, peers, routes, `wg` status |
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

pub mod dataplane;
pub mod discovery;
pub mod error;
pub mod kernel;
pub mod keys;
pub mod manager;
pub mod node;
pub mod pump;
pub mod tunnel;

// Convenience re-exports — `use ozma_mesh::{MeshManager, MeshNode, ...}`.
pub use dataplane::{
    CommandRunner, CommandSpec, DataplaneConfig, DataplaneError, DataplaneState, DataplaneStatus,
    FileOpener, MeshDataplane, NatConfig, Sysfs,
};
pub use discovery::{
    DiscoveredPeer, DiscoveryAction, DiscoveryConfig, DiscoveryCounters, DiscoveryEngine,
    DiscoveryStatus, MdnsBrowser, RawAdvertisement, TrustedPeer, MDNS_FQDN, MDNS_SERVICE_TYPE,
};
pub use error::{MeshError, Result};
pub use kernel::{
    parse_wg_dump, redact_keys, KernelWgSettings, KernelWgStatus, KernelWireguard, WgDump,
    WgDumpPeer, DEFAULT_KERNEL_INTERFACE, DEFAULT_KERNEL_KEEPALIVE, DEFAULT_KERNEL_MTU,
    HANDSHAKE_REJECT_AFTER_SECS, WIREGUARD_MODULE_PATH,
};
pub use manager::{MeshAdvertiser, MeshManager, PeerChange, PeerOrigin, PeerStatus};
pub use node::{MeshNode, WgPrivateKey, WgPublicKey};
pub use pump::{
    ManagerPacketIo, PacketPump, PumpConfig, PumpError, PumpState, PumpStats, PumpStatus,
    SystemTunProvider, TunIo, TunProvider, TunnelIo,
};
pub use tunnel::WgTunnel;
