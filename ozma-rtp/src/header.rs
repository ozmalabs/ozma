// SPDX-License-Identifier: AGPL-3.0-only
//! RTP header parsing.
//!
//! The packetizer always writes the fixed 12-byte header. The parser is more
//! liberal because clients and Moonlight-style peers may send CSRC lists,
//! extensions, or padding in RTCP-adjacent traffic.

use crate::error::RtpError;

/// Length of the fixed RTP header.
pub const RTP_HEADER_LEN: usize = 12;

/// Parsed RTP header with the payload range resolved.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RtpHeader {
    /// Marker bit.
    pub marker: bool,
    /// RTP payload type.
    pub payload_type: u8,
    /// Sequence number.
    pub sequence: u16,
    /// RTP timestamp (90 kHz for H.264).
    pub timestamp: u32,
    /// Synchronisation source.
    pub ssrc: u32,
    /// Offset where the payload starts.
    pub payload_offset: usize,
    /// Offset where the payload ends (padding removed).
    pub payload_end: usize,
    /// Header length including CSRCs and extension.
    pub header_len: usize,
}

impl RtpHeader {
    /// Parse and validate a complete RTP packet.
    pub fn parse(packet: &[u8]) -> Result<Self, RtpError> {
        if packet.len() < RTP_HEADER_LEN {
            return Err(RtpError::PacketTooShort {
                len: packet.len(),
                min: RTP_HEADER_LEN,
            });
        }
        let version = packet[0] >> 6;
        if version != 2 {
            return Err(RtpError::UnsupportedVersion(version));
        }
        let padding = packet[0] & 0x20 != 0;
        let extension = packet[0] & 0x10 != 0;
        let csrc_count = (packet[0] & 0x0f) as usize;

        let mut offset = RTP_HEADER_LEN;
        let csrc_bytes = csrc_count
            .checked_mul(4)
            .ok_or(RtpError::TruncatedHeader("CSRC count overflows"))?;
        offset = offset
            .checked_add(csrc_bytes)
            .ok_or(RtpError::TruncatedHeader("CSRC list overflows"))?;
        if offset > packet.len() {
            return Err(RtpError::TruncatedHeader("CSRC list exceeds packet"));
        }

        if extension {
            if offset + 4 > packet.len() {
                return Err(RtpError::TruncatedHeader("extension header exceeds packet"));
            }
            let words = u16::from_be_bytes([packet[offset + 2], packet[offset + 3]]) as usize;
            let extension_bytes = words
                .checked_mul(4)
                .ok_or(RtpError::TruncatedHeader("extension length overflows"))?;
            offset = offset
                .checked_add(4 + extension_bytes)
                .ok_or(RtpError::TruncatedHeader("extension length overflows"))?;
            if offset > packet.len() {
                return Err(RtpError::TruncatedHeader("extension exceeds packet"));
            }
        }

        let mut end = packet.len();
        if padding {
            let padding_len = *packet
                .last()
                .ok_or(RtpError::TruncatedHeader("missing padding byte"))?
                as usize;
            if padding_len == 0 || padding_len > end.saturating_sub(offset) {
                return Err(RtpError::InvalidPayload("invalid RTP padding length"));
            }
            end -= padding_len;
        }

        let marker = packet[1] & 0x80 != 0;
        let payload_type = packet[1] & 0x7f;
        let sequence = u16::from_be_bytes([packet[2], packet[3]]);
        let timestamp = u32::from_be_bytes([packet[4], packet[5], packet[6], packet[7]]);
        let ssrc = u32::from_be_bytes([packet[8], packet[9], packet[10], packet[11]]);

        Ok(Self {
            marker,
            payload_type,
            sequence,
            timestamp,
            ssrc,
            payload_offset: offset,
            payload_end: end,
            header_len: offset,
        })
    }

    /// Payload slice for this header.
    pub fn payload<'a>(&self, packet: &'a [u8]) -> Option<&'a [u8]> {
        packet.get(self.payload_offset..self.payload_end)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal() -> Vec<u8> {
        vec![0x80, 96, 0x00, 0x2a, 0, 0, 0x23, 0x28, 1, 2, 3, 4, 9, 9]
    }

    #[test]
    fn parses_minimal_header() {
        let packet = minimal();
        let header = RtpHeader::parse(&packet).unwrap();
        assert_eq!(header.sequence, 42);
        assert_eq!(header.timestamp, 9000);
        assert_eq!(header.ssrc, 0x0102_0304);
        assert_eq!(header.payload(&packet), Some(&[9u8, 9][..]));
    }

    #[test]
    fn skips_csrc_and_extension() {
        let mut packet = vec![0x80 | 0x10 | 1, 96, 0, 1, 0, 0, 0, 0, 0, 0, 0, 0];
        packet.extend_from_slice(&[0, 0, 0, 0]); // one CSRC
        packet.extend_from_slice(&[0xab, 0xcd, 0, 1]); // extension, one word
        packet.extend_from_slice(&[0, 0, 0, 0]);
        packet.extend_from_slice(&[7, 7]);
        let header = RtpHeader::parse(&packet).unwrap();
        assert_eq!(header.payload(&packet), Some(&[7u8, 7][..]));
    }

    #[test]
    fn strips_padding() {
        let mut packet = minimal();
        packet[0] |= 0x20;
        packet.extend_from_slice(&[0, 0]);
        packet[2] = 0x00;
        packet[3] = 0x2b;
        let last = packet.len() - 1;
        packet[last] = 2;
        let header = RtpHeader::parse(&packet).unwrap();
        assert_eq!(header.payload(&packet), Some(&[9u8, 9][..]));
    }

    #[test]
    fn rejects_bad_version_and_truncation() {
        let mut packet = minimal();
        packet[0] = 0x40;
        assert!(matches!(
            RtpHeader::parse(&packet),
            Err(RtpError::UnsupportedVersion(1))
        ));
        assert!(matches!(
            RtpHeader::parse(&packet[..4]),
            Err(RtpError::PacketTooShort { .. })
        ));
    }
}
