// SPDX-License-Identifier: AGPL-3.0-only
//! Pairing offers for a client that cannot use a browser or platform
//! authenticator.
//!
//! A [`PairingCode`] combines a six-digit human code with a high-entropy nonce
//! and a short expiry. The node renders a QR payload
//! (`ozma://pair?node=<id>&nonce=<...>&fp=<...>`) and, for constrained
//! consoles, the grouped digits from [`PairingCode::human_readable`]. The
//! verifier consumes the offer on success and destroys it after a bounded
//! number of wrong attempts.
//!
//! Passkey/WebAuthn is the preferred flow where a platform authenticator
//! exists; this module is the short-code fallback and deliberately does not
//! implement WebAuthn. Pairing produces the identity/permissions that a caller
//! stores elsewhere; this crate only owns the challenge lifecycle.

use std::collections::HashMap;
use std::fmt;
use std::sync::{Mutex, MutexGuard};

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::Rng;
use rand::RngCore;

use ozma_protocol::{ApiError, Result};

use crate::error::codes;

/// Number of digits in the human-readable code.
pub const PAIRING_CODE_DIGITS: usize = 6;
/// Default lifetime of a pairing offer: 60 seconds.
pub const DEFAULT_PAIRING_TTL_SECONDS: u64 = 60;
/// Default number of wrong code guesses before an offer is destroyed.
pub const DEFAULT_PAIRING_MAX_ATTEMPTS: u32 = 5;
/// Default upper bound on pending offers.
pub const DEFAULT_MAX_PENDING_PAIRINGS: usize = 32;
/// URI scheme/host used by the QR payload.
pub const PAIRING_URI_PREFIX: &str = "ozma://pair?";

/// A generated pairing offer.
///
/// The code and nonce are redacted from `Debug`; use [`PairingCode::uri`] or
/// [`PairingCode::human_readable`] to display them deliberately.
#[derive(Clone)]
pub struct PairingCode {
    node_id: String,
    fingerprint: String,
    code: String,
    nonce: String,
    created_at: u64,
    expires_at: u64,
    max_attempts: u32,
}

impl fmt::Debug for PairingCode {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PairingCode")
            .field("node_id", &self.node_id)
            .field("fingerprint", &self.fingerprint)
            .field("code", &"[redacted]")
            .field("nonce", &"[redacted]")
            .field("created_at", &self.created_at)
            .field("expires_at", &self.expires_at)
            .field("max_attempts", &self.max_attempts)
            .finish()
    }
}

impl PairingCode {
    /// Generate an offer with the default 60 second lifetime and five attempts.
    pub fn generate(
        node_id: impl Into<String>,
        fingerprint: impl Into<String>,
        now: u64,
    ) -> Result<Self> {
        Self::generate_with(
            node_id,
            fingerprint,
            now,
            DEFAULT_PAIRING_TTL_SECONDS,
            DEFAULT_PAIRING_MAX_ATTEMPTS,
        )
    }

    /// Generate an offer with explicit limits.
    pub fn generate_with(
        node_id: impl Into<String>,
        fingerprint: impl Into<String>,
        now: u64,
        ttl_seconds: u64,
        max_attempts: u32,
    ) -> Result<Self> {
        let node_id = node_id.into();
        let fingerprint = fingerprint.into();

        validate_label("node id", &node_id, 256)?;
        validate_label("fingerprint", &fingerprint, 512)?;
        if ttl_seconds == 0 {
            return Err(pairing_unavailable(
                "a pairing code must not expire immediately",
            ));
        }
        if max_attempts == 0 {
            return Err(pairing_unavailable(
                "a pairing code must allow at least one attempt",
            ));
        }

        let code = format!(
            "{:0width$}",
            OsRng.gen_range(0..1_000_000u32),
            width = PAIRING_CODE_DIGITS
        );
        let nonce = random_nonce();

        Ok(Self {
            node_id,
            fingerprint,
            code,
            nonce,
            created_at: now,
            expires_at: now.saturating_add(ttl_seconds),
            max_attempts,
        })
    }

    /// The six digit code. Keep this out of logs.
    pub fn code(&self) -> &str {
        &self.code
    }

    /// The high-entropy nonce that addresses this offer.
    pub fn nonce(&self) -> &str {
        &self.nonce
    }

    pub fn node_id(&self) -> &str {
        &self.node_id
    }

    pub fn fingerprint(&self) -> &str {
        &self.fingerprint
    }

    pub fn created_at(&self) -> u64 {
        self.created_at
    }

    pub fn expires_at(&self) -> u64 {
        self.expires_at
    }

    pub fn max_attempts(&self) -> u32 {
        self.max_attempts
    }

    pub fn is_expired(&self, now: u64) -> bool {
        now >= self.expires_at
    }

    /// Constant-time comparison of a candidate code.
    pub fn matches_code(&self, candidate: &str) -> bool {
        constant_time_eq(&self.code, candidate)
    }

    /// QR/URI payload for this offer.
    pub fn uri(&self) -> String {
        format!(
            "{PAIRING_URI_PREFIX}node={}&nonce={}&fp={}",
            percent_encode(&self.node_id),
            percent_encode(&self.nonce),
            percent_encode(&self.fingerprint)
        )
    }

    /// Grouped digits for an OLED/console, for example `123 456`.
    pub fn human_readable(&self) -> String {
        let mut grouped = String::with_capacity(PAIRING_CODE_DIGITS + 1);
        for (index, ch) in self.code.chars().enumerate() {
            if index == 3 {
                grouped.push(' ');
            }
            grouped.push(ch);
        }
        grouped
    }
}

/// The parsed contents of a [`PairingCode::uri`] payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PairingUri {
    pub node_id: String,
    pub nonce: String,
    pub fingerprint: String,
}

impl PairingUri {
    /// Parse `ozma://pair?node=...&nonce=...&fp=...`.
    ///
    /// Unknown query parameters are ignored for forward compatibility.
    pub fn parse(uri: &str) -> Result<Self> {
        let query = uri
            .strip_prefix(PAIRING_URI_PREFIX)
            .ok_or_else(|| pairing_invalid("pairing URI must start with ozma://pair?"))?;

        let mut node_id = None;
        let mut nonce = None;
        let mut fingerprint = None;

        for pair in query.split('&') {
            if pair.is_empty() {
                continue;
            }
            let (key, value) = pair
                .split_once('=')
                .ok_or_else(|| pairing_invalid("malformed pairing URI parameter"))?;
            let value = percent_decode(value)?;
            match key {
                "node" => set_once(&mut node_id, value, "node")?,
                "nonce" => set_once(&mut nonce, value, "nonce")?,
                "fp" => set_once(&mut fingerprint, value, "fp")?,
                _ => {}
            }
        }

        let node_id = node_id.ok_or_else(|| pairing_invalid("pairing URI has no node id"))?;
        let nonce = nonce.ok_or_else(|| pairing_invalid("pairing URI has no nonce"))?;
        let fingerprint =
            fingerprint.ok_or_else(|| pairing_invalid("pairing URI has no fingerprint"))?;

        validate_label("node id", &node_id, 256)?;
        validate_label("nonce", &nonce, 512)?;
        validate_label("fingerprint", &fingerprint, 512)?;

        Ok(Self {
            node_id,
            nonce,
            fingerprint,
        })
    }
}

fn set_once(slot: &mut Option<String>, value: String, name: &str) -> Result<()> {
    if slot.replace(value).is_some() {
        return Err(pairing_invalid(format!(
            "pairing URI contains {name} more than once"
        )));
    }
    Ok(())
}

#[derive(Clone, Debug)]
struct PendingPairing {
    offer: PairingCode,
    attempts_remaining: u32,
}

#[derive(Debug, Default)]
struct PairingState {
    pending: HashMap<String, PendingPairing>,
}

/// Bounded, single-use pairing store.
#[derive(Debug)]
pub struct PairingStore {
    state: Mutex<PairingState>,
    max_pending: usize,
}

impl Default for PairingStore {
    fn default() -> Self {
        Self::new()
    }
}

impl PairingStore {
    /// A store with the default capacity.
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_MAX_PENDING_PAIRINGS)
    }

    /// A store with an explicit capacity (clamped to at least one).
    pub fn with_capacity(max_pending: usize) -> Self {
        Self {
            state: Mutex::new(PairingState::default()),
            max_pending: max_pending.max(1),
        }
    }

    fn state(&self) -> MutexGuard<'_, PairingState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Add a fresh offer. Expired offers and full stores are rejected.
    pub fn insert(&self, offer: PairingCode, now: u64) -> Result<()> {
        if offer.is_expired(now) {
            return Err(pairing_unavailable("cannot store an expired pairing offer"));
        }

        let mut state = self.state();
        prune_expired(&mut state, now);

        if state.pending.len() >= self.max_pending {
            return Err(pairing_unavailable("too many pending pairing offers").retryable());
        }
        if state.pending.contains_key(offer.nonce()) {
            return Err(pairing_unavailable("pairing nonce collision"));
        }

        state.pending.insert(
            offer.nonce.clone(),
            PendingPairing {
                attempts_remaining: offer.max_attempts,
                offer,
            },
        );
        Ok(())
    }

    /// Check a code and consume the offer on success.
    ///
    /// A wrong code decreases the attempt budget; exhausting it destroys the
    /// offer. An expired offer is removed and reported as
    /// `auth.pairing_expired`, while an unknown or already consumed nonce is
    /// `auth.pairing_invalid`.
    pub fn verify_and_consume(&self, nonce: &str, code: &str, now: u64) -> Result<PairingCode> {
        let mut state = self.state();

        let Some(pending) = state.pending.get(nonce) else {
            prune_expired(&mut state, now);
            return Err(pairing_invalid("pairing nonce is unknown or already used"));
        };

        let expired = pending.offer.is_expired(now);
        let expires_at = pending.offer.expires_at;
        let code_matches = constant_time_eq(pending.offer.code(), code);
        let attempts_after_failure = pending.attempts_remaining.saturating_sub(1);

        if expired {
            state.pending.remove(nonce);
            return Err(
                ApiError::new(codes::PAIRING_EXPIRED, "the pairing code has expired")
                    .with_details(serde_json::json!({
                        "expires_at": expires_at,
                        "now": now,
                    }))
                    .retryable(),
            );
        }

        if !code_matches {
            let attempts_remaining = attempts_after_failure;
            if attempts_remaining == 0 {
                state.pending.remove(nonce);
                return Err(ApiError::new(
                    codes::PAIRING_EXHAUSTED,
                    "too many incorrect pairing attempts; the code was destroyed",
                ));
            }
            if let Some(pending) = state.pending.get_mut(nonce) {
                pending.attempts_remaining = attempts_remaining;
            }
            return Err(
                ApiError::new(codes::PAIRING_REJECTED, "the pairing code is incorrect")
                    .with_details(serde_json::json!({
                        "attempts_remaining": attempts_remaining,
                    })),
            );
        }

        let pending = state
            .pending
            .remove(nonce)
            .expect("the pending offer was present above");
        Ok(pending.offer)
    }

    /// Number of pending (possibly expired but not yet pruned) offers.
    pub fn len(&self) -> usize {
        self.state().pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn prune_expired(state: &mut PairingState, now: u64) {
    state
        .pending
        .retain(|_, pending| !pending.offer.is_expired(now));
}

fn random_nonce() -> String {
    let mut bytes = [0u8; 32];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn validate_label(name: &str, value: &str, max_len: usize) -> Result<()> {
    if value.is_empty() {
        return Err(pairing_invalid(format!("{name} must not be empty")));
    }
    if value.len() > max_len {
        return Err(pairing_invalid(format!("{name} is too long")));
    }
    if value.chars().any(char::is_control) {
        return Err(pairing_invalid(format!(
            "{name} contains a control character"
        )));
    }
    Ok(())
}

fn pairing_invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::PAIRING_INVALID, message)
}

fn pairing_unavailable(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::PAIRING_UNAVAILABLE, message)
}

fn constant_time_eq(left: &str, right: &str) -> bool {
    let left = left.as_bytes();
    let right = right.as_bytes();
    if left.len() != right.len() {
        return false;
    }
    let mut difference = 0u8;
    for (a, b) in left.iter().zip(right.iter()) {
        difference |= a ^ b;
    }
    difference == 0
}

fn percent_encode(value: &str) -> String {
    let mut encoded = String::with_capacity(value.len());
    for byte in value.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b'~') {
            encoded.push(byte as char);
        } else {
            encoded.push('%');
            encoded.push_str(&format!("{byte:02X}"));
        }
    }
    encoded
}

fn percent_decode(value: &str) -> Result<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        match bytes[index] {
            b'%' => {
                let hex = bytes
                    .get(index + 1..index + 3)
                    .and_then(|pair| std::str::from_utf8(pair).ok())
                    .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                    .ok_or_else(|| pairing_invalid("pairing URI has invalid percent encoding"))?;
                decoded.push(hex);
                index += 3;
            }
            byte => {
                decoded.push(byte);
                index += 1;
            }
        }
    }
    String::from_utf8(decoded).map_err(|_| pairing_invalid("pairing URI is not valid UTF-8"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generates_six_digits_and_a_high_entropy_nonce() {
        let offer = PairingCode::generate("node-a", "SHA256:abcdef", 1_000).unwrap();
        assert_eq!(offer.code().len(), 6);
        assert!(offer.code().chars().all(|c| c.is_ascii_digit()));
        assert_eq!(offer.human_readable().len(), 7);
        assert_eq!(offer.human_readable().chars().nth(3), Some(' '));
        assert!(offer.nonce().len() >= 40);
        assert_eq!(offer.expires_at(), 1_060);
        assert!(!offer.is_expired(1_059));
        assert!(offer.is_expired(1_060));
        assert!(offer.matches_code(offer.code()));
        assert!(!offer.matches_code("000000") || offer.code() == "000000");
    }

    #[test]
    fn code_and_nonce_are_redacted_from_debug() {
        let offer = PairingCode::generate("node-a", "fp", 1_000).unwrap();
        let shown = format!("{offer:?}");
        assert!(shown.contains("node-a"));
        assert!(!shown.contains(offer.code()));
        assert!(!shown.contains(offer.nonce()));
        assert!(shown.contains("redacted"));
    }

    #[test]
    fn uri_roundtrips_special_characters() {
        let offer = PairingCode::generate("node a/1?&", "SHA256:ab+cd/ef=", 1_000).unwrap();
        let uri = offer.uri();
        assert!(uri.starts_with(PAIRING_URI_PREFIX));
        assert!(!uri.contains(' '));
        assert_eq!(
            uri.matches('?').count(),
            1,
            "only the scheme delimiter is raw"
        );

        let parsed = PairingUri::parse(&uri).unwrap();
        assert_eq!(parsed.node_id, offer.node_id());
        assert_eq!(parsed.nonce, offer.nonce());
        assert_eq!(parsed.fingerprint, offer.fingerprint());
    }

    #[test]
    fn uri_parser_rejects_wrong_scheme_and_missing_fields() {
        assert_eq!(
            PairingUri::parse("https://pair?node=a&nonce=b&fp=c")
                .unwrap_err()
                .code,
            codes::PAIRING_INVALID
        );
        assert_eq!(
            PairingUri::parse("ozma://pair?node=a&nonce=b")
                .unwrap_err()
                .code,
            codes::PAIRING_INVALID
        );
        assert_eq!(
            PairingUri::parse("ozma://pair?node=a&node=b&nonce=b&fp=c")
                .unwrap_err()
                .code,
            codes::PAIRING_INVALID
        );
        assert_eq!(
            PairingUri::parse("ozma://pair?node=%zz&nonce=b&fp=c")
                .unwrap_err()
                .code,
            codes::PAIRING_INVALID
        );
    }

    #[test]
    fn insert_and_consume_is_single_use() {
        let store = PairingStore::new();
        let offer = PairingCode::generate("node-a", "fp", 1_000).unwrap();
        store.insert(offer.clone(), 1_000).unwrap();

        let consumed = store
            .verify_and_consume(offer.nonce(), offer.code(), 1_010)
            .unwrap();
        assert_eq!(consumed.code(), offer.code());
        assert!(store.is_empty());

        let replay = store
            .verify_and_consume(offer.nonce(), offer.code(), 1_010)
            .unwrap_err();
        assert_eq!(replay.code, codes::PAIRING_INVALID);
    }

    #[test]
    fn wrong_codes_burn_attempts_then_destroy_the_offer() {
        let store = PairingStore::new();
        let offer = PairingCode::generate_with("node-a", "fp", 1_000, 60, 2).unwrap();
        store.insert(offer.clone(), 1_000).unwrap();

        let error = store
            .verify_and_consume(offer.nonce(), "not-the-code", 1_001)
            .unwrap_err();
        assert_eq!(error.code, codes::PAIRING_REJECTED);
        assert_eq!(error.details["attempts_remaining"], 1);

        let error = store
            .verify_and_consume(offer.nonce(), "still-wrong", 1_002)
            .unwrap_err();
        assert_eq!(error.code, codes::PAIRING_EXHAUSTED);
        assert!(store.is_empty());

        // The correct code no longer works after exhaustion.
        let error = store
            .verify_and_consume(offer.nonce(), offer.code(), 1_003)
            .unwrap_err();
        assert_eq!(error.code, codes::PAIRING_INVALID);
    }

    #[test]
    fn expired_offers_are_reported_and_removed() {
        let store = PairingStore::new();
        let offer = PairingCode::generate("node-a", "fp", 1_000).unwrap();
        store.insert(offer.clone(), 1_000).unwrap();

        let error = store
            .verify_and_consume(offer.nonce(), offer.code(), 1_060)
            .unwrap_err();
        assert_eq!(error.code, codes::PAIRING_EXPIRED);
        assert!(error.retryable);
        assert!(store.is_empty());
    }

    #[test]
    fn pending_offers_are_bounded_and_expired_ones_are_pruned() {
        let store = PairingStore::with_capacity(1);
        let first = PairingCode::generate("node-a", "fp", 1_000).unwrap();
        store.insert(first.clone(), 1_000).unwrap();

        let second = PairingCode::generate("node-a", "fp", 1_050).unwrap();
        let error = store.insert(second.clone(), 1_051).unwrap_err();
        assert_eq!(error.code, codes::PAIRING_UNAVAILABLE);
        assert!(error.retryable);

        // Once the first offer expires at 1_060, the second one can be stored.
        store.insert(second, 1_060).unwrap();
        assert_eq!(store.len(), 1);
    }

    #[test]
    fn generation_rejects_bad_labels_and_zero_limits() {
        assert_eq!(
            PairingCode::generate("", "fp", 1_000).unwrap_err().code,
            codes::PAIRING_INVALID
        );
        assert_eq!(
            PairingCode::generate("node", "", 1_000).unwrap_err().code,
            codes::PAIRING_INVALID
        );
        assert_eq!(
            PairingCode::generate("node\n", "fp", 1_000)
                .unwrap_err()
                .code,
            codes::PAIRING_INVALID
        );
        assert_eq!(
            PairingCode::generate_with("node", "fp", 1_000, 0, 5)
                .unwrap_err()
                .code,
            codes::PAIRING_UNAVAILABLE
        );
        assert_eq!(
            PairingCode::generate_with("node", "fp", 1_000, 60, 0)
                .unwrap_err()
                .code,
            codes::PAIRING_UNAVAILABLE
        );
    }

    #[test]
    fn expired_offer_cannot_be_inserted() {
        let store = PairingStore::new();
        let offer = PairingCode::generate("node", "fp", 1_000).unwrap();
        let error = store.insert(offer, 1_060).unwrap_err();
        assert_eq!(error.code, codes::PAIRING_UNAVAILABLE);
    }
}
