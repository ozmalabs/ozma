// SPDX-License-Identifier: AGPL-3.0-only
//! H.264 RTP packetizer (RFC 6184).
//!
//! One [`H264Packetizer`] is stateful: it owns the RTP sequence number and the
//! cached SPS/PPS pair that is repeated before IDR access units. Feed it
//! Annex-B access units and it returns ready-to-send RTP packets.

use bytes::Bytes;

use crate::error::RtpError;
use crate::nal::{
    is_idr, nal_type, push_annex_b, split_annex_b, NAL_TYPE_PPS, NAL_TYPE_SPS, RTP_FU_A, RTP_STAP_A,
};

/// Fixed size of an RTP header without CSRC entries or extensions.
pub const RTP_HEADER_LEN: usize = 12;

/// Default MTU for RTP over LAN or TCP interleaving.
///
/// 1200 bytes leaves room for IP/UDP headers inside a 1500-byte Ethernet MTU
/// and is also a safe size for WebRTC-style transports.
pub const DEFAULT_MTU: usize = 1200;

/// Default RTP payload type for H.264 (dynamic range).
pub const DEFAULT_PAYLOAD_TYPE: u8 = 96;

/// Smallest MTU that can carry an RTP header plus a non-empty FU-A fragment.
pub const MIN_MTU: usize = RTP_HEADER_LEN + 2 + 1;

/// Default upper bound on one access unit handed to the packetizer.
pub const DEFAULT_MAX_ACCESS_UNIT_BYTES: usize = 8 * 1024 * 1024;

/// Packetizer tuning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PacketizerConfig {
    /// Maximum RTP packet size in bytes, including the 12-byte RTP header.
    pub mtu: usize,
    /// RTP payload type used for H.264.
    pub payload_type: u8,
    /// Repeat cached SPS/PPS before every IDR access unit.
    ///
    /// When the access unit already carries a parameter set, only the missing
    /// cached set is added. Set to `false` for strict bitstream round-trips.
    pub repeat_parameter_sets: bool,
    /// Reject access units larger than this many bytes.
    pub max_access_unit_bytes: usize,
}

impl Default for PacketizerConfig {
    fn default() -> Self {
        Self {
            mtu: DEFAULT_MTU,
            payload_type: DEFAULT_PAYLOAD_TYPE,
            repeat_parameter_sets: true,
            max_access_unit_bytes: DEFAULT_MAX_ACCESS_UNIT_BYTES,
        }
    }
}

impl PacketizerConfig {
    /// Validate MTU and size limits.
    pub fn validate(&self) -> Result<(), RtpError> {
        if self.mtu < MIN_MTU {
            return Err(RtpError::MtuTooSmall {
                mtu: self.mtu,
                min: MIN_MTU,
            });
        }
        if self.max_access_unit_bytes == 0 {
            return Err(RtpError::AccessUnitTooLarge { size: 0, limit: 0 });
        }
        Ok(())
    }

    /// Maximum NAL unit bytes in a single-NAL packet.
    pub fn max_single_payload(&self) -> usize {
        self.mtu.saturating_sub(RTP_HEADER_LEN)
    }

    /// Maximum fragment bytes in one FU-A packet.
    pub fn max_fragment_payload(&self) -> usize {
        self.mtu.saturating_sub(RTP_HEADER_LEN + 2)
    }
}

/// Counters for one packetizer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PacketizerStats {
    /// Access units accepted.
    pub access_units: u64,
    /// RTP packets produced.
    pub packets: u64,
    /// RTP bytes produced, including headers.
    pub bytes: u64,
    /// Access units that contained an IDR slice (or were flagged keyframe).
    pub keyframes: u64,
    /// Single-NAL packets.
    pub single_nal_packets: u64,
    /// STAP-A aggregate packets.
    pub stap_a_packets: u64,
    /// FU-A fragment packets.
    pub fu_a_packets: u64,
}

#[derive(Default, Debug)]
struct CachedParameterSets {
    sps: Option<Bytes>,
    pps: Option<Bytes>,
}

/// Stateful H.264 RTP packetizer.
#[derive(Debug)]
pub struct H264Packetizer {
    config: PacketizerConfig,
    ssrc: u32,
    sequence: u16,
    stats: PacketizerStats,
    cache: CachedParameterSets,
}

impl H264Packetizer {
    /// Create a packetizer for `ssrc` and `mtu`.
    pub fn new(ssrc: u32, mtu: usize) -> Result<Self, RtpError> {
        Self::with_config(
            ssrc,
            PacketizerConfig {
                mtu,
                ..PacketizerConfig::default()
            },
        )
    }

    /// Create a packetizer with explicit configuration.
    pub fn with_config(ssrc: u32, config: PacketizerConfig) -> Result<Self, RtpError> {
        config.validate()?;
        Ok(Self {
            config,
            ssrc,
            sequence: 0,
            stats: PacketizerStats::default(),
            cache: CachedParameterSets::default(),
        })
    }

    /// Current configuration.
    pub fn config(&self) -> &PacketizerConfig {
        &self.config
    }

    /// SSRC carried by produced packets.
    pub fn ssrc(&self) -> u32 {
        self.ssrc
    }

    /// Set the SSRC used by subsequent packets.
    pub fn set_ssrc(&mut self, ssrc: u32) {
        self.ssrc = ssrc;
    }

    /// Sequence number that the next packet will carry.
    pub fn next_sequence(&self) -> u16 {
        self.sequence
    }

    /// Override the next sequence number.
    pub fn set_sequence(&mut self, sequence: u16) {
        self.sequence = sequence;
    }

    /// Snapshot of counters.
    pub fn stats(&self) -> PacketizerStats {
        self.stats
    }

    /// Drop cached SPS/PPS and counters, as for a new stream.
    pub fn reset(&mut self) {
        self.cache = CachedParameterSets::default();
        self.sequence = 0;
        self.stats = PacketizerStats::default();
    }

    /// Packetize one Annex-B access unit.
    ///
    /// `timestamp` is the RTP timestamp in 90 kHz units. The marker bit is set
    /// on the final packet of the access unit. SPS/PPS NAL units found in the
    /// access unit are sent together as a STAP-A packet at the front; if
    /// [`PacketizerConfig::repeat_parameter_sets`] is set, a cached SPS/PPS
    /// pair is also inserted before every IDR that does not carry one.
    pub fn packetize(
        &mut self,
        annex_b: &[u8],
        timestamp: u32,
        keyframe: bool,
    ) -> Result<Vec<Bytes>, RtpError> {
        if annex_b.len() > self.config.max_access_unit_bytes {
            return Err(RtpError::AccessUnitTooLarge {
                size: annex_b.len(),
                limit: self.config.max_access_unit_bytes,
            });
        }
        let nals = split_annex_b(annex_b)?;
        let idr = keyframe || nals.iter().any(|nal| is_idr(nal));

        let mut parameter_sets: Vec<Bytes> = Vec::new();
        let mut media_nals: Vec<&[u8]> = Vec::new();
        let mut has_sps = false;
        let mut has_pps = false;

        for nal in nals {
            match nal_type(nal) {
                Some(NAL_TYPE_SPS) => {
                    has_sps = true;
                    let bytes = Bytes::copy_from_slice(nal);
                    self.cache.sps = Some(bytes.clone());
                    push_unique(&mut parameter_sets, bytes);
                }
                Some(NAL_TYPE_PPS) => {
                    has_pps = true;
                    let bytes = Bytes::copy_from_slice(nal);
                    self.cache.pps = Some(bytes.clone());
                    push_unique(&mut parameter_sets, bytes);
                }
                _ => media_nals.push(nal),
            }
        }

        if idr && self.config.repeat_parameter_sets {
            if !has_sps {
                if let Some(sps) = &self.cache.sps {
                    push_unique(&mut parameter_sets, sps.clone());
                }
            }
            if !has_pps {
                if let Some(pps) = &self.cache.pps {
                    push_unique(&mut parameter_sets, pps.clone());
                }
            }
        }

        if parameter_sets.is_empty() && media_nals.is_empty() {
            return Err(RtpError::EmptyAccessUnit);
        }

        let max_single = self.config.max_single_payload();
        let max_fragment = self.config.max_fragment_payload();
        let mut bodies: Vec<Bytes> = Vec::new();
        let mut stap_a_used = false;

        if !parameter_sets.is_empty() {
            let stap_size = 1
                + parameter_sets.len() * 2
                + parameter_sets.iter().map(|nal| nal.len()).sum::<usize>();
            if parameter_sets.len() >= 2 && stap_size <= max_single {
                let mut body = Vec::with_capacity(stap_size);
                body.push(RTP_STAP_A);
                for nal in &parameter_sets {
                    let len = nal.len();
                    body.push((len >> 8) as u8);
                    body.push(len as u8);
                    body.extend_from_slice(nal);
                }
                bodies.push(Bytes::from(body));
                stap_a_used = true;
            } else {
                for nal in &parameter_sets {
                    self.push_nal_body(&mut bodies, nal, max_single, max_fragment)?;
                }
            }
        }

        for nal in media_nals {
            self.push_nal_body(&mut bodies, nal, max_single, max_fragment)?;
        }

        if bodies.is_empty() {
            return Err(RtpError::EmptyAccessUnit);
        }

        let packet_count = bodies.len();
        let mut packets = Vec::with_capacity(packet_count);
        for (index, body) in bodies.into_iter().enumerate() {
            let marker = index + 1 == packet_count;
            let mut packet = Vec::with_capacity(RTP_HEADER_LEN + body.len());
            write_header(
                &mut packet,
                marker,
                self.config.payload_type,
                self.sequence,
                timestamp,
                self.ssrc,
            );
            packet.extend_from_slice(&body);
            self.sequence = self.sequence.wrapping_add(1);
            self.stats.packets += 1;
            self.stats.bytes += packet.len() as u64;
            packets.push(Bytes::from(packet));
        }

        self.stats.access_units += 1;
        if idr {
            self.stats.keyframes += 1;
        }
        if stap_a_used {
            self.stats.stap_a_packets += 1;
        }
        Ok(packets)
    }

    fn push_nal_body(
        &mut self,
        bodies: &mut Vec<Bytes>,
        nal: &[u8],
        max_single: usize,
        max_fragment: usize,
    ) -> Result<(), RtpError> {
        if nal.is_empty() {
            return Err(RtpError::InvalidPayload("empty NAL unit"));
        }
        if nal.len() <= max_single {
            bodies.push(Bytes::copy_from_slice(nal));
            self.stats.single_nal_packets += 1;
            return Ok(());
        }
        if max_fragment == 0 {
            return Err(RtpError::MtuTooSmall {
                mtu: self.config.mtu,
                min: MIN_MTU,
            });
        }

        let header = nal[0];
        let indicator = (header & 0xe0) | RTP_FU_A;
        let nal_type = header & 0x1f;
        let mut offset = 1usize;
        let mut first = true;
        while offset < nal.len() {
            let end = (offset + max_fragment).min(nal.len());
            let last = end == nal.len();
            let mut fu_header = nal_type;
            if first {
                fu_header |= 0x80;
            }
            if last {
                fu_header |= 0x40;
            }
            let mut body = Vec::with_capacity(2 + end - offset);
            body.push(indicator);
            body.push(fu_header);
            body.extend_from_slice(&nal[offset..end]);
            bodies.push(Bytes::from(body));
            self.stats.fu_a_packets += 1;
            first = false;
            offset = end;
        }
        Ok(())
    }
}

/// Write an RTP version-2 header with no padding, extension, or CSRC list.
pub fn write_header(
    out: &mut Vec<u8>,
    marker: bool,
    payload_type: u8,
    sequence: u16,
    timestamp: u32,
    ssrc: u32,
) {
    out.push(0x80); // version 2, padding/extension/CSRC absent
    out.push(((marker as u8) << 7) | (payload_type & 0x7f));
    out.extend_from_slice(&sequence.to_be_bytes());
    out.extend_from_slice(&timestamp.to_be_bytes());
    out.extend_from_slice(&ssrc.to_be_bytes());
}

fn push_unique(list: &mut Vec<Bytes>, value: Bytes) {
    if !list.iter().any(|existing| existing == &value) {
        list.push(value);
    }
}

/// Convert microseconds to a 90 kHz RTP timestamp.
pub fn micros_to_rtp_timestamp(micros: u64) -> u32 {
    ((micros as u128 * 90_000u128) / 1_000_000u128) as u32
}

/// Small helper for monotonically increasing RTP timestamps.
///
/// Encoders may restart their PTS at zero, and capture timestamps may be
/// slightly out of order. This clock maps input microseconds to 90 kHz output
/// and never moves backwards.
#[derive(Clone, Debug, Default)]
pub struct RtpTimestampClock {
    base_micros: Option<u64>,
    last_timestamp: Option<u32>,
}

impl RtpTimestampClock {
    /// Reset the clock, for a new encoder or a resumed stream.
    pub fn reset(&mut self) {
        self.base_micros = None;
        self.last_timestamp = None;
    }

    /// Convert a presentation timestamp, keeping output monotonic.
    pub fn timestamp(&mut self, micros: u64) -> u32 {
        let base = *self.base_micros.get_or_insert(micros);
        let relative = micros.saturating_sub(base);
        let mut timestamp = micros_to_rtp_timestamp(relative);
        if let Some(last) = self.last_timestamp {
            if timestamp <= last {
                timestamp = last.wrapping_add(1);
            }
        }
        self.last_timestamp = Some(timestamp);
        timestamp
    }
}

/// Re-encode NAL units into an Annex-B access unit.
pub fn nals_to_annex_b(nals: &[Vec<u8>]) -> Vec<u8> {
    let mut out = Vec::new();
    for nal in nals {
        push_annex_b(&mut out, nal);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn timestamp_clock_is_monotonic() {
        let mut clock = RtpTimestampClock::default();
        assert_eq!(clock.timestamp(1_000_000), 0);
        assert_eq!(clock.timestamp(1_033_333), 2_999);
        assert_eq!(clock.timestamp(1_033_333), 3_000);
        clock.reset();
        assert_eq!(clock.timestamp(5_000_000), 0);
    }
}
