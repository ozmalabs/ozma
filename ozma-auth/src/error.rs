// SPDX-License-Identifier: AGPL-3.0-only
//! Authentication-specific error codes.
//!
//! The core protocol only guarantees the transport/API codes in
//! `ozma_protocol::codes`. Authentication providers add namespaced codes so a
//! caller can tell "this backend is not installed" apart from "the credential
//! was wrong" without parsing a message.

/// Stable `auth.*` error codes emitted by this crate's providers.
pub mod codes {
    // PAM
    /// The platform has no usable PAM stack (or the build omitted it).
    pub const PAM_UNAVAILABLE: &str = "auth.pam_unavailable";
    /// PAM was reached and refused the credential or the account policy.
    pub const PAM_REJECTED: &str = "auth.pam_rejected";
    /// PAM is configured incorrectly (bad service name, unusable policy, ...).
    pub const PAM_CONFIG: &str = "auth.pam_config";

    // SSH public-key authentication
    /// The OpenSSH client tooling or the authorized-keys file is unavailable.
    pub const SSH_UNAVAILABLE: &str = "auth.ssh_unavailable";
    /// The supplied key or signature did not authenticate the principal.
    pub const SSH_REJECTED: &str = "auth.ssh_rejected";
    /// The SSH provider is misconfigured (path, permissions, ...).
    pub const SSH_CONFIG: &str = "auth.ssh_config";
    /// A public key was presented where a challenge signature is required.
    pub const SSH_CHALLENGE_REQUIRED: &str = "auth.ssh_challenge_required";
    /// The challenge id is unknown, already consumed, or bound elsewhere.
    pub const SSH_CHALLENGE_INVALID: &str = "auth.ssh_challenge_invalid";
    /// The challenge existed but its time-to-live elapsed.
    pub const SSH_CHALLENGE_EXPIRED: &str = "auth.ssh_challenge_expired";

    // Pairing
    /// The pairing code did not match.
    pub const PAIRING_REJECTED: &str = "auth.pairing_rejected";
    /// The pairing attempt used an unknown/consumed nonce.
    pub const PAIRING_INVALID: &str = "auth.pairing_invalid";
    /// The pairing offer expired before it was consumed.
    pub const PAIRING_EXPIRED: &str = "auth.pairing_expired";
    /// Too many wrong codes were entered; the offer was destroyed.
    pub const PAIRING_EXHAUSTED: &str = "auth.pairing_exhausted";
    /// The pairing store has no room, or the offer is malformed.
    pub const PAIRING_UNAVAILABLE: &str = "auth.pairing_unavailable";
}

pub use codes::*;
