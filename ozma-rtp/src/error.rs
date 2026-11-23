// SPDX-License-Identifier: AGPL-3.0-only
//! Errors produced by the RTP packetizer and depacketizer.

/// A recoverable or fatal RTP/H.264 framing error.
///
/// The depacketizer reports malformed input instead of panicking. Callers may
/// log an error and keep reading: a single bad datagram must not tear down a
/// session.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum RtpError {
    /// Packet is shorter than the fixed RTP header.
    #[error("RTP packet is too short: {len} bytes (need at least {min})")]
    PacketTooShort {
        /// Length of the packet that was supplied.
        len: usize,
        /// Minimum acceptable length.
        min: usize,
    },

    /// RTP version field is not 2.
    #[error("unsupported RTP version {0}")]
    UnsupportedVersion(u8),

    /// A header field claims more bytes than the packet contains.
    #[error("truncated RTP header: {0}")]
    TruncatedHeader(&'static str),

    /// The packet carries no payload.
    #[error("RTP packet has an empty payload")]
    EmptyPayload,

    /// The packet's payload type does not match the configured value.
    #[error("unexpected RTP payload type {found} (expected {expected})")]
    UnexpectedPayloadType {
        /// Payload type found in the packet.
        found: u8,
        /// Payload type configured on the depacketizer.
        expected: u8,
    },

    /// An RTP payload type that is not H.264.
    #[error("unsupported RTP payload type {0}")]
    UnsupportedPayloadType(u8),

    /// A legal H.264 RTP payload type that this depacketizer does not support.
    #[error("unsupported H.264 RTP payload type {0} (only single NAL, STAP-A, FU-A)")]
    UnsupportedH264PayloadType(u8),

    /// The H.264 payload was structurally invalid.
    #[error("invalid H.264 payload: {0}")]
    InvalidPayload(&'static str),

    /// Input is not an Annex-B byte stream.
    #[error("input is not Annex-B H.264 (no start code found)")]
    NotAnnexB,

    /// The MTU is too small to carry an RTP header and one payload byte.
    #[error("MTU {mtu} is too small (minimum {min})")]
    MtuTooSmall {
        /// Requested MTU.
        mtu: usize,
        /// Smallest supported MTU.
        min: usize,
    },

    /// A single access unit exceeded the configured packetizer limit.
    #[error("access unit is too large: {size} bytes (limit {limit})")]
    AccessUnitTooLarge {
        /// Size of the access unit.
        size: usize,
        /// Configured limit.
        limit: usize,
    },

    /// Input to the packetizer contained no NAL units.
    #[error("access unit contains no NAL units")]
    EmptyAccessUnit,

    /// A duplicate sequence number carried different bytes.
    #[error("conflicting duplicate RTP sequence {0}")]
    ConflictingDuplicate(u16),
}
