// SPDX-License-Identifier: AGPL-3.0-only
//! Sliding-window replay protection.
//!
//! Mirrors `ReplayWindow` in `controller/transport.py`.

/// Default replay-window size for HID (keyboard/mouse) streams.
pub const REPLAY_WINDOW_HID: usize = 64;

/// Default replay-window size for audio streams (larger due to jitter).
pub const REPLAY_WINDOW_AUDIO: usize = 512;

/// Sliding-window replay protection.
///
/// Tracks the highest seen counter and a multi-word bitmap of recent counters.
/// Rejects packets with `counter ≤ (highest − window_size)` or already seen.
///
/// Mirrors `ReplayWindow` in `controller/transport.py`.
pub struct ReplayWindow {
    window_size: usize,
    highest: u64,
    /// Bit N = 1 means `(highest − N)` has been seen.
    /// Word 0 holds bits 0..63 (most recent), word 1 holds bits 64..127, etc.
    bitmap: alloc::vec::Vec<u64>,
}

impl ReplayWindow {
    /// Create a new replay window with the given size (in packets).
    pub fn new(window_size: usize) -> Self {
        assert!(window_size > 0, "window_size must be > 0");
        let words = (window_size + 63) / 64;
        Self {
            window_size,
            highest: 0,
            bitmap: alloc::vec![0u64; words],
        }
    }

    /// Return `true` if the counter is valid (not replayed) and advance the
    /// window.  Returns `false` for replayed or too-old packets.
    pub fn check_and_advance(&mut self, counter: u64) -> bool {
        if counter > self.highest {
            let shift = (counter - self.highest) as usize;
            self.shift_bitmap(shift);
            self.set_bit(0); // mark the new highest
            self.highest = counter;
            return true;
        }

        let diff = (self.highest - counter) as usize;
        if diff >= self.window_size {
            return false; // too old
        }
        if self.get_bit(diff) {
            return false; // already seen (replay)
        }
        self.set_bit(diff);
        true
    }

    // ── Bitmap helpers ────────────────────────────────────────────────────────

    /// Shift the bitmap left by `shift` positions (older entries fall off).
    fn shift_bitmap(&mut self, shift: usize) {
        if shift >= self.window_size {
            for w in &mut self.bitmap {
                *w = 0;
            }
            return;
        }

        let word_shift = shift / 64;
        let bit_shift = shift % 64;
        let n = self.bitmap.len();

        for i in (0..n).rev() {
            let src = i.wrapping_sub(word_shift);
            self.bitmap[i] = if src >= n {
                0
            } else if bit_shift == 0 {
                self.bitmap[src]
            } else {
                let lo = self.bitmap[src] << bit_shift;
                let hi = if src > 0 {
                    self.bitmap[src - 1] >> (64 - bit_shift)
                } else {
                    0
                };
                lo | hi
            };
        }
    }

    fn set_bit(&mut self, pos: usize) {
        let word = pos / 64;
        let bit = pos % 64;
        if word < self.bitmap.len() {
            self.bitmap[word] |= 1u64 << bit;
        }
    }

    fn get_bit(&self, pos: usize) -> bool {
        let word = pos / 64;
        let bit = pos % 64;
        word < self.bitmap.len() && (self.bitmap[word] >> bit) & 1 == 1
    }
}
