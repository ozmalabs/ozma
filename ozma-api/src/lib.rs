// SPDX-License-Identifier: AGPL-3.0-only
//! Axum API for the Ozma node contract.
//!
//! The router is deliberately independent of the session and display
//! implementations: it talks to a [`NodeBackend`]. That keeps the HTTP layer
//! testable with a deterministic in-memory backend and lets the node swap in
//! the real session manager without rewriting routes.

#![forbid(unsafe_code)]

pub mod error;
pub mod state;
pub mod router;

pub use error::ApiFailure;
pub use router::router;
pub use state::{MemoryBackend, NodeBackend};
