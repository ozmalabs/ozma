// SPDX-License-Identifier: AGPL-3.0-only
//! Scoped, signed session tokens.
//!
//! A token is `base64url(payload).base64url(hmac_sha256(domain || payload))`.
//! The payload is a JSON [`TokenClaims`]. Tokens are short-lived, bound to one
//! session and client, and carry the permissions granted for that session.
//!
//! Verification is constant-time with respect to the MAC (`hmac` uses
//! `subtle` internally). Malformed, tampered, or mis-bound tokens return
//! `token.invalid`; an otherwise valid token past its expiry returns
//! `token.expired`.
//!
//! Neither [`TokenIssuer`] nor [`TokenVerifier`] exposes its secret through
//! `Debug`, and no method logs a token.

use std::fmt;

use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use base64::Engine as _;
use hmac::{Hmac, Mac};
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use sha2::Sha256;

use ozma_protocol::codes;
use ozma_protocol::{ApiError, ClientId, NodeId, PermissionSet, Result, SessionId};

type HmacSha256 = Hmac<Sha256>;

/// Domain separator so these MACs cannot be confused with another protocol.
const MAC_DOMAIN: &[u8] = b"ozma-session-token-v1";

/// The signed payload of a session token.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct TokenClaims {
    /// The session this token authorizes.
    pub session_id: SessionId,
    /// The paired client that presented the token.
    pub client_id: ClientId,
    /// Optional node binding. A token issued for one node should not verify on
    /// another.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node_id: Option<NodeId>,
    /// Permissions granted for the session. This is a subset of the client's
    /// long-lived grant.
    pub permissions: PermissionSet,
    /// Issue time, seconds since the Unix epoch.
    pub issued_at: u64,
    /// Expiry time, seconds since the Unix epoch. Tokens expire at
    /// `now >= expires_at`.
    pub expires_at: u64,
    /// Single-use random value. Lets the verifier/controller detect replay
    /// where state is kept; every issued token gets a fresh nonce.
    pub nonce: String,
}

impl TokenClaims {
    /// Build claims with a fresh random nonce.
    pub fn new(
        session_id: impl Into<SessionId>,
        client_id: impl Into<ClientId>,
        permissions: PermissionSet,
        issued_at: u64,
        expires_at: u64,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            client_id: client_id.into(),
            node_id: None,
            permissions,
            issued_at,
            expires_at,
            nonce: generate_nonce(),
        }
    }

    /// Bind the claims to a node.
    pub fn with_node_id(mut self, node_id: impl Into<NodeId>) -> Self {
        self.node_id = Some(node_id.into());
        self
    }

    /// Replace the generated nonce, primarily for deterministic tests.
    pub fn with_nonce(mut self, nonce: impl Into<String>) -> Self {
        self.nonce = nonce.into();
        self
    }
}

/// Issues tokens with a shared secret.
#[derive(Clone)]
pub struct TokenIssuer {
    secret: Vec<u8>,
}

impl fmt::Debug for TokenIssuer {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenIssuer")
            .field("secret", &"[redacted]")
            .finish()
    }
}

impl TokenIssuer {
    /// Create an issuer. The secret is copied and never exposed.
    ///
    /// Production callers should use at least 32 random bytes per node or
    /// controller; tests may use a short literal.
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self {
            secret: secret.as_ref().to_vec(),
        }
    }

    /// Sign an exact set of claims. This is deterministic apart from the nonce
    /// the caller supplies.
    pub fn issue(&self, claims: &TokenClaims) -> Result<String> {
        if let Some(problem) = claims_problem(claims) {
            return Err(ApiError::invalid(format!(
                "cannot issue session token: {problem}"
            )));
        }

        let json = serde_json::to_vec(claims)
            .map_err(|error| ApiError::internal(format!("cannot encode token claims: {error}")))?;
        let payload = URL_SAFE_NO_PAD.encode(json);
        let signature = self.sign(payload.as_bytes())?;

        Ok(format!(
            "{payload}.{}",
            URL_SAFE_NO_PAD.encode(signature.as_slice())
        ))
    }

    /// Build claims for a fresh session token with a generated nonce, sign
    /// them, and return both the token and its claims.
    pub fn issue_new(
        &self,
        session_id: impl Into<SessionId>,
        client_id: impl Into<ClientId>,
        permissions: PermissionSet,
        ttl_seconds: u64,
        now: u64,
    ) -> Result<(String, TokenClaims)> {
        let claims = TokenClaims::new(
            session_id,
            client_id,
            permissions,
            now,
            now.saturating_add(ttl_seconds),
        );
        let token = self.issue(&claims)?;
        Ok((token, claims))
    }

    fn sign(&self, payload: &[u8]) -> Result<Vec<u8>> {
        let mut mac = HmacSha256::new_from_slice(&self.secret).map_err(|_| {
            ApiError::internal("the configured token secret cannot be used for HMAC-SHA256")
        })?;
        mac.update(MAC_DOMAIN);
        mac.update(payload);
        Ok(mac.finalize().into_bytes().to_vec())
    }
}

/// Verifies tokens with the same secret used by the issuer.
#[derive(Clone)]
pub struct TokenVerifier {
    secret: Vec<u8>,
    clock_skew_seconds: u64,
}

impl fmt::Debug for TokenVerifier {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("TokenVerifier")
            .field("secret", &"[redacted]")
            .field("clock_skew_seconds", &self.clock_skew_seconds)
            .finish()
    }
}

impl TokenVerifier {
    /// Create a verifier. `clock_skew_seconds` defaults to 60.
    pub fn new(secret: impl AsRef<[u8]>) -> Self {
        Self {
            secret: secret.as_ref().to_vec(),
            clock_skew_seconds: 60,
        }
    }

    /// Tolerate a small amount of clock difference for `issued_at`.
    pub fn with_clock_skew(mut self, seconds: u64) -> Self {
        self.clock_skew_seconds = seconds;
        self
    }

    /// Verify the signature, claims shape, and expiry.
    pub fn verify(&self, token: &str, now: u64) -> Result<TokenClaims> {
        let (payload, signature_b64) = token.split_once('.').ok_or_else(|| {
            invalid("Session token is missing its signature segment")
        })?;
        if payload.is_empty() || signature_b64.is_empty() || signature_b64.contains('.') {
            return Err(invalid("Session token has an invalid encoding"));
        }

        let signature = URL_SAFE_NO_PAD
            .decode(signature_b64)
            .map_err(|_| invalid("Session token signature is not valid base64url"))?;

        let mut mac = HmacSha256::new_from_slice(&self.secret)
            .map_err(|_| ApiError::internal("the configured token secret cannot be used"))?;
        mac.update(MAC_DOMAIN);
        mac.update(payload.as_bytes());
        // `verify_slice` is constant-time; no byte-by-byte comparison here.
        mac.verify_slice(&signature)
            .map_err(|_| invalid("Session token signature does not match"))?;

        let claims_bytes = URL_SAFE_NO_PAD
            .decode(payload)
            .map_err(|_| invalid("Session token payload is not valid base64url"))?;
        let claims: TokenClaims = serde_json::from_slice(&claims_bytes)
            .map_err(|_| invalid("Session token payload is not valid JSON claims"))?;

        if let Some(problem) = claims_problem(&claims) {
            return Err(invalid(problem));
        }

        if claims.issued_at > now.saturating_add(self.clock_skew_seconds) {
            return Err(invalid("Session token was issued in the future"));
        }

        if now >= claims.expires_at {
            return Err(ApiError::new(
                codes::TOKEN_EXPIRED,
                "The session token has expired",
            )
            .with_details(serde_json::json!({
                "expires_at": claims.expires_at,
                "now": now,
            }))
            .retryable());
        }

        Ok(claims)
    }

    /// Verify and require that the token is bound to `node_id`.
    pub fn verify_for_node(&self, token: &str, now: u64, node_id: &NodeId) -> Result<TokenClaims> {
        let claims = self.verify(token, now)?;
        match &claims.node_id {
            Some(bound) if bound == node_id => Ok(claims),
            Some(bound) => Err(invalid(format!(
                "Session token is bound to node {bound}, not {node_id}"
            ))),
            None => Err(invalid(
                "Session token is not bound to a node but node binding is required",
            )),
        }
    }
}

fn claims_problem(claims: &TokenClaims) -> Option<&'static str> {
    if claims.session_id.is_empty() {
        return Some("token claims have an empty session_id");
    }
    if claims.client_id.is_empty() {
        return Some("token claims have an empty client_id");
    }
    if claims.nonce.is_empty() {
        return Some("token claims have an empty nonce");
    }
    if claims.expires_at <= claims.issued_at {
        return Some("token claims expire at or before their issue time");
    }
    None
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError::new(codes::TOKEN_INVALID, message)
}

/// A random 128-bit nonce, encoded as URL-safe base64 without padding.
pub fn generate_nonce() -> String {
    let mut bytes = [0u8; 16];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::Permission;

    const SECRET: &[u8] = b"test-secret-for-ozma-session-tokens";

    fn permissions() -> PermissionSet {
        [Permission::View, Permission::Control]
            .into_iter()
            .collect()
    }

    fn claims() -> TokenClaims {
        TokenClaims::new(
            "session-1",
            "client-1",
            permissions(),
            1_000,
            1_600,
        )
        .with_nonce("fixed-nonce")
        .with_node_id("node-a")
    }

    #[test]
    fn issues_and_verifies_claims() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(SECRET);
        let expected = claims();

        let token = issuer.issue(&expected).unwrap();
        let decoded = verifier.verify(&token, 1_100).unwrap();

        assert_eq!(decoded, expected);
    }

    #[test]
    fn tokens_are_url_safe_and_deterministic_for_fixed_claims() {
        let issuer = TokenIssuer::new(SECRET);
        let first = issuer.issue(&claims()).unwrap();
        let second = issuer.issue(&claims()).unwrap();

        assert_eq!(first, second);
        assert!(!first.contains('='));
        assert!(!first.contains('+'));
        assert!(!first.contains('/'));
    }

    #[test]
    fn a_different_nonce_produces_a_different_token() {
        let issuer = TokenIssuer::new(SECRET);
        let first = issuer.issue(&claims()).unwrap();
        let second = issuer
            .issue(&claims().with_nonce("another-nonce"))
            .unwrap();
        assert_ne!(first, second);
    }

    #[test]
    fn rejects_a_tampered_payload() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(SECRET);
        let token = issuer.issue(&claims()).unwrap();
        let (payload, signature) = token.split_once('.').unwrap();

        // Decode, change the client id, and re-encode without re-signing.
        let bytes = URL_SAFE_NO_PAD.decode(payload).unwrap();
        let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        value["client_id"] = serde_json::json!("attacker");
        let forged_payload = URL_SAFE_NO_PAD.encode(serde_json::to_vec(&value).unwrap());
        let forged = format!("{forged_payload}.{signature}");

        let error = verifier.verify(&forged, 1_100).unwrap_err();
        assert_eq!(error.code, codes::TOKEN_INVALID);
    }

    #[test]
    fn rejects_a_tampered_signature() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(SECRET);
        let token = issuer.issue(&claims()).unwrap();
        let (payload, signature) = token.split_once('.').unwrap();
        let mut bytes = URL_SAFE_NO_PAD.decode(signature).unwrap();
        bytes[0] ^= 0x01;
        let forged = format!("{payload}.{}", URL_SAFE_NO_PAD.encode(bytes));

        let error = verifier.verify(&forged, 1_100).unwrap_err();
        assert_eq!(error.code, codes::TOKEN_INVALID);
    }

    #[test]
    fn rejects_a_different_secret() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(b"a-completely-different-secret");
        let token = issuer.issue(&claims()).unwrap();

        let error = verifier.verify(&token, 1_100).unwrap_err();
        assert_eq!(error.code, codes::TOKEN_INVALID);
    }

    #[test]
    fn rejects_malformed_tokens() {
        let verifier = TokenVerifier::new(SECRET);
        for token in ["", "one-segment", "two..segments", ".", "a.b.c"] {
            let error = verifier.verify(token, 1_100).unwrap_err();
            assert_eq!(error.code, codes::TOKEN_INVALID, "token: {token:?}");
        }
    }

    #[test]
    fn expired_tokens_are_reported_separately() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(SECRET);
        let token = issuer.issue(&claims()).unwrap();

        let error = verifier.verify(&token, 1_600).unwrap_err();
        assert_eq!(error.code, codes::TOKEN_EXPIRED);
        assert!(error.retryable);
    }

    #[test]
    fn expiry_is_exclusive_at_the_boundary() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(SECRET);
        let token = issuer.issue(&claims()).unwrap();

        assert!(verifier.verify(&token, 1_599).is_ok());
        assert!(verifier.verify(&token, 1_600).is_err());
    }

    #[test]
    fn node_binding_is_enforced() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(SECRET);
        let token = issuer.issue(&claims()).unwrap();

        assert_eq!(
            verifier
                .verify_for_node(&token, 1_100, &NodeId::new("node-a"))
                .unwrap()
                .client_id,
            ClientId::new("client-1")
        );

        let error = verifier
            .verify_for_node(&token, 1_100, &NodeId::new("node-b"))
            .unwrap_err();
        assert_eq!(error.code, codes::TOKEN_INVALID);
    }

    #[test]
    fn issue_new_sets_expiry_and_a_fresh_nonce() {
        let issuer = TokenIssuer::new(SECRET);
        let (first, first_claims) = issuer
            .issue_new("session-1", "client-1", permissions(), 300, 1_000)
            .unwrap();
        let (second, second_claims) = issuer
            .issue_new("session-1", "client-1", permissions(), 300, 1_000)
            .unwrap();

        assert_eq!(first_claims.expires_at, 1_300);
        assert_eq!(first_claims.issued_at, 1_000);
        assert_ne!(first_claims.nonce, second_claims.nonce);
        assert_ne!(first, second);
    }

    #[test]
    fn refuses_to_issue_empty_claims() {
        let issuer = TokenIssuer::new(SECRET);
        let mut empty = claims();
        empty.session_id = SessionId::new("");
        assert_eq!(
            issuer.issue(&empty).unwrap_err().code,
            codes::INVALID_REQUEST
        );

        let mut expired = claims();
        expired.expires_at = expired.issued_at;
        assert_eq!(
            issuer.issue(&expired).unwrap_err().code,
            codes::INVALID_REQUEST
        );
    }

    #[test]
    fn debug_does_not_contain_the_secret() {
        let issuer = TokenIssuer::new(SECRET);
        let verifier = TokenVerifier::new(SECRET);
        let issuer_debug = format!("{issuer:?}");
        let verifier_debug = format!("{verifier:?}");
        assert!(!issuer_debug.contains("test-secret"));
        assert!(!verifier_debug.contains("test-secret"));
        assert!(issuer_debug.contains("redacted"));
        assert!(verifier_debug.contains("redacted"));
    }
}
