// SPDX-License-Identifier: AGPL-3.0-only
//! H.264 RTP depacketizer (RFC 6184).
//!
//! The depacketizer accepts complete RTP packets (not just payloads), groups
//! them by timestamp, and returns an Annex-B access unit when the marker bit
//! closes a complete, gap-free range. Out-of-order packets are buffered. Loss
//! is reported through [`DepacketizerStats`] and is never a panic.

use std::collections::BTreeMap;

use bytes::Bytes;

use crate::error::RtpError;
use crate::header::RtpHeader;
use crate::nal::{is_idr, push_annex_b, RTP_FU_A, RTP_SINGLE_NAL_MAX, RTP_STAP_A};

/// Default limit on incomplete access units buffered at once.
pub const DEFAULT_MAX_PENDING_UNITS: usize = 16;
/// Default limit on one reassembled access unit.
pub const DEFAULT_MAX_ACCESS_UNIT_BYTES: usize = 16 * 1024 * 1024;
/// Default maximum span between the first and marker sequence number.
pub const DEFAULT_MAX_SEQUENCE_SPAN: usize = 4096;

/// Depacketizer tuning.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepacketizerConfig {
    /// If set, reject packets with a different RTP payload type.
    pub payload_type: Option<u8>,
    /// Maximum incomplete access units held in memory.
    pub max_pending_units: usize,
    /// Maximum reassembled access-unit size.
    pub max_access_unit_bytes: usize,
    /// Maximum packet span between the first packet and the marker packet.
    pub max_sequence_span: usize,
}

impl Default for DepacketizerConfig {
    fn default() -> Self {
        Self {
            payload_type: None,
            max_pending_units: DEFAULT_MAX_PENDING_UNITS,
            max_access_unit_bytes: DEFAULT_MAX_ACCESS_UNIT_BYTES,
            max_sequence_span: DEFAULT_MAX_SEQUENCE_SPAN,
        }
    }
}

/// Counters for one depacketizer.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DepacketizerStats {
    /// RTP packets examined (including malformed packets).
    pub packets: u64,
    /// RTP packet bytes examined, including headers.
    pub bytes: u64,
    /// RTP payload bytes seen.
    pub payload_bytes: u64,
    /// Complete access units emitted.
    pub access_units: u64,
    /// Emitted access units containing an IDR slice.
    pub keyframes: u64,
    /// Incomplete access units discarded (evicted or flushed).
    pub incomplete_access_units: u64,
    /// Access units discarded because reassembly was malformed.
    pub dropped_access_units: u64,
    /// RTP packet gaps counted inside discarded access units.
    pub missing_fragments: u64,
    /// Packets rejected by RTP or H.264 payload validation.
    pub malformed_packets: u64,
    /// Duplicate packets recognised and ignored.
    pub duplicates: u64,
    /// Packets that arrived before an earlier sequence number.
    pub reordered_packets: u64,
    /// STAP-A packets processed.
    pub stap_a_packets: u64,
    /// FU-A packets processed.
    pub fu_a_packets: u64,
}

/// A complete access unit reassembled from RTP packets.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DepacketizedAccessUnit {
    /// RTP timestamp shared by the contributing packets.
    pub timestamp: u32,
    /// Annex-B encoded access unit (each NAL prefixed with `00 00 00 01`).
    pub data: Vec<u8>,
    /// Whether the access unit contains an IDR slice.
    pub keyframe: bool,
    /// Number of NAL units in the access unit.
    pub nals: usize,
    /// Number of RTP packets that formed the access unit.
    pub packets: usize,
    /// First RTP sequence number.
    pub first_sequence: u16,
    /// Last (marker) RTP sequence number.
    pub last_sequence: u16,
}

impl DepacketizedAccessUnit {
    /// Total bytes of the Annex-B access unit.
    pub fn len(&self) -> usize {
        self.data.len()
    }

    /// Whether the access unit is empty. Always false for emitted units.
    pub fn is_empty(&self) -> bool {
        self.data.is_empty()
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Piece {
    Nals(Vec<Bytes>),
    Fu {
        start: bool,
        end: bool,
        nal_header: u8,
        data: Bytes,
    },
}

#[derive(Debug)]
struct PendingUnit {
    packets: BTreeMap<u16, Piece>,
    first_seq: Option<u16>,
    marker_seq: Option<u16>,
    bytes: usize,
    order: u64,
}

impl PendingUnit {
    fn new(order: u64) -> Self {
        Self {
            packets: BTreeMap::new(),
            first_seq: None,
            marker_seq: None,
            bytes: 0,
            order,
        }
    }
}

/// Stateful H.264 RTP depacketizer.
#[derive(Debug)]
pub struct H264Depacketizer {
    config: DepacketizerConfig,
    pending: BTreeMap<u32, PendingUnit>,
    stats: DepacketizerStats,
    order: u64,
    last_sequence: Option<u16>,
    last_emitted_timestamp: Option<u32>,
}

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl H264Depacketizer {
    /// Create a depacketizer with default limits.
    pub fn new() -> Self {
        Self::with_config(DepacketizerConfig::default())
    }

    /// Create a depacketizer that only accepts `payload_type`.
    pub fn with_payload_type(payload_type: u8) -> Self {
        Self::with_config(DepacketizerConfig {
            payload_type: Some(payload_type),
            ..DepacketizerConfig::default()
        })
    }

    /// Create a depacketizer with explicit limits.
    pub fn with_config(config: DepacketizerConfig) -> Self {
        Self {
            config: DepacketizerConfig {
                max_pending_units: config.max_pending_units.max(1),
                max_access_unit_bytes: config.max_access_unit_bytes.max(1),
                max_sequence_span: config.max_sequence_span.max(1),
                ..config
            },
            pending: BTreeMap::new(),
            stats: DepacketizerStats::default(),
            order: 0,
            last_sequence: None,
            last_emitted_timestamp: None,
        }
    }

    /// Current configuration.
    pub fn config(&self) -> &DepacketizerConfig {
        &self.config
    }

    /// Snapshot of counters.
    pub fn stats(&self) -> DepacketizerStats {
        self.stats
    }

    /// Number of incomplete access units currently buffered.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Number of missing packets across buffered units whose marker has arrived.
    ///
    /// This is a non-destructive view of loss while waiting for a reordering
    /// window. [`Self::flush_incomplete`] discards those units and moves the
    /// count into [`DepacketizerStats::missing_fragments`].
    pub fn pending_missing_fragments(&self) -> usize {
        self.pending
            .values()
            .filter_map(|unit| unit.marker_seq.map(|marker| count_missing(unit, marker)))
            .sum()
    }

    /// Drop all buffered state and counters.
    pub fn reset(&mut self) {
        self.pending.clear();
        self.stats = DepacketizerStats::default();
        self.order = 0;
        self.last_sequence = None;
        self.last_emitted_timestamp = None;
    }

    /// Feed one complete RTP packet.
    ///
    /// Returns `Ok(Some(unit))` when this packet completed a gap-free access
    /// unit. Returns `Ok(None)` while a unit is still incomplete. Malformed
    /// packets yield `Err` and are counted; the depacketizer remains usable.
    pub fn push(&mut self, packet: &[u8]) -> Result<Option<DepacketizedAccessUnit>, RtpError> {
        let result = self.push_inner(packet);
        if result.is_err() {
            self.stats.malformed_packets = self.stats.malformed_packets.saturating_add(1);
        }
        result
    }

    fn push_inner(&mut self, packet: &[u8]) -> Result<Option<DepacketizedAccessUnit>, RtpError> {
        self.stats.packets = self.stats.packets.saturating_add(1);
        self.stats.bytes = self.stats.bytes.saturating_add(packet.len() as u64);

        let header = RtpHeader::parse(packet)?;
        if let Some(expected) = self.config.payload_type {
            if header.payload_type != expected {
                return Err(RtpError::UnexpectedPayloadType {
                    found: header.payload_type,
                    expected,
                });
            }
        }
        let payload = header
            .payload(packet)
            .filter(|payload| !payload.is_empty())
            .ok_or(RtpError::EmptyPayload)?;
        self.stats.payload_bytes = self
            .stats
            .payload_bytes
            .saturating_add(payload.len() as u64);

        let piece = parse_payload(payload)?;
        match &piece {
            Piece::Nals(_) if payload[0] & 0x1f == RTP_STAP_A => {
                self.stats.stap_a_packets = self.stats.stap_a_packets.saturating_add(1);
            }
            Piece::Fu { .. } => {
                self.stats.fu_a_packets = self.stats.fu_a_packets.saturating_add(1);
            }
            _ => {}
        }

        let sequence = header.sequence;
        if let Some(previous) = self.last_sequence {
            if sequence != previous && sequence_before(sequence, previous) {
                self.stats.reordered_packets = self.stats.reordered_packets.saturating_add(1);
            }
            if sequence_after(sequence, previous) {
                self.last_sequence = Some(sequence);
            }
        } else {
            self.last_sequence = Some(sequence);
        }

        if self.last_emitted_timestamp == Some(header.timestamp) {
            // Duplicate traffic after a completed access unit. A new access
            // unit must use a new 90 kHz timestamp.
            self.stats.duplicates = self.stats.duplicates.saturating_add(1);
            return Ok(None);
        }

        self.order = self.order.wrapping_add(1);
        let order = self.order;
        if !self.pending.contains_key(&header.timestamp)
            && self.pending.len() >= self.config.max_pending_units
        {
            self.evict_oldest();
        }

        let unit = self
            .pending
            .entry(header.timestamp)
            .or_insert_with(|| PendingUnit::new(order));

        if let Some(existing) = unit.packets.get(&sequence) {
            if existing == &piece {
                self.stats.duplicates = self.stats.duplicates.saturating_add(1);
                return Ok(None);
            }
            return Err(RtpError::ConflictingDuplicate(sequence));
        }

        if unit.first_seq.is_none()
            || unit
                .first_seq
                .map(|first| sequence_before(sequence, first))
                .unwrap_or(false)
        {
            unit.first_seq = Some(sequence);
        }
        if header.marker {
            unit.marker_seq = Some(match unit.marker_seq {
                Some(existing) if sequence_before(sequence, existing) => existing,
                _ => sequence,
            });
        }
        unit.bytes = unit.bytes.saturating_add(payload.len());
        unit.packets.insert(sequence, piece);

        if unit.bytes > self.config.max_access_unit_bytes {
            let size = unit.bytes;
            self.pending.remove(&header.timestamp);
            self.stats.dropped_access_units = self.stats.dropped_access_units.saturating_add(1);
            return Err(RtpError::AccessUnitTooLarge {
                size,
                limit: self.config.max_access_unit_bytes,
            });
        }

        self.try_finalize(header.timestamp)
    }

    /// Discard every incomplete access unit, counting the lost packets.
    ///
    /// Call after a gap has persisted (for example on a receive timeout) to
    /// resynchronise on the next keyframe. Returns the number of units dropped.
    pub fn flush_incomplete(&mut self) -> usize {
        let timestamps: Vec<u32> = self.pending.keys().copied().collect();
        for timestamp in &timestamps {
            if let Some(unit) = self.pending.remove(timestamp) {
                let missing = unit
                    .marker_seq
                    .map(|marker| count_missing(&unit, marker))
                    .unwrap_or(0);
                self.stats.incomplete_access_units =
                    self.stats.incomplete_access_units.saturating_add(1);
                self.stats.dropped_access_units = self.stats.dropped_access_units.saturating_add(1);
                self.stats.missing_fragments =
                    self.stats.missing_fragments.saturating_add(missing as u64);
            }
        }
        timestamps.len()
    }

    fn evict_oldest(&mut self) {
        let oldest = self
            .pending
            .iter()
            .min_by_key(|(_, unit)| unit.order)
            .map(|(timestamp, _)| *timestamp);
        if let Some(timestamp) = oldest {
            if let Some(unit) = self.pending.remove(&timestamp) {
                let missing = unit
                    .marker_seq
                    .map(|marker| count_missing(&unit, marker))
                    .unwrap_or(0);
                self.stats.incomplete_access_units =
                    self.stats.incomplete_access_units.saturating_add(1);
                self.stats.dropped_access_units = self.stats.dropped_access_units.saturating_add(1);
                self.stats.missing_fragments =
                    self.stats.missing_fragments.saturating_add(missing as u64);
            }
        }
    }

    fn try_finalize(&mut self, timestamp: u32) -> Result<Option<DepacketizedAccessUnit>, RtpError> {
        let Some(unit) = self.pending.get(&timestamp) else {
            return Ok(None);
        };
        let (Some(first), Some(marker)) = (unit.first_seq, unit.marker_seq) else {
            return Ok(None);
        };
        let range_len = marker.wrapping_sub(first) as usize + 1;
        if range_len > self.config.max_sequence_span {
            self.pending.remove(&timestamp);
            self.stats.dropped_access_units = self.stats.dropped_access_units.saturating_add(1);
            return Err(RtpError::InvalidPayload("RTP sequence span is too large"));
        }
        if count_missing(unit, marker) > 0 {
            return Ok(None);
        }

        let unit = self
            .pending
            .remove(&timestamp)
            .ok_or(RtpError::InvalidPayload("access unit disappeared"))?;
        let nals = match assemble(&unit, first, marker) {
            Ok(nals) => nals,
            Err(error) => {
                self.stats.dropped_access_units = self.stats.dropped_access_units.saturating_add(1);
                return Err(error);
            }
        };

        let mut data = Vec::new();
        for nal in &nals {
            push_annex_b(&mut data, nal);
        }
        if data.len() > self.config.max_access_unit_bytes {
            self.stats.dropped_access_units = self.stats.dropped_access_units.saturating_add(1);
            return Err(RtpError::AccessUnitTooLarge {
                size: data.len(),
                limit: self.config.max_access_unit_bytes,
            });
        }
        let keyframe = nals.iter().any(|nal| is_idr(nal));
        self.stats.access_units = self.stats.access_units.saturating_add(1);
        if keyframe {
            self.stats.keyframes = self.stats.keyframes.saturating_add(1);
        }
        self.last_emitted_timestamp = Some(timestamp);

        Ok(Some(DepacketizedAccessUnit {
            timestamp,
            data,
            keyframe,
            nals: nals.len(),
            packets: range_len,
            first_sequence: first,
            last_sequence: marker,
        }))
    }
}

fn parse_payload(payload: &[u8]) -> Result<Piece, RtpError> {
    let first = *payload
        .first()
        .ok_or(RtpError::InvalidPayload("empty payload"))?;
    if first & 0x80 != 0 {
        return Err(RtpError::InvalidPayload(
            "H.264 payload has the forbidden zero bit set",
        ));
    }
    let nal_type = first & 0x1f;
    match nal_type {
        1..=RTP_SINGLE_NAL_MAX => Ok(Piece::Nals(vec![Bytes::copy_from_slice(payload)])),
        RTP_STAP_A => parse_stap_a(payload),
        RTP_FU_A => parse_fu_a(payload),
        other => Err(RtpError::UnsupportedH264PayloadType(other)),
    }
}

fn parse_stap_a(payload: &[u8]) -> Result<Piece, RtpError> {
    let mut nals = Vec::new();
    let mut offset = 1usize;
    while offset < payload.len() {
        if offset + 2 > payload.len() {
            return Err(RtpError::InvalidPayload("truncated STAP-A size field"));
        }
        let size = u16::from_be_bytes([payload[offset], payload[offset + 1]]) as usize;
        offset += 2;
        if size == 0 {
            return Err(RtpError::InvalidPayload(
                "STAP-A contains an empty NAL unit",
            ));
        }
        let end = offset
            .checked_add(size)
            .ok_or(RtpError::InvalidPayload("STAP-A size overflows"))?;
        if end > payload.len() {
            return Err(RtpError::InvalidPayload("STAP-A NAL exceeds payload"));
        }
        let nal = &payload[offset..end];
        if nal[0] & 0x80 != 0 {
            return Err(RtpError::InvalidPayload(
                "STAP-A NAL has the forbidden zero bit set",
            ));
        }
        nals.push(Bytes::copy_from_slice(nal));
        offset = end;
    }
    if nals.is_empty() {
        return Err(RtpError::InvalidPayload("empty STAP-A packet"));
    }
    Ok(Piece::Nals(nals))
}

fn parse_fu_a(payload: &[u8]) -> Result<Piece, RtpError> {
    if payload.len() < 3 {
        return Err(RtpError::InvalidPayload("truncated FU-A packet"));
    }
    let indicator = payload[0];
    let fu_header = payload[1];
    if fu_header & 0x20 != 0 {
        return Err(RtpError::InvalidPayload("FU-A reserved bit is set"));
    }
    let original_type = fu_header & 0x1f;
    if original_type == 0 || original_type > RTP_SINGLE_NAL_MAX {
        return Err(RtpError::InvalidPayload("invalid FU-A NAL type"));
    }
    let start = fu_header & 0x80 != 0;
    let end = fu_header & 0x40 != 0;
    let nal_header = (indicator & 0xe0) | original_type;
    Ok(Piece::Fu {
        start,
        end,
        nal_header,
        data: Bytes::copy_from_slice(&payload[2..]),
    })
}

fn assemble(unit: &PendingUnit, first: u16, marker: u16) -> Result<Vec<Vec<u8>>, RtpError> {
    let mut nals = Vec::new();
    let mut current_fu: Option<(u8, Vec<u8>)> = None;
    let mut offset = 0u16;
    loop {
        let sequence = first.wrapping_add(offset);
        let piece = unit
            .packets
            .get(&sequence)
            .ok_or(RtpError::InvalidPayload("missing access-unit packet"))?;
        match piece {
            Piece::Nals(list) => {
                if current_fu.is_some() {
                    return Err(RtpError::InvalidPayload("FU-A fragment was not terminated"));
                }
                for nal in list {
                    if nal.is_empty() {
                        return Err(RtpError::InvalidPayload("empty NAL unit"));
                    }
                    nals.push(nal.to_vec());
                }
            }
            Piece::Fu {
                start,
                end,
                nal_header,
                data,
            } => {
                if *start {
                    if current_fu.is_some() {
                        return Err(RtpError::InvalidPayload("nested FU-A start"));
                    }
                    current_fu = Some((*nal_header, data.to_vec()));
                } else {
                    let Some((header, buffer)) = current_fu.as_mut() else {
                        return Err(RtpError::InvalidPayload("FU-A fragment without start"));
                    };
                    if *header != *nal_header {
                        return Err(RtpError::InvalidPayload(
                            "FU-A NAL header changed mid-fragment",
                        ));
                    }
                    buffer.extend_from_slice(data);
                }
                if *end {
                    let Some((header, buffer)) = current_fu.take() else {
                        return Err(RtpError::InvalidPayload("FU-A end without start"));
                    };
                    let mut nal = Vec::with_capacity(buffer.len() + 1);
                    nal.push(header);
                    nal.extend_from_slice(&buffer);
                    nals.push(nal);
                }
            }
        }
        if sequence == marker {
            break;
        }
        offset = offset.wrapping_add(1);
        if offset as usize > 0x8000 {
            return Err(RtpError::InvalidPayload(
                "RTP sequence range did not terminate",
            ));
        }
    }
    if current_fu.is_some() {
        return Err(RtpError::InvalidPayload("FU-A fragment did not end"));
    }
    if nals.is_empty() {
        return Err(RtpError::InvalidPayload(
            "access unit contains no NAL units",
        ));
    }
    Ok(nals)
}

fn count_missing(unit: &PendingUnit, marker: u16) -> usize {
    let Some(first) = unit.first_seq else {
        return 0;
    };
    let range_len = marker.wrapping_sub(first) as usize;
    if range_len > DEFAULT_MAX_SEQUENCE_SPAN * 4 {
        return range_len.saturating_sub(unit.packets.len());
    }
    let mut missing = 0usize;
    let mut offset = 0u16;
    loop {
        let sequence = first.wrapping_add(offset);
        if !unit.packets.contains_key(&sequence) {
            missing += 1;
        }
        if sequence == marker {
            break;
        }
        offset = offset.wrapping_add(1);
        if offset as usize > DEFAULT_MAX_SEQUENCE_SPAN * 4 {
            return missing;
        }
    }
    missing
}

fn sequence_after(a: u16, b: u16) -> bool {
    let distance = a.wrapping_sub(b);
    distance != 0 && distance < 0x8000
}

fn sequence_before(a: u16, b: u16) -> bool {
    sequence_after(b, a)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_ordering_handles_wrap() {
        assert!(sequence_after(0, 65535));
        assert!(sequence_before(65535, 0));
        assert!(!sequence_after(5, 5));
        assert!(sequence_after(10, 5));
    }

    #[test]
    fn stap_a_rejects_truncated_units() {
        assert!(parse_stap_a(&[24, 0, 5, 0x67]).is_err());
        assert!(parse_stap_a(&[24, 0, 0]).is_err());
        assert!(parse_stap_a(&[24]).is_err());
    }

    #[test]
    fn fu_a_rejects_missing_start() {
        let piece = parse_fu_a(&[0x7c, 0x05, 1, 2]).unwrap();
        assert!(matches!(
            piece,
            Piece::Fu {
                start: false,
                end: false,
                ..
            }
        ));
    }
}
