// SPDX-License-Identifier: AGPL-3.0-only
//! Channel framing for reliable streams and unreliable datagrams.
//!
//! Every reliable channel shares one ordered byte stream. A frame is a
//! one-byte channel id, a big-endian `u32` payload length, and the payload.
//! Unreliable channels (video, audio) travel as datagrams with a one-byte
//! channel id and no length prefix, because the datagram boundary is the
//! length.
//!
//! Both the native QUIC adapter and the browser WebTransport adapter use this
//! module so a session's bytes are identical on every transport.

use bytes::Bytes;
use ozma_protocol::{ApiError, Result};
use ozma_transport::ChannelId;

/// The fixed header size of a reliable stream frame.
pub const FRAME_HEADER_LEN: usize = 5;

/// Encode one reliable-channel frame.
pub fn encode_reliable(channel: ChannelId, payload: &[u8]) -> Result<Vec<u8>> {
    if payload.len() > u32::MAX as usize {
        return Err(ozma_transport::payload_too_large(
            channel,
            payload.len(),
            u32::MAX as usize,
        ));
    }
    let mut frame = Vec::with_capacity(FRAME_HEADER_LEN + payload.len());
    frame.push(channel.raw());
    frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
    frame.extend_from_slice(payload);
    Ok(frame)
}

/// Encode one unreliable-channel datagram.
///
/// `capacity` is the transport's maximum datagram size including the channel
/// byte. Passing `None` skips the check.
pub fn encode_datagram(
    channel: ChannelId,
    payload: &[u8],
    capacity: Option<usize>,
) -> Result<Vec<u8>> {
    if let Some(capacity) = capacity {
        if payload.len() + 1 > capacity {
            return Err(ozma_transport::payload_too_large(
                channel,
                payload.len(),
                capacity.saturating_sub(1),
            ));
        }
    }
    let mut datagram = Vec::with_capacity(1 + payload.len());
    datagram.push(channel.raw());
    datagram.extend_from_slice(payload);
    Ok(datagram)
}

/// Incremental decoder for a reliable channel stream.
///
/// Feed arbitrary chunks with [`FrameDecoder::push`]; it returns every complete
/// frame. Partial bytes stay buffered. The decoder enforces a maximum payload
/// so a corrupt length prefix cannot force an unbounded allocation.
#[derive(Debug)]
pub struct FrameDecoder {
    buffer: Vec<u8>,
    max_payload: usize,
}

impl FrameDecoder {
    /// Create a decoder that rejects payloads larger than `max_payload`.
    pub fn new(max_payload: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_payload,
        }
    }

    /// Append bytes and drain complete frames.
    pub fn push(&mut self, bytes: &[u8]) -> Result<Vec<(ChannelId, Bytes)>> {
        self.buffer.extend_from_slice(bytes);
        let mut frames = Vec::new();
        let mut offset = 0;
        while self.buffer.len() - offset >= FRAME_HEADER_LEN {
            let channel = ChannelId::new(self.buffer[offset]);
            let length = u32::from_be_bytes([
                self.buffer[offset + 1],
                self.buffer[offset + 2],
                self.buffer[offset + 3],
                self.buffer[offset + 4],
            ]) as usize;
            if length > self.max_payload {
                return Err(ozma_transport::payload_too_large(
                    channel,
                    length,
                    self.max_payload,
                ));
            }
            if self.buffer.len() - offset - FRAME_HEADER_LEN < length {
                break;
            }
            let start = offset + FRAME_HEADER_LEN;
            let end = start + length;
            frames.push((channel, Bytes::copy_from_slice(&self.buffer[start..end])));
            offset = end;
        }
        if offset > 0 {
            self.buffer.drain(..offset);
        }
        Ok(frames)
    }

    /// Decode one unreliable-channel datagram.
    pub fn decode_datagram(datagram: &[u8]) -> Result<(ChannelId, Bytes)> {
        let (&channel, payload) = datagram.split_first().ok_or_else(|| {
            ApiError::new(
                ozma_protocol::error::codes::INVALID_REQUEST,
                "empty datagram has no channel id",
            )
        })?;
        Ok((ChannelId::new(channel), Bytes::copy_from_slice(payload)))
    }

    /// Bytes buffered for a frame that has not fully arrived.
    pub fn buffered(&self) -> usize {
        self.buffer.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reliable_frames_roundtrip_across_chunk_boundaries() {
        let first = encode_reliable(ChannelId::CONTROL, b"hello").unwrap();
        let second = encode_reliable(ChannelId::AUDIO, b"world!").unwrap();
        let mut stream = first.clone();
        stream.extend_from_slice(&second);

        let mut decoder = FrameDecoder::new(u16::MAX as usize);
        assert!(decoder.push(&stream[..3]).unwrap().is_empty());
        assert!(decoder.push(&stream[3..7]).unwrap().is_empty());
        let frames = decoder.push(&stream[7..]).unwrap();
        assert_eq!(frames.len(), 2);
        assert_eq!(frames[0].0, ChannelId::CONTROL);
        assert_eq!(frames[0].1.as_ref(), b"hello");
        assert_eq!(frames[1].0, ChannelId::AUDIO);
        assert_eq!(frames[1].1.as_ref(), b"world!");
        assert_eq!(decoder.buffered(), 0);
    }

    #[test]
    fn oversized_length_is_rejected_without_allocating() {
        let mut decoder = FrameDecoder::new(16);
        let mut bytes = vec![ChannelId::CONTROL.raw()];
        bytes.extend_from_slice(&(1024u32).to_be_bytes());
        assert!(decoder.push(&bytes).is_err());
    }

    #[test]
    fn datagrams_carry_the_channel_byte() {
        let encoded = encode_datagram(ChannelId::VIDEO, b"fragment", Some(1200)).unwrap();
        assert_eq!(encoded[0], ChannelId::VIDEO.raw());
        let (channel, payload) = FrameDecoder::decode_datagram(&encoded).unwrap();
        assert_eq!(channel, ChannelId::VIDEO);
        assert_eq!(payload.as_ref(), b"fragment");
        assert!(encode_datagram(ChannelId::VIDEO, &[0; 1200], Some(1200)).is_err());
    }
}
