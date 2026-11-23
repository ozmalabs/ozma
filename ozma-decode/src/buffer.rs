// SPDX-License-Identifier: AGPL-3.0-only
//! A small bounded queue of decoded frames.
//!
//! Transport reassembly and decode are independent of rendering. A client that
//! is briefly slow (a resized window, a paused surface) should not let decoded
//! frames grow without bound. [`FrameBuffer`] is the explicit, testable policy:
//! FIFO, fixed capacity, oldest frame dropped when full, and a drop counter.
//! It is not a scheduler or an A/V sync buffer.

use std::collections::VecDeque;

use crate::frame::DecodedVideoFrame;
use crate::stats::DecodeStats;

/// A bounded FIFO of decoded frames.
#[derive(Clone, Debug)]
pub struct FrameBuffer {
    capacity: usize,
    frames: VecDeque<DecodedVideoFrame>,
    stats: DecodeStats,
}

impl FrameBuffer {
    /// Create a buffer holding at most `capacity` frames.
    ///
    /// Zero is treated as one: a frame buffer with no capacity could not hold
    /// even a keyframe and would silently discard everything.
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            frames: VecDeque::new(),
            stats: DecodeStats::new(),
        }
    }

    /// Maximum number of frames retained.
    pub fn capacity(&self) -> usize {
        self.capacity
    }

    /// Number of frames currently retained.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// True when no frame is retained.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }

    /// True when the next push will evict the oldest frame.
    pub fn is_full(&self) -> bool {
        self.frames.len() >= self.capacity
    }

    /// Counters for accepted and evicted frames.
    ///
    /// `frames_decoded` here means "accepted by the buffer", and
    /// `frames_dropped` counts frames evicted to make room. `errors` stays zero;
    /// decode errors belong to the decoder.
    pub fn stats(&self) -> DecodeStats {
        self.stats
    }

    /// Push a frame, evicting and returning the oldest if the buffer is full.
    pub fn push(&mut self, frame: DecodedVideoFrame) -> Option<DecodedVideoFrame> {
        let evicted = if self.frames.len() >= self.capacity {
            self.stats.record_drop();
            self.frames.pop_front()
        } else {
            None
        };
        self.frames.push_back(frame);
        self.stats.record_frame();
        evicted
    }

    /// Remove and return the oldest frame.
    pub fn pop(&mut self) -> Option<DecodedVideoFrame> {
        self.frames.pop_front()
    }

    /// Borrow the oldest frame.
    pub fn peek(&self) -> Option<&DecodedVideoFrame> {
        self.frames.front()
    }

    /// Borrow the newest frame.
    pub fn peek_newest(&self) -> Option<&DecodedVideoFrame> {
        self.frames.back()
    }

    /// Iterate oldest to newest.
    pub fn iter(&self) -> impl DoubleEndedIterator<Item = &DecodedVideoFrame> {
        self.frames.iter()
    }

    /// Drop every retained frame without changing the counters.
    ///
    /// Use this on a stream restart: the next decodable picture is a keyframe,
    /// and frames from the old resolution or stream must not be presented.
    pub fn clear(&mut self) {
        self.frames.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::frame::{DecodeMeta, I420Planes};

    fn frame(sequence: u64) -> DecodedVideoFrame {
        DecodedVideoFrame::new(
            DecodeMeta::new(sequence * 1_000, sequence, sequence == 0),
            2,
            2,
            I420Planes::new(vec![16; 4], vec![128; 1], vec![128; 1]),
        )
        .unwrap()
    }

    #[test]
    fn zero_capacity_is_treated_as_one() {
        let mut buffer = FrameBuffer::new(0);
        assert_eq!(buffer.capacity(), 1);
        assert!(buffer.push(frame(0)).is_none());
        assert_eq!(buffer.len(), 1);
    }

    #[test]
    fn overflow_evicts_the_oldest_and_counts_the_drop() {
        let mut buffer = FrameBuffer::new(2);
        assert!(buffer.push(frame(0)).is_none());
        assert!(buffer.push(frame(1)).is_none());
        let evicted = buffer.push(frame(2)).expect("oldest frame evicted");
        assert_eq!(evicted.sequence, 0);
        assert_eq!(buffer.len(), 2);
        assert_eq!(buffer.stats().frames_decoded, 3);
        assert_eq!(buffer.stats().frames_dropped, 1);
        assert_eq!(buffer.peek().unwrap().sequence, 1);
        assert_eq!(buffer.peek_newest().unwrap().sequence, 2);
    }

    #[test]
    fn pop_is_fifo_and_clear_keeps_counters() {
        let mut buffer = FrameBuffer::new(4);
        buffer.push(frame(5));
        buffer.push(frame(6));
        assert_eq!(buffer.pop().unwrap().sequence, 5);
        buffer.clear();
        assert!(buffer.is_empty());
        assert_eq!(buffer.stats().frames_decoded, 2);
        assert_eq!(buffer.stats().frames_dropped, 0);
    }
}
