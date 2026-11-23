// SPDX-License-Identifier: AGPL-3.0-only
//! Exponential reconnect backoff, matching the Python constants.

use std::time::Duration;

/// First reconnect delay (Python `_BACKOFF_INITIAL`).
pub const BACKOFF_INITIAL: Duration = Duration::from_millis(500);

/// Maximum reconnect delay (Python `_BACKOFF_MAX`).
pub const BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Multiplier applied to each delay (Python `_BACKOFF_FACTOR`).
pub const BACKOFF_FACTOR: f32 = 2.0;

/// Delay for a zero-based reconnect attempt, capped at `max`.
///
/// Exposed so tests and supervisors can assert the schedule without sleeping.
///
/// With the Python defaults the sequence is `0.5s, 1s, 2s, 4s, 5s, 5s, ...`.
pub fn backoff_delay(attempt: u32, initial: Duration, max: Duration, factor: f32) -> Duration {
    if initial >= max {
        return max;
    }
    let factor = if factor.is_finite() && factor > 0.0 {
        f64::from(factor)
    } else {
        1.0
    };
    // Clamp the exponent before `powi` so a huge attempt count cannot
    // overflow into a panic or a NaN duration.
    let exponent = attempt.min(1024) as i32;
    let seconds = initial.as_secs_f64() * factor.powi(exponent);
    if !seconds.is_finite() || seconds >= max.as_secs_f64() {
        max
    } else {
        Duration::from_secs_f64(seconds)
    }
}

/// A small stateful helper for supervisors that retry connections.
#[derive(Debug, Clone)]
pub struct Backoff {
    initial: Duration,
    max: Duration,
    factor: f32,
    attempt: u32,
}

impl Backoff {
    /// Create a backoff schedule.
    pub fn new(initial: Duration, max: Duration, factor: f32) -> Self {
        Self {
            initial,
            max,
            factor,
            attempt: 0,
        }
    }

    /// Build a backoff from a [`QmpConfig`](crate::QmpConfig).
    pub fn from_config(config: &crate::QmpConfig) -> Self {
        Self::new(
            config.backoff_initial,
            config.backoff_max,
            config.backoff_factor,
        )
    }

    /// Number of delays produced so far.
    pub fn attempt(&self) -> u32 {
        self.attempt
    }

    /// Return the next delay and advance the attempt counter.
    pub fn next_delay(&mut self) -> Duration {
        let delay = self.delay_for(self.attempt);
        self.attempt = self.attempt.saturating_add(1);
        delay
    }

    /// Delay for an explicit zero-based attempt.
    pub fn delay_for(&self, attempt: u32) -> Duration {
        backoff_delay(attempt, self.initial, self.max, self.factor)
    }

    /// Restart the schedule after a successful connection.
    pub fn reset(&mut self) {
        self.attempt = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_match_python() {
        assert_eq!(BACKOFF_INITIAL, Duration::from_millis(500));
        assert_eq!(BACKOFF_MAX, Duration::from_secs(5));
        assert_eq!(BACKOFF_FACTOR, 2.0);
    }

    #[test]
    fn schedule_caps_at_max() {
        let mut backoff = Backoff::new(BACKOFF_INITIAL, BACKOFF_MAX, BACKOFF_FACTOR);
        assert_eq!(backoff.next_delay(), Duration::from_millis(500));
        assert_eq!(backoff.next_delay(), Duration::from_secs(1));
        assert_eq!(backoff.next_delay(), Duration::from_secs(2));
        assert_eq!(backoff.next_delay(), Duration::from_secs(4));
        assert_eq!(backoff.next_delay(), Duration::from_secs(5));
        for _ in 0..10 {
            assert_eq!(backoff.next_delay(), BACKOFF_MAX);
        }
    }

    #[test]
    fn huge_attempt_is_capped_not_panicking() {
        assert_eq!(
            backoff_delay(u32::MAX, BACKOFF_INITIAL, BACKOFF_MAX, BACKOFF_FACTOR),
            BACKOFF_MAX
        );
    }
}
