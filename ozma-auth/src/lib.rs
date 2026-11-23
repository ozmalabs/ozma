// SPDX-License-Identifier: AGPL-3.0-only
//! Authentication and authorization primitives for the Ozma node.
//!
//! This crate provides:
//!
//! * [`TokenIssuer`]/[`TokenVerifier`] — HMAC-SHA256 signed, scoped session
//!   tokens with an expiry and a nonce.
//! * [`ConsentPolicy`] — decides whether a session needs local consent on this
//!   machine class.
//! * [`GrantStore`] — in-memory client grants with revocation and expiry.
//! * [`LocalAuthenticator`] — the extension point for local OS authentication.
//! * [`PamAuthenticator`] — Linux PAM password authentication behind the
//!   `LocalAuthenticator` trait. Non-Linux builds compile a stub that reports
//!   `auth.pam_unavailable` and never links libpam.
//! * [`SshKeyAuthenticator`] — OpenSSH `authorized_keys` matching plus SSHSIG
//!   challenge verification through `ssh-keygen -Y verify`.
//! * [`PairingCode`]/[`PairingStore`] — six-digit + high-entropy short-code
//!   pairing with a URI/QR payload, expiry, single use, and an attempt limit.
//!
//! Passkey/WebAuthn pairing is the preferred browser/platform flow but is not
//! implemented in this crate yet; short-code pairing is the constrained-console
//! fallback.
//!
//! Only the Linux PAM FFI module opts out of the crate-level
//! `deny(unsafe_code)`; the rest of the crate is safe Rust.
//!
//! Secrets and tokens are never logged or stored in debug output by this
//! crate; token issuers, passwords, pairing codes, and pairing nonces redact
//! themselves.

#![deny(unsafe_code)]

pub mod consent;
pub mod credential;
pub mod error;
pub mod grant;
pub mod pairing;
pub mod pam;
pub mod ssh;
pub mod token;

pub use consent::{ConsentPolicy, MachineClass};
pub use credential::{AuthenticatedIdentity, Credential, LocalAuthenticator};
pub use error::codes as auth_codes;
pub use grant::{ClientGrant, Grant, GrantStore};
pub use pairing::{
    PairingCode, PairingStore, PairingUri, DEFAULT_MAX_PENDING_PAIRINGS,
    DEFAULT_PAIRING_MAX_ATTEMPTS, DEFAULT_PAIRING_TTL_SECONDS, PAIRING_CODE_DIGITS,
    PAIRING_URI_PREFIX,
};
pub use pam::{PamAuthenticator, PamConfig, UserPolicy, DEFAULT_PAM_SERVICE, FALLBACK_PAM_SERVICE};
pub use ssh::{
    AuthorizedKey, AuthorizedKeys, SshChallenge, SshChallengeStore, SshKeyAuthenticator,
    DEFAULT_MAX_PENDING_SSH_CHALLENGES, DEFAULT_SSH_CHALLENGE_TTL_SECONDS, SSH_CHALLENGE_NAMESPACE,
};
pub use token::{TokenClaims, TokenIssuer, TokenVerifier};

/// How long a pending consent request stays approvable.
pub const DEFAULT_CONSENT_TIMEOUT_SECONDS: u64 = 120;
