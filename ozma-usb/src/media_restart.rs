// SPDX-License-Identifier: AGPL-3.0-only
//! Bounded restart policy for a userspace SCSI media export or FunctionFS
//! daemon.
//!
//! A failed bulk transport or an observed gadget unbind is not retried
//! forever. [`MediaRestartPolicy`] caps the number of restarts and computes a
//! capped exponential backoff; [`MediaRestartTracker`] counts attempts and
//! resets after a real successful transfer. The policy is data-only: it sleeps
//! nothing and touches no endpoint, so the default tests stay deterministic.

use serde::{Deserialize, Serialize};

use crate::error::UsbError;

/// Default restart attempts before the export stays failed.
pub const USB_MEDIA_RESTART_DEFAULT_MAX_ATTEMPTS: u32 = 3;
/// Default first retry delay.
pub const USB_MEDIA_RESTART_DEFAULT_BACKOFF_MS: u64 = 250;
/// Default backoff ceiling.
pub const USB_MEDIA_RESTART_DEFAULT_MAX_BACKOFF_MS: u64 = 4_000;
/// Largest accepted restart-attempt cap.
pub const USB_MEDIA_RESTART_MAX_ATTEMPTS_LIMIT: u32 = 16;
/// Largest accepted backoff delay or ceiling.
pub const USB_MEDIA_RESTART_MAX_BACKOFF_LIMIT_MS: u64 = 300_000;

/// Bounds for restarting a failed media export.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRestartPolicy {
    /// Maximum restart attempts. `0` disables restart and leaves the export
    /// failed on the first transport error/rebind.
    pub max_attempts: u32,
    /// First retry delay in milliseconds.
    pub backoff_ms: u64,
    /// Exponential backoff ceiling in milliseconds.
    pub max_backoff_ms: u64,
    /// Reload the persisted write overlay when the export is rebuilt.
    ///
    /// When true and the export has a persistence path, the overlay is
    /// committed before the rebuild so the restarted device serves the same
    /// bytes. When false, a writable export restarts from the base image.
    pub reload_overlay: bool,
}

impl Default for MediaRestartPolicy {
    fn default() -> Self {
        Self {
            max_attempts: USB_MEDIA_RESTART_DEFAULT_MAX_ATTEMPTS,
            backoff_ms: USB_MEDIA_RESTART_DEFAULT_BACKOFF_MS,
            max_backoff_ms: USB_MEDIA_RESTART_DEFAULT_MAX_BACKOFF_MS,
            reload_overlay: true,
        }
    }
}

impl MediaRestartPolicy {
    /// A policy with an attempt cap and first/ceiling backoff.
    pub fn new(max_attempts: u32, backoff_ms: u64, max_backoff_ms: u64) -> Self {
        Self {
            max_attempts,
            backoff_ms,
            max_backoff_ms,
            reload_overlay: true,
        }
    }

    /// Disable restart entirely.
    pub fn disabled() -> Self {
        Self {
            max_attempts: 0,
            ..Self::default()
        }
    }

    /// Set whether a rebuild reloads the persisted write overlay.
    pub fn with_overlay_reload(mut self, reload: bool) -> Self {
        self.reload_overlay = reload;
        self
    }

    /// Reject impossible bounds before any endpoint is touched.
    pub fn validate(&self) -> Result<(), UsbError> {
        if self.max_attempts > USB_MEDIA_RESTART_MAX_ATTEMPTS_LIMIT {
            return Err(UsbError::InvalidField {
                field: "usb-media.restart.max_attempts",
                reason: format!(
                    "attempt cap must be 0..={USB_MEDIA_RESTART_MAX_ATTEMPTS_LIMIT}, got {}",
                    self.max_attempts
                ),
            });
        }
        if self.backoff_ms > USB_MEDIA_RESTART_MAX_BACKOFF_LIMIT_MS
            || self.max_backoff_ms > USB_MEDIA_RESTART_MAX_BACKOFF_LIMIT_MS
        {
            return Err(UsbError::InvalidField {
                field: "usb-media.restart.backoff_ms",
                reason: format!(
                    "backoff values must be 0..={USB_MEDIA_RESTART_MAX_BACKOFF_LIMIT_MS} ms"
                ),
            });
        }
        if self.max_attempts > 0 && self.max_backoff_ms < self.backoff_ms {
            return Err(UsbError::InvalidField {
                field: "usb-media.restart.max_backoff_ms",
                reason: format!(
                    "backoff ceiling {} ms is below the first delay {} ms",
                    self.max_backoff_ms, self.backoff_ms
                ),
            });
        }
        Ok(())
    }

    /// Capped exponential delay for a 1-based attempt number.
    pub fn backoff_for(&self, attempt: u32) -> u64 {
        if attempt == 0 {
            return 0;
        }
        let shift = attempt.saturating_sub(1).min(63);
        let delay = self.backoff_ms.checked_shl(shift).unwrap_or(u64::MAX);
        delay.min(self.max_backoff_ms)
    }
}

/// One scheduled restart attempt.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRestartAttempt {
    /// 1-based attempt number.
    pub attempt: u32,
    /// Delay that should elapse before the rebuild.
    pub backoff_ms: u64,
}

/// Attempt counter for one export. A real successful transfer resets it.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRestartTracker {
    attempts: u32,
    exhausted: bool,
}

impl MediaRestartTracker {
    /// Fresh tracker with no recorded failures.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record one failure and return the next bounded attempt, or `None` when
    /// the cap has been reached (the export must stay failed).
    pub fn record_failure(&mut self, policy: &MediaRestartPolicy) -> Option<MediaRestartAttempt> {
        if self.attempts >= policy.max_attempts {
            self.exhausted = true;
            return None;
        }
        self.attempts = self.attempts.saturating_add(1);
        self.exhausted = false;
        Some(MediaRestartAttempt {
            attempt: self.attempts,
            backoff_ms: policy.backoff_for(self.attempts),
        })
    }

    /// A real command cycle succeeded; clear the failure streak.
    pub fn record_success(&mut self) {
        self.attempts = 0;
        self.exhausted = false;
    }

    /// Restart attempts recorded in the current failure streak.
    pub fn attempts(&self) -> u32 {
        self.attempts
    }

    /// Whether the configured cap was reached and no further restart is
    /// allowed.
    pub fn is_exhausted(&self) -> bool {
        self.exhausted
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_policy_is_bounded_and_validates() {
        let policy = MediaRestartPolicy::default();
        policy.validate().unwrap();
        assert_eq!(policy.max_attempts, USB_MEDIA_RESTART_DEFAULT_MAX_ATTEMPTS);
        assert!(policy.reload_overlay);
    }

    #[test]
    fn backoff_doubles_and_stays_below_the_ceiling() {
        let policy = MediaRestartPolicy::new(8, 100, 350);
        assert_eq!(policy.backoff_for(1), 100);
        assert_eq!(policy.backoff_for(2), 200);
        assert_eq!(policy.backoff_for(3), 350);
        assert_eq!(policy.backoff_for(9), 350);
        assert_eq!(policy.backoff_for(0), 0);
    }

    #[test]
    fn attempts_stop_at_the_cap_and_report_exhaustion() {
        let policy = MediaRestartPolicy::new(2, 10, 40);
        let mut tracker = MediaRestartTracker::new();
        assert_eq!(tracker.record_failure(&policy).unwrap().attempt, 1);
        assert_eq!(tracker.record_failure(&policy).unwrap().attempt, 2);
        assert!(tracker.record_failure(&policy).is_none());
        assert!(tracker.is_exhausted());
        assert_eq!(tracker.attempts(), 2);
    }

    #[test]
    fn zero_attempts_never_restart_and_success_resets() {
        let disabled = MediaRestartPolicy::disabled();
        let mut tracker = MediaRestartTracker::new();
        assert!(tracker.record_failure(&disabled).is_none());
        assert!(tracker.is_exhausted());

        let policy = MediaRestartPolicy::new(3, 10, 40);
        let mut tracker = MediaRestartTracker::new();
        tracker.record_failure(&policy).unwrap();
        tracker.record_success();
        assert_eq!(tracker.attempts(), 0);
        assert!(!tracker.is_exhausted());
    }

    #[test]
    fn invalid_bounds_are_typed() {
        let mut policy = MediaRestartPolicy::default();
        policy.max_attempts = USB_MEDIA_RESTART_MAX_ATTEMPTS_LIMIT + 1;
        assert_eq!(policy.validate().unwrap_err().kind(), "invalid-config");

        let policy = MediaRestartPolicy::new(1, 500, 100);
        assert_eq!(policy.validate().unwrap_err().kind(), "invalid-config");
    }
}
