//! WireGuard key rotation state shared by the userspace and kernel backends.
//!
//! A rotation installs a freshly generated X25519 keypair on the local node,
//! updates every peer tunnel/link, and advertises the new public key.  The
//! previous key stays available for a bounded grace window so handshakes that
//! started before the switch can still complete:
//!
//! * the userspace manager keeps the old boringtun tunnels alongside the new
//!   ones and accepts either key while the window is open;
//! * the kernel backend has a single interface key, so it retains the previous
//!   private key for rollback only (documented in
//!   `docs/phase4-mesh-rotation-report.md`).
//!
//! No type in this module serialises private key material.  [`RotationStatus`]
//! contains a public-key fingerprint and generation counter only; the private
//! keys live in [`KeyRotation`] and are never `Debug`-printed.

use std::fmt;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

use crate::node::{WgPrivateKey, WgPublicKey};

/// Default grace window after a rotation.
pub const DEFAULT_GRACE: Duration = Duration::from_secs(90);
/// Smallest accepted grace window.
pub const MIN_GRACE: Duration = Duration::from_secs(5);
/// Largest accepted grace window (must stay well below a day).
pub const MAX_GRACE: Duration = Duration::from_secs(600);

/// Smallest scheduled rotation interval (one hour).
pub const MIN_ROTATE_HOURS: u64 = 1;
/// Largest scheduled rotation interval (30 days).
pub const MAX_ROTATE_HOURS: u64 = 24 * 30;
/// Default jitter added to a scheduled rotation (five minutes).
pub const DEFAULT_ROTATE_JITTER_SECS: u64 = 300;
/// Largest accepted schedule jitter (one hour).
pub const MAX_ROTATE_JITTER_SECS: u64 = 3_600;

/// Validate a requested grace window.
///
/// Rotation is refused rather than silently clamped: an operator asking for a
/// zero or day-long grace window gets a typed error.
pub fn validate_grace(grace: Duration) -> Result<Duration, String> {
    if grace < MIN_GRACE || grace > MAX_GRACE {
        return Err(format!(
            "key rotation grace must be between {} and {} seconds",
            MIN_GRACE.as_secs(),
            MAX_GRACE.as_secs()
        ));
    }
    Ok(grace)
}

/// Optional scheduled rotation policy (`--mesh-rotate-hours`).
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationSchedule {
    /// Rotation interval in hours (`1..=720`).
    pub interval_hours: u64,
    /// Random jitter added to each interval in seconds.
    #[serde(default)]
    pub jitter_secs: u64,
    /// Grace window applied to every scheduled rotation.
    #[serde(default = "default_grace_secs")]
    pub grace_secs: u64,
}

fn default_grace_secs() -> u64 {
    DEFAULT_GRACE.as_secs()
}

impl RotationSchedule {
    /// Build a schedule with default jitter and grace.
    pub fn new(interval_hours: u64) -> Self {
        Self {
            interval_hours,
            jitter_secs: DEFAULT_ROTATE_JITTER_SECS,
            grace_secs: DEFAULT_GRACE.as_secs(),
        }
    }

    pub fn with_jitter_secs(mut self, jitter_secs: u64) -> Self {
        self.jitter_secs = jitter_secs;
        self
    }

    pub fn with_grace_secs(mut self, grace_secs: u64) -> Self {
        self.grace_secs = grace_secs;
        self
    }

    /// Validate CLI/config bounds.
    pub fn validate(&self) -> Result<(), String> {
        if !(MIN_ROTATE_HOURS..=MAX_ROTATE_HOURS).contains(&self.interval_hours) {
            return Err(format!(
                "mesh rotation interval must be between {MIN_ROTATE_HOURS} and \
                 {MAX_ROTATE_HOURS} hours (0 disables scheduled rotation)"
            ));
        }
        if self.jitter_secs > MAX_ROTATE_JITTER_SECS {
            return Err(format!(
                "mesh rotation jitter must be at most {MAX_ROTATE_JITTER_SECS} seconds"
            ));
        }
        validate_grace(Duration::from_secs(self.grace_secs))?;
        Ok(())
    }

    /// Nominal interval between rotations.
    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval_hours.saturating_mul(3_600))
    }

    /// Grace window applied by this schedule.
    pub fn grace(&self) -> Duration {
        Duration::from_secs(self.grace_secs)
    }

    /// Interval plus a random jitter in `0..=jitter_secs`.
    pub fn next_delay(&self) -> Duration {
        let jitter = if self.jitter_secs == 0 {
            0
        } else {
            rand::random::<u64>() % (self.jitter_secs.saturating_add(1))
        };
        self.interval() + Duration::from_secs(jitter)
    }
}

/// Schedule as reported in status.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationScheduleStatus {
    pub interval_hours: u64,
    pub jitter_secs: u64,
    pub grace_secs: u64,
    /// Unix milliseconds at which the scheduler expects the next rotation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_rotation_unix_ms: Option<u64>,
}

impl RotationScheduleStatus {
    pub fn from_schedule(schedule: &RotationSchedule, next: Option<SystemTime>) -> Self {
        Self {
            interval_hours: schedule.interval_hours,
            jitter_secs: schedule.jitter_secs,
            grace_secs: schedule.grace_secs,
            next_rotation_unix_ms: next.map(system_time_millis),
        }
    }
}

/// Public, secret-free view of the local key lifecycle.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct RotationStatus {
    /// Monotonic generation counter; starts at `1` for the configured key.
    pub key_id: u64,
    /// Current public key.  Public keys are safe to publish.
    pub public_key: String,
    /// Short, stable fingerprint of the current public key.
    pub fingerprint: String,
    /// Unix milliseconds of the most recent completed rotation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_rotation_unix_ms: Option<u64>,
    /// Seconds left in the grace window, when one is open.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grace_remaining_secs: Option<u64>,
    /// Fingerprint of the previous key while it is still retained.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub previous_fingerprint: Option<String>,
    /// Grace window applied by the most recent rotation.
    pub grace_secs: u64,
    /// Scheduled rotation policy, when enabled.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scheduled: Option<RotationScheduleStatus>,
}

fn system_time_millis(time: SystemTime) -> u64 {
    time.duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0)
}

/// Stable fingerprint of a WireGuard public key.
///
/// FNV-1a over the decoded 32 bytes, formatted as `wg:<16 hex chars>`.  This is
/// only a human/operator identifier; it is derived from the public key and
/// contains no secret material.
pub fn fingerprint(public_key: &WgPublicKey) -> String {
    let bytes = public_key.to_bytes().unwrap_or_else(|_| {
        // An un-decodable key can only come from a broken caller; hash the
        // text so status still produces a stable identifier.
        let mut padded = [0u8; 32];
        for (slot, byte) in padded.iter_mut().zip(public_key.0.as_bytes()) {
            *slot = *byte;
        }
        padded
    });
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    format!("wg:{hash:016x}")
}

struct PreviousKey {
    private: WgPrivateKey,
    public: WgPublicKey,
    /// Monotonic deadline used by the backend to stop accepting the old key.
    expires_at: Instant,
    /// Wall-clock deadline for status reporting.
    started_at: SystemTime,
}

/// Local key lineage: current key, optional grace key, generation, schedule.
pub struct KeyRotation {
    current: WgPrivateKey,
    public: WgPublicKey,
    previous: Option<PreviousKey>,
    generation: u64,
    last_rotation: Option<SystemTime>,
    grace: Duration,
    schedule: Option<RotationSchedule>,
}

impl fmt::Debug for KeyRotation {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("KeyRotation")
            .field("public", &self.public)
            .field("generation", &self.generation)
            .field("grace", &self.grace)
            .field("previous", &self.previous.as_ref().map(|key| &key.public))
            .finish_non_exhaustive()
    }
}

impl KeyRotation {
    /// Start a lineage at the configured private key.
    pub fn new(private_key: WgPrivateKey) -> Self {
        let public = private_key.public_key();
        Self {
            current: private_key,
            public,
            previous: None,
            generation: 1,
            last_rotation: None,
            grace: DEFAULT_GRACE,
            schedule: None,
        }
    }

    pub fn current_private(&self) -> &WgPrivateKey {
        &self.current
    }

    pub fn current_public(&self) -> &WgPublicKey {
        &self.public
    }

    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Previous public keys that are still inside the grace window.
    pub fn previous_public_keys(&self, now: Instant) -> Vec<WgPublicKey> {
        match &self.previous {
            Some(previous) if now < previous.expires_at => vec![previous.public.clone()],
            Some(_) | None => Vec::new(),
        }
    }

    pub fn set_schedule(&mut self, schedule: Option<RotationSchedule>) {
        self.schedule = schedule;
    }

    pub fn schedule(&self) -> Option<&RotationSchedule> {
        self.schedule.as_ref()
    }

    /// Install `new_private` as the current key, retaining the old one for
    /// `grace`.  Returns the new public status snapshot.
    pub fn begin(
        &mut self,
        new_private: WgPrivateKey,
        grace: Duration,
        now: Instant,
        wall: SystemTime,
    ) -> RotationStatus {
        let new_public = new_private.public_key();
        let previous = std::mem::replace(&mut self.current, new_private);
        let previous_public = std::mem::replace(&mut self.public, new_public);
        self.previous = Some(PreviousKey {
            private: previous,
            public: previous_public,
            expires_at: now + grace,
            started_at: wall,
        });
        self.generation = self.generation.wrapping_add(1);
        self.last_rotation = Some(wall);
        self.grace = grace;
        self.status(now)
    }

    /// Drop the previous key when its grace window has elapsed.
    ///
    /// Returns `true` when a retained key was removed, so the caller can tear
    /// down any old tunnels or re-advertise without the old public key.
    pub fn expire(&mut self, now: Instant) -> bool {
        if self
            .previous
            .as_ref()
            .is_some_and(|previous| now >= previous.expires_at)
        {
            self.previous = None;
            return true;
        }
        false
    }

    /// Previous key still retained (for rollback).
    pub fn previous_private(&self) -> Option<&WgPrivateKey> {
        self.previous.as_ref().map(|previous| &previous.private)
    }

    pub fn previous_started_at(&self) -> Option<SystemTime> {
        self.previous.as_ref().map(|previous| previous.started_at)
    }

    pub fn grace(&self) -> Duration {
        self.grace
    }

    /// Secret-free status snapshot.
    pub fn status(&self, now: Instant) -> RotationStatus {
        let (grace_remaining_secs, previous_fingerprint) = match &self.previous {
            Some(previous) if now < previous.expires_at => {
                let remaining = previous.expires_at.saturating_duration_since(now);
                (
                    Some(remaining.as_secs().max(1)),
                    Some(fingerprint(&previous.public)),
                )
            }
            _ => (None, None),
        };
        RotationStatus {
            key_id: self.generation,
            public_key: self.public.0.clone(),
            fingerprint: fingerprint(&self.public),
            last_rotation_unix_ms: self.last_rotation.map(system_time_millis),
            grace_remaining_secs,
            previous_fingerprint,
            grace_secs: self.grace.as_secs(),
            scheduled: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fingerprint_is_stable_and_public_only() {
        let key = WgPrivateKey::generate();
        let public = key.public_key();
        let first = fingerprint(&public);
        let second = fingerprint(&public);
        assert_eq!(first, second);
        assert!(first.starts_with("wg:"));
        assert!(!first.contains(&key.to_base64()));
    }

    #[test]
    fn rotation_keeps_previous_public_key_during_grace() {
        let manager_wall = SystemTime::now();
        let now = Instant::now();
        let mut rotation = KeyRotation::new(WgPrivateKey::generate());
        let old_public = rotation.current_public().clone();
        let new_private = WgPrivateKey::generate();
        let new_public = new_private.public_key();
        let status = rotation.begin(new_private, Duration::from_secs(30), now, manager_wall);
        assert_eq!(status.key_id, 2);
        assert_eq!(status.public_key, new_public.0);
        assert_eq!(status.previous_fingerprint, Some(fingerprint(&old_public)));
        assert_eq!(rotation.previous_public_keys(now), vec![old_public]);
        assert!(!rotation.expire(now));
        assert!(rotation.expire(now + Duration::from_secs(31)));
        assert!(rotation
            .previous_public_keys(now + Duration::from_secs(31))
            .is_empty());
    }

    #[test]
    fn grace_bounds_are_enforced() {
        assert!(validate_grace(Duration::from_secs(4)).is_err());
        assert!(validate_grace(Duration::from_secs(601)).is_err());
        assert!(validate_grace(DEFAULT_GRACE).is_ok());
    }

    #[test]
    fn schedule_bounds_and_jitter() {
        assert!(RotationSchedule::new(0).validate().is_err());
        assert!(RotationSchedule::new(MIN_ROTATE_HOURS).validate().is_ok());
        assert!(RotationSchedule::new(MAX_ROTATE_HOURS + 1)
            .validate()
            .is_err());
        let schedule = RotationSchedule::new(2)
            .with_jitter_secs(10)
            .with_grace_secs(30);
        schedule.validate().unwrap();
        for _ in 0..20 {
            let delay = schedule.next_delay();
            assert!(delay >= Duration::from_secs(7_200));
            assert!(delay <= Duration::from_secs(7_210));
        }
    }
}
