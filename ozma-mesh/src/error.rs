use thiserror::Error;

/// Errors produced by the ozma-mesh crate.
#[derive(Debug, Error)]
pub enum MeshError {
    #[error("WireGuard key error: {0}")]
    KeyError(String),

    #[error("WireGuard tunnel error: {0}")]
    TunnelError(String),

    #[error("Peer not found: {0}")]
    PeerNotFound(String),

    #[error("Peer already exists: {0}")]
    PeerAlreadyExists(String),

    #[error("Invalid key: {0}")]
    InvalidKey(String),

    #[error("mDNS error: {0}")]
    MdnsError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialisation error: {0}")]
    Serde(#[from] serde_json::Error),
}
