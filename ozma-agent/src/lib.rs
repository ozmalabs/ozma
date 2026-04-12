//! ozma-agent — Connect client library.
//!
//! Exposes [`ConnectClient`] for use by the agent binary and integration tests.

pub mod client;
pub mod metrics;
pub mod relay;

pub use client::{AgentConfig, ConnectClient};
