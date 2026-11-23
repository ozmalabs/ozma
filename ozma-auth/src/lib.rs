// SPDX-License-Identifier: AGPL-3.0-only
//! Authentication and authorization primitives for the Ozma node.
//!
//! This crate is deliberately synchronous and I/O-free. It provides:
//!
//! * [`TokenIssuer`]/[`TokenVerifier`] — HMAC-SHA256 signed, scoped session
//!   tokens with an expiry and a nonce.
//! * [`ConsentPolicy`] — decides whether a session needs local consent on this
//!   machine class.
//! * [`GrantStore`] — in-memory client grants with revocation and expiry.
//! * [`LocalAuthenticator`] — the extension point later used by PAM and
//!   SSH-key adapters. No platform authentication is implemented in this pass.
//!
//! Secrets and tokens are never logged or stored in debug output by this
//! crate; the issuer and verifier redact their keys.

#![forbid(unsafe_code)]

pub mod consent;
pub mod credential;
pub mod grant;
pub mod token;

pub use consent::{ConsentPolicy, MachineClass};
pub use credential::{AuthenticatedIdentity, Credential, LocalAuthenticator};
pub use grant::{ClientGrant, Grant, GrantStore};
pub use token::{TokenClaims, TokenIssuer, TokenVerifier};

/// How long a pending consent request stays approvable.
pub const DEFAULT_CONSENT_TIMEOUT_SECONDS: u64 = 120;
