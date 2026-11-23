// SPDX-License-Identifier: AGPL-3.0-only
//! Typed transport errors.
//!
//! Every error is an [`ozma_protocol::ApiError`] so it can cross the API
//! boundary unchanged. The core code for "no usable path or connection" is
//! [`ozma_protocol::codes::TRANSPORT_UNAVAILABLE`]; malformed peer behaviour
//! uses the crate-private `transport.protocol_error` extension code.

use ozma_protocol::{codes, ApiError};

use crate::channel::ChannelId;
use crate::event::CloseReason;

/// Extension code for a peer that violated the transport framing/channel map.
pub const TRANSPORT_PROTOCOL_ERROR: &str = "transport.protocol_error";

/// No viable path, or the transport/connection is no longer usable.
pub fn transport_unavailable(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::TRANSPORT_UNAVAILABLE, message)
        .with_remediation("Retry, or fall back to another transport offered by the director.")
        .retryable()
}

/// A channel id that is not present in the negotiated [`crate::ChannelMap`].
pub fn channel_unknown(channel: ChannelId) -> ApiError {
    ApiError::new(
        codes::INVALID_REQUEST,
        format!("channel {} is not part of this session", channel.raw()),
    )
    .with_details(format!("unknown channel id {}", channel.raw()))
}

/// The transport or the peer has closed, so no further payloads can be sent.
pub fn channel_closed(channel: Option<ChannelId>, reason: CloseReason) -> ApiError {
    let subject = match channel {
        Some(channel) => format!("channel {} is closed", channel.raw()),
        None => "transport is closed".to_string(),
    };
    ApiError::new(
        codes::TRANSPORT_UNAVAILABLE,
        format!("{subject} ({})", reason.as_str()),
    )
    .with_details(reason.as_str())
    .retryable()
}

/// A payload exceeds the negotiated channel or datagram limit.
pub fn payload_too_large(channel: ChannelId, len: usize, max: usize) -> ApiError {
    ApiError::new(
        codes::INVALID_REQUEST,
        format!(
            "payload of {len} bytes exceeds the {max}-byte limit on channel {}",
            channel.raw()
        ),
    )
    .with_details(format!("channel={} len={} max={}", channel.raw(), len, max))
}

/// The peer sent data that does not fit the negotiation.
pub fn protocol_error(message: impl Into<String>) -> ApiError {
    ApiError::new(TRANSPORT_PROTOCOL_ERROR, message)
}
