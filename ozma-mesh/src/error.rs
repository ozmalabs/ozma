//! Error types for the ozma-mesh crate.

use thiserror::Error;

/// All errors that can be produced by the ozma-mesh crate.
#[derive(Debug, Error, PartialEq)]
pub enum MeshError {
    #[error("WireGuard key error: {0}")]
    KeyError(String),

    #[error("WireGuard tunnel error: {0}")]
    TunnelError(String),

    #[error("Peer not found: {0}")]
    PeerNotFound(String),

    #[error("Peer already exists: {0}")]
    PeerAlreadyExists(String),

    #[error("Invalid mesh IP: {0}")]
    InvalidIp(String),

    #[error("mDNS error: {0}")]
    Mdns(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialisation error: {0}")]
    Serialisation(#[from] serde_json::Error),

    #[error("Base64 decode error: {0}")]
    Base64(#[from] base64::DecodeError),
}

/// Convenience `Result` alias for this crate.
pub type Result<T> = std::result::Result<T, MeshError>;
