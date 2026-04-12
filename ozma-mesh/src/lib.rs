//! ozma-mesh — WireGuard mesh networking + mDNS peer discovery.
//!
//! # Overview
//!
//! This crate provides the shared mesh-networking layer used by every ozma
//! node and the controller:
//!
//! * [`MeshNode`] — identity record (node ID, WireGuard public key, mesh IP).
//! * [`MeshManager`] — lifecycle manager: key generation, peer add/remove,
//!   WireGuard tunnel maintenance, mDNS advertising.
//!
//! # WireGuard backend
//!
//! Uses [boringtun](https://github.com/cloudflare/boringtun) — Cloudflare's
//! pure-Rust userspace WireGuard implementation.  No kernel module required.
//!
//! # mDNS
//!
//! Advertises `_ozma._udp.local` so nodes on the same LAN segment discover
//! each other without a central rendezvous server.

pub mod error;
pub mod manager;
pub mod node;

pub use error::MeshError;
pub use manager::MeshManager;
pub use node::{generate_keypair, MeshNode, WgPrivateKey, WgPublicKey};
