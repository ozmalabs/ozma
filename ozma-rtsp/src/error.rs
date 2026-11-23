// SPDX-License-Identifier: AGPL-3.0-only
//! Errors produced by RTSP parsing and session handling.

use std::io;

/// An RTSP protocol, transport, or I/O error.
#[derive(Debug, thiserror::Error)]
pub enum RtspError {
    /// A request line, header, or body was structurally invalid.
    #[error("malformed RTSP request: {0}")]
    MalformedRequest(String),

    /// `Transport:` named a mode this server does not implement.
    #[error("unsupported RTSP transport: {0}")]
    UnsupportedTransport(String),

    /// The `Session:` header named a session that does not exist.
    #[error("unknown RTSP session")]
    UnknownSession,

    /// The request is not valid in the current session state.
    #[error("request is not valid in this session state: {0}")]
    InvalidState(&'static str),

    /// No media source factory was configured.
    #[error("no video source is configured")]
    NoSource,

    /// An I/O failure on the socket or a media socket.
    #[error("I/O error: {0}")]
    Io(#[from] io::Error),

    /// The RTP packetizer rejected an access unit.
    #[error("RTP error: {0}")]
    Rtp(#[from] ozma_rtp::RtpError),
}
