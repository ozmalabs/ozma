// SPDX-License-Identifier: AGPL-3.0-only
//! Reassembly for fragmented video access units.
//!
//! Video travels on an unreliable datagram channel. Each access unit is split
//! by the node into fragments with [`VideoFragmentHeader`]. This module is the
//! client-side counterpart; it tolerates reordering, duplicates, and missing
//! fragments without blocking the control channel.

use std::collections::BTreeMap;

use bytes::Bytes;
use ozma_protocol::media::VideoFragmentHeader;
use serde::{Deserialize, Serialize};

/// A complete encoded video access unit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct VideoFrame {
    pub track_id: u8,
    pub sequence: u64,
    pub keyframe: bool,
    pub data: Vec<u8>,
}

/// Why a datagram could not be assembled.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum VideoAssemblyError {
    /// The datagram was not a valid video fragment.
    InvalidFragment,
    /// A fragment index was duplicated with different bytes.
    ConflictingFragment,
    /// The frame exceeded the configured size limit.
    FrameTooLarge { bytes: usize },
    /// Too many incomplete frames are pending.
    TooManyPending { pending: usize },
}

impl std::fmt::Display for VideoAssemblyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidFragment => f.write_str("invalid video fragment"),
            Self::ConflictingFragment => f.write_str("conflicting video fragment"),
            Self::FrameTooLarge { bytes } => write!(f, "video frame is too large: {bytes} bytes"),
            Self::TooManyPending { pending } => {
                write!(f, "too many incomplete video frames: {pending}")
            }
        }
    }
}

impl std::error::Error for VideoAssemblyError {}

struct PendingFrame {
    keyframe: bool,
    count: u16,
    fragments: Vec<Option<Bytes>>,
    received: u16,
    bytes: usize,
}

/// Reassembles fragmented encoded video frames for one or more tracks.
pub struct VideoAssembler {
    pending: BTreeMap<(u8, u64), PendingFrame>,
    last_completed: BTreeMap<u8, u64>,
    max_frame_bytes: usize,
    max_pending: usize,
}

impl Default for VideoAssembler {
    fn default() -> Self {
        Self::new(8 * 1024 * 1024, 8)
    }
}

impl VideoAssembler {
    pub fn new(max_frame_bytes: usize, max_pending: usize) -> Self {
        Self {
            pending: BTreeMap::new(),
            last_completed: BTreeMap::new(),
            max_frame_bytes: max_frame_bytes.max(1),
            max_pending: max_pending.max(1),
        }
    }

    /// Feed one datagram. Returns a frame once all its fragments are present.
    pub fn push(
        &mut self,
        datagram: &[u8],
    ) -> Result<Option<VideoFrame>, VideoAssemblyError> {
        let (header, payload) =
            VideoFragmentHeader::parse(datagram).ok_or(VideoAssemblyError::InvalidFragment)?;
        let key = (header.track_id, header.frame_sequence);
        if let Some(completed) = self.last_completed.get(&header.track_id) {
            if header.frame_sequence <= *completed {
                return Ok(None); // stale or duplicate frame
            }
        }
        if header.count as usize > 4096 {
            return Err(VideoAssemblyError::InvalidFragment);
        }
        if !self.pending.contains_key(&key) && self.pending.len() >= self.max_pending {
            if let Some(oldest) = self.pending.keys().next().copied() {
                self.pending.remove(&oldest);
            }
        }
        let (complete, too_large, bytes) = {
            let entry = self.pending.entry(key).or_insert_with(|| PendingFrame {
                keyframe: header.keyframe,
                count: header.count,
                fragments: vec![None; header.count as usize],
                received: 0,
                bytes: 0,
            });
            if entry.count != header.count {
                return Err(VideoAssemblyError::InvalidFragment);
            }
            let slot = &mut entry.fragments[header.index as usize];
            if let Some(existing) = slot {
                if existing.as_ref() != payload {
                    return Err(VideoAssemblyError::ConflictingFragment);
                }
                return Ok(None);
            }
            entry.bytes = entry.bytes.saturating_add(payload.len());
            *slot = Some(Bytes::copy_from_slice(payload));
            entry.received += 1;
            (
                entry.received == entry.count,
                entry.bytes > self.max_frame_bytes,
                entry.bytes,
            )
        };
        if too_large {
            self.pending.remove(&key);
            return Err(VideoAssemblyError::FrameTooLarge { bytes });
        }
        if !complete {
            return Ok(None);
        }

        let pending = self.pending.remove(&key).expect("entry exists");
        let mut data = Vec::with_capacity(pending.bytes);
        for fragment in pending.fragments {
            data.extend_from_slice(&fragment.expect("all fragments present"));
        }
        self.last_completed.insert(header.track_id, header.frame_sequence);
        Ok(Some(VideoFrame {
            track_id: header.track_id,
            sequence: header.frame_sequence,
            keyframe: pending.keyframe,
            data,
        }))
    }

    /// Number of incomplete frames currently buffered.
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    /// Drop all incomplete frames, for reconnect or a keyframe request.
    pub fn reset(&mut self) {
        self.pending.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fragment(keyframe: bool, sequence: u64, index: u16, count: u16, payload: &[u8]) -> Vec<u8> {
        VideoFragmentHeader::new(keyframe, 0, sequence, index, count).encode_into(payload)
    }

    #[test]
    fn reassembles_out_of_order_fragments() {
        let mut assembler = VideoAssembler::default();
        assert!(assembler
            .push(&fragment(true, 7, 1, 3, b"world"))
            .unwrap()
            .is_none());
        assert!(assembler
            .push(&fragment(true, 7, 0, 3, b"hello "))
            .unwrap()
            .is_none());
        let frame = assembler
            .push(&fragment(true, 7, 2, 3, b"!"))
            .unwrap()
            .unwrap();
        assert!(frame.keyframe);
        assert_eq!(frame.sequence, 7);
        assert_eq!(frame.data.as_slice(), b"hello world!");
        assert_eq!(assembler.pending_len(), 0);
    }

    #[test]
    fn ignores_duplicates_and_stale_frames() {
        let mut assembler = VideoAssembler::default();
        assert!(assembler
            .push(&fragment(false, 1, 0, 2, b"a"))
            .unwrap()
            .is_none());
        assert!(assembler
            .push(&fragment(false, 1, 0, 2, b"a"))
            .unwrap()
            .is_none());
        assert!(assembler
            .push(&fragment(false, 1, 1, 2, b"b"))
            .unwrap()
            .is_some());
        assert!(assembler
            .push(&fragment(false, 1, 1, 2, b"b"))
            .unwrap()
            .is_none());
    }

    #[test]
    fn rejects_invalid_fragments() {
        let mut assembler = VideoAssembler::default();
        assert_eq!(
            assembler.push(b"not a fragment").unwrap_err(),
            VideoAssemblyError::InvalidFragment
        );
    }
}
