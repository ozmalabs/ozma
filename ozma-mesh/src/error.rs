use thiserror::Error;

/// Errors produced by the ozma-mesh crate.
#[derive(Debug, Error)]
pub enum MeshError {
    #[error("WireGuard key error: {0}")]
    KeyError(String),

    #[error("Tunnel error: {0}")]
    TunnelError(String),

    #[error("Peer not found: {0}")]
    PeerNotFound(String),

    #[error("Duplicate peer: {0}")]
    DuplicatePeer(String),

    #[error("Invalid mesh IP: {0}")]
    InvalidIp(String),

    #[error("mDNS error: {0}")]
    MdnsError(String),

    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Serialisation error: {0}")]
    Serde(#[from] serde_json::Error),
}
