// SPDX-License-Identifier: AGPL-3.0-only
//! Small counters shared by decoders and the decoded-frame buffer.
//!
//! The client-side decode path is headless and has no metrics endpoint of its
//! own, so the counters are deliberately plain values. A session layer can read
//! them with [`VideoDecoder::stats`](crate::VideoDecoder::stats) and publish
//! them alongside its other quality metrics.

use serde::{Deserialize, Serialize};

/// Counters for one decoder or one [`FrameBuffer`](crate::FrameBuffer).
///
/// `frames_decoded` counts frames produced by the backend (or accepted by the
/// buffer), `frames_dropped` counts output that was discarded because a bounded
/// queue was full, and `errors` counts access units the decoder rejected. None
/// of the counters wrap; they saturate at [`u64::MAX`].
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct DecodeStats {
    /// Frames successfully decoded (or pushed into a buffer).
    pub frames_decoded: u64,
    /// Frames discarded because a bounded queue was full.
    pub frames_dropped: u64,
    /// Access units or backend calls that failed.
    pub errors: u64,
}

impl DecodeStats {
    /// A zeroed counter set.
    pub const fn new() -> Self {
        Self {
            frames_decoded: 0,
            frames_dropped: 0,
            errors: 0,
        }
    }

    /// True when no frame has been decoded and no error has been recorded.
    pub const fn is_empty(&self) -> bool {
        self.frames_decoded == 0 && self.frames_dropped == 0 && self.errors == 0
    }

    /// Frames successfully decoded (or accepted by a buffer).
    ///
    /// Alias for the [`frames_decoded`](Self::frames_decoded) field, for callers
    /// that use the shorter `frames`/`drops` naming.
    pub const fn frames(&self) -> u64 {
        self.frames_decoded
    }

    /// Frames discarded because a bounded queue was full.
    pub const fn drops(&self) -> u64 {
        self.frames_dropped
    }

    pub(crate) fn record_frame(&mut self) {
        self.frames_decoded = self.frames_decoded.saturating_add(1);
    }

    pub(crate) fn record_drop(&mut self) {
        self.frames_dropped = self.frames_dropped.saturating_add(1);
    }

    pub(crate) fn record_error(&mut self) {
        self.errors = self.errors.saturating_add(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counters_expose_both_names() {
        let mut stats = DecodeStats::new();
        stats.record_frame();
        stats.record_drop();
        assert_eq!(stats.frames(), 1);
        assert_eq!(stats.drops(), 1);
    }

    #[test]
    fn counters_saturate_instead_of_wrapping() {
        let mut stats = DecodeStats {
            frames_decoded: u64::MAX,
            frames_dropped: u64::MAX,
            errors: u64::MAX,
        };
        stats.record_frame();
        stats.record_drop();
        stats.record_error();
        assert_eq!(
            stats,
            DecodeStats {
                frames_decoded: u64::MAX,
                frames_dropped: u64::MAX,
                errors: u64::MAX,
            }
        );
    }

    #[test]
    fn a_fresh_counter_set_is_empty() {
        assert!(DecodeStats::new().is_empty());
        assert!(DecodeStats::default().is_empty());
    }
}
