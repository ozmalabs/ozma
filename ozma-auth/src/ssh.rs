// SPDX-License-Identifier: AGPL-3.0-only
//! SSH public-key authentication with an SSHSIG challenge.
//!
//! The caller presents a public key *and* an SSHSIG signature over a nonce
//! issued by [`SshChallengeStore`]. The presented key is only used after it
//! matches an entry in the configured OpenSSH `authorized_keys` file; a key
//! supplied by the caller is never trusted on its own. Signature verification
//! runs `ssh-keygen -Y verify` with a temporary, 0600 allowed-signers file and
//! the fixed namespace [`SSH_CHALLENGE_NAMESPACE`].
//!
//! SSHSIG signatures are made by the client with:
//!
//! ```text
//! ssh-keygen -Y sign -n ozma-auth -f ~/.ssh/id_ed25519 nonce
//! ```
//!
//! and the resulting `nonce.sig` is sent back. This crate does not implement
//! Ed25519 itself, so it inherits OpenSSH's parser and algorithm handling.
//!
//! `authorized_keys` options are parsed and skipped; `from=`, command
//! restrictions, and certificate-authority options are **not** enforced in this
//! pass. A node that needs those should gate the provider behind a dedicated
//! account or policy.

use std::collections::HashMap;
use std::fs;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Output, Stdio};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use rand::rngs::OsRng;
use rand::RngCore;
use serde::{Deserialize, Serialize};

use ozma_protocol::{ApiError, PermissionSet, Result};

use crate::credential::{AuthenticatedIdentity, Credential, LocalAuthenticator};
use crate::error::codes;

/// SSHSIG namespace used by Ozma. Both signing and verification use it.
pub const SSH_CHALLENGE_NAMESPACE: &str = "ozma-auth";
/// Default lifetime of a pending SSH challenge.
pub const DEFAULT_SSH_CHALLENGE_TTL_SECONDS: u64 = 60;
/// Default upper bound on pending challenges.
pub const DEFAULT_MAX_PENDING_SSH_CHALLENGES: usize = 64;

/// Principal written into the temporary allowed-signers file.
///
/// The caller's requested principal is checked separately; using a constant
/// here keeps caller input out of a file format that has an options grammar.
const ALLOWED_SIGNER_PRINCIPAL: &str = "ozma-auth";

const KEY_TYPES: &[&str] = &[
    "ssh-ed25519",
    "ssh-ed448",
    "ssh-rsa",
    "ssh-dss",
    "ecdsa-sha2-nistp256",
    "ecdsa-sha2-nistp384",
    "ecdsa-sha2-nistp521",
    "sk-ssh-ed25519@openssh.com",
    "sk-ecdsa-sha2-nistp256@openssh.com",
];

/// One usable line from an OpenSSH `authorized_keys` file.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthorizedKey {
    key_type: String,
    key_base64: String,
    blob: Vec<u8>,
    comment: Option<String>,
}

impl AuthorizedKey {
    /// The OpenSSH key algorithm (`ssh-ed25519`, `ssh-rsa`, ...).
    pub fn key_type(&self) -> &str {
        &self.key_type
    }

    /// The base64 key material exactly as it appeared in the file.
    pub fn key_base64(&self) -> &str {
        &self.key_base64
    }

    /// Optional trailing comment, preserved for audit/UX only.
    pub fn comment(&self) -> Option<&str> {
        self.comment.as_deref()
    }

    /// A canonical `keytype base64` line (no options, no comment).
    pub fn public_key_line(&self) -> String {
        format!("{} {}", self.key_type, self.key_base64)
    }
}

/// Parsed `authorized_keys` contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AuthorizedKeys {
    keys: Vec<AuthorizedKey>,
    skipped_lines: usize,
}

impl AuthorizedKeys {
    /// All valid keys in file order.
    pub fn keys(&self) -> &[AuthorizedKey] {
        &self.keys
    }

    /// Non-comment lines that were not a usable key.
    pub fn skipped_lines(&self) -> usize {
        self.skipped_lines
    }

    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    /// Find the authorized entry matching a caller-supplied public key line.
    ///
    /// Options and a trailing comment on the caller's line are ignored; only
    /// the algorithm and decoded key blob are compared.
    pub fn matches(&self, public_key: &[u8]) -> Option<&AuthorizedKey> {
        let candidate = parse_key_line(std::str::from_utf8(public_key).ok()?.trim())?;
        self.keys
            .iter()
            .find(|key| key.key_type == candidate.key_type && key.blob.as_slice() == candidate.blob)
    }
}

/// Parse an OpenSSH `authorized_keys` file.
///
/// Blank lines and `#` comments are ignored. Options before the key type are
/// skipped (including quoted options containing spaces). Lines whose key is
/// malformed or whose embedded algorithm does not match its text are counted
/// in [`AuthorizedKeys::skipped_lines`] rather than aborting the whole file.
pub fn parse_authorized_keys(input: &str) -> AuthorizedKeys {
    let mut keys = Vec::new();
    let mut skipped_lines = 0;

    for raw in input.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match parse_key_line(line) {
            Some(key) => keys.push(key),
            None => skipped_lines += 1,
        }
    }

    AuthorizedKeys {
        keys,
        skipped_lines,
    }
}

fn parse_key_line(line: &str) -> Option<AuthorizedKey> {
    let tokens = split_tokens(line);
    let type_index = tokens.iter().position(|token| is_key_type(token))?;
    let key_type = tokens[type_index].to_owned();
    let key_base64 = (*tokens.get(type_index + 1)?).to_owned();
    let blob = decode_key_blob(&key_base64)?;

    // The blob carries its own algorithm name; refusing a mismatch prevents a
    // line from claiming one algorithm while containing another key.
    if blob_key_type(&blob)? != key_type.as_str() {
        return None;
    }

    let comment = tokens
        .get(type_index + 2..)
        .map(|rest| rest.join(" "))
        .filter(|comment| !comment.is_empty());

    Some(AuthorizedKey {
        key_type,
        key_base64,
        blob,
        comment,
    })
}

fn is_key_type(token: &str) -> bool {
    KEY_TYPES.contains(&token)
}

/// Split a key line on unquoted whitespace. Quoted spans and backslash escapes
/// are kept inside one token so `command="echo hello"` is not mistaken for a
/// key type or extra argument.
fn split_tokens(line: &str) -> Vec<&str> {
    let bytes = line.as_bytes();
    let mut tokens = Vec::new();
    let mut start = None;
    let mut in_quotes = false;
    let mut escaped = false;

    for (index, byte) in bytes.iter().copied().enumerate() {
        if in_quotes {
            if escaped {
                escaped = false;
            } else if byte == b'\\' {
                escaped = true;
            } else if byte == b'"' {
                in_quotes = false;
            }
            continue;
        }

        match byte {
            b' ' | b'\t' => {
                if let Some(begin) = start.take() {
                    tokens.push(&line[begin..index]);
                }
            }
            b'"' => {
                start.get_or_insert(index);
                in_quotes = true;
            }
            _ => {
                start.get_or_insert(index);
            }
        }
    }

    if let Some(begin) = start {
        tokens.push(&line[begin..]);
    }
    tokens
}

fn decode_key_blob(value: &str) -> Option<Vec<u8>> {
    STANDARD
        .decode(value)
        .or_else(|_| STANDARD_NO_PAD.decode(value))
        .ok()
}

fn blob_key_type(blob: &[u8]) -> Option<&str> {
    if blob.len() < 4 {
        return None;
    }
    let length = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    let end = 4usize.checked_add(length)?;
    if length == 0 || blob.len() < end {
        return None;
    }
    std::str::from_utf8(&blob[4..end]).ok()
}

/// A pending SSH challenge. `nonce` is signed by the client; `challenge_id`
/// addresses the pending entry and is single-use.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SshChallenge {
    pub challenge_id: String,
    /// URL-safe base64 of 32 random bytes. The client signs these bytes.
    pub nonce: String,
    /// Always [`SSH_CHALLENGE_NAMESPACE`].
    pub namespace: String,
    /// Expiry in seconds since the Unix epoch; valid while `now < expires_at`.
    pub expires_at: u64,
}

#[derive(Clone, Debug)]
struct PendingChallenge {
    nonce: String,
    principal: String,
    expires_at: u64,
}

#[derive(Debug, Default)]
struct ChallengeState {
    pending: HashMap<String, PendingChallenge>,
}

/// Bounded, single-use store of SSH challenges.
#[derive(Debug)]
pub struct SshChallengeStore {
    state: Mutex<ChallengeState>,
    ttl_seconds: u64,
    max_pending: usize,
}

impl Default for SshChallengeStore {
    fn default() -> Self {
        Self::new()
    }
}

impl SshChallengeStore {
    /// A store with a 60 second TTL and 64 pending challenges.
    pub fn new() -> Self {
        Self::with_limits(
            DEFAULT_MAX_PENDING_SSH_CHALLENGES,
            DEFAULT_SSH_CHALLENGE_TTL_SECONDS,
        )
    }

    /// A store with explicit limits. `max_pending` is clamped to at least one.
    pub fn with_limits(max_pending: usize, ttl_seconds: u64) -> Self {
        Self {
            state: Mutex::new(ChallengeState::default()),
            ttl_seconds,
            max_pending: max_pending.max(1),
        }
    }

    fn state(&self) -> MutexGuard<'_, ChallengeState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Issue a challenge bound to `principal`.
    pub fn issue(&self, principal: &str, now: u64) -> Result<SshChallenge> {
        if principal.trim().is_empty() || principal.chars().any(char::is_control) {
            return Err(ApiError::new(
                codes::SSH_CHALLENGE_INVALID,
                "an SSH challenge requires a non-empty principal",
            ));
        }

        let mut state = self.state();
        prune_expired(&mut state, now);

        if state.pending.len() >= self.max_pending {
            return Err(ApiError::new(
                codes::SSH_CHALLENGE_INVALID,
                "too many pending SSH challenges",
            )
            .with_details(serde_json::json!({ "max_pending": self.max_pending }))
            .retryable());
        }

        let mut challenge_id = random_token(16);
        while state.pending.contains_key(&challenge_id) {
            challenge_id = random_token(16);
        }

        let nonce = random_token(32);
        state.pending.insert(
            challenge_id.clone(),
            PendingChallenge {
                nonce: nonce.clone(),
                principal: principal.to_owned(),
                expires_at: now.saturating_add(self.ttl_seconds),
            },
        );

        Ok(SshChallenge {
            challenge_id,
            nonce,
            namespace: SSH_CHALLENGE_NAMESPACE.to_owned(),
            expires_at: now.saturating_add(self.ttl_seconds),
        })
    }

    /// Consume a challenge exactly once.
    ///
    /// The entry is removed even when the principal is wrong, so a stolen
    /// challenge id cannot be retried against a different identity.
    pub fn consume(&self, challenge_id: &str, principal: &str, now: u64) -> Result<SshChallenge> {
        let mut state = self.state();
        let pending = state.pending.remove(challenge_id).ok_or_else(|| {
            ApiError::new(
                codes::SSH_CHALLENGE_INVALID,
                "SSH challenge is unknown or has already been used",
            )
        })?;

        if now >= pending.expires_at {
            return Err(
                ApiError::new(codes::SSH_CHALLENGE_EXPIRED, "SSH challenge has expired")
                    .with_details(serde_json::json!({
                        "expires_at": pending.expires_at,
                        "now": now,
                    }))
                    .retryable(),
            );
        }

        if pending.principal != principal {
            return Err(ApiError::new(
                codes::SSH_CHALLENGE_INVALID,
                "SSH challenge is bound to a different principal",
            ));
        }

        Ok(SshChallenge {
            challenge_id: challenge_id.to_owned(),
            nonce: pending.nonce,
            namespace: SSH_CHALLENGE_NAMESPACE.to_owned(),
            expires_at: pending.expires_at,
        })
    }

    /// Number of pending (possibly expired but not yet pruned) challenges.
    pub fn len(&self) -> usize {
        self.state().pending.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

fn prune_expired(state: &mut ChallengeState, now: u64) {
    state.pending.retain(|_, pending| now < pending.expires_at);
}

fn random_token(bytes: usize) -> String {
    let mut buffer = vec![0u8; bytes];
    OsRng.fill_bytes(&mut buffer);
    URL_SAFE_NO_PAD.encode(buffer)
}

/// An SSH public-key authenticator.
///
/// The `authorized_keys` path is read on every verification so revoking a key
/// takes effect immediately.
#[derive(Clone, Debug)]
pub struct SshKeyAuthenticator {
    authorized_keys: PathBuf,
    permissions: PermissionSet,
}

impl SshKeyAuthenticator {
    pub fn new(authorized_keys: impl Into<PathBuf>) -> Self {
        Self {
            authorized_keys: authorized_keys.into(),
            permissions: PermissionSet::all(),
        }
    }

    /// Restrict the permissions granted after a successful signature.
    pub fn with_permissions(mut self, permissions: PermissionSet) -> Self {
        self.permissions = permissions;
        self
    }

    pub fn authorized_keys_path(&self) -> &Path {
        &self.authorized_keys
    }

    /// Parse the configured file. Unreadable files are `auth.ssh_unavailable`.
    pub fn load_authorized_keys(&self) -> Result<AuthorizedKeys> {
        let text = fs::read_to_string(&self.authorized_keys).map_err(|error| {
            ApiError::new(
                codes::SSH_UNAVAILABLE,
                format!("cannot read the SSH authorized_keys file: {error}"),
            )
            .with_remediation("create the file or configure an existing authorized_keys path")
        })?;
        Ok(parse_authorized_keys(&text))
    }

    /// Find the matching authorized entry for a caller-supplied public key.
    pub fn find_authorized_key(&self, public_key: &[u8]) -> Result<Option<AuthorizedKey>> {
        Ok(self.load_authorized_keys()?.matches(public_key).cloned())
    }

    /// Issue a challenge for `principal` in `store`.
    pub fn issue_challenge(
        &self,
        store: &SshChallengeStore,
        principal: &str,
        now: u64,
    ) -> Result<SshChallenge> {
        store.issue(principal, now)
    }

    /// Verify an SSHSIG signature over the challenge nonce and return the
    /// authenticated identity.
    ///
    /// The challenge is consumed before any key or signature check, so a failed
    /// attempt always requires a fresh challenge. The caller's public key must
    /// match an `authorized_keys` entry.
    pub fn verify_signature(
        &self,
        store: &SshChallengeStore,
        challenge_id: &str,
        principal: &str,
        public_key: &[u8],
        signature: &[u8],
        now: u64,
    ) -> Result<AuthenticatedIdentity> {
        let challenge = store.consume(challenge_id, principal, now)?;
        let keys = self.load_authorized_keys()?;
        let key = keys.matches(public_key).ok_or_else(|| {
            ApiError::new(
                codes::SSH_REJECTED,
                "the supplied SSH public key is not authorized for this node",
            )
        })?;

        verify_sshsig(key, challenge.nonce.as_bytes(), signature)?;

        Ok(AuthenticatedIdentity {
            principal: principal.to_owned(),
            permissions: self.permissions.clone(),
        })
    }
}

impl LocalAuthenticator for SshKeyAuthenticator {
    /// SSH keys require a challenge signature and cannot be checked through the
    /// public-key-only trait method. This never synthesizes a success.
    fn authenticate(
        &self,
        _principal: &str,
        credential: Credential<'_>,
    ) -> Result<AuthenticatedIdentity> {
        match credential {
            Credential::SshPublicKey(_) => Err(ApiError::new(
                codes::SSH_CHALLENGE_REQUIRED,
                "SSH authentication requires an SSHSIG signature over a challenge",
            )),
            Credential::Password(_) => Err(ApiError::new(
                codes::SSH_REJECTED,
                "this authenticator does not accept passwords",
            )),
        }
    }
}

fn verify_sshsig(key: &AuthorizedKey, message: &[u8], signature: &[u8]) -> Result<()> {
    let directory = tempfile::Builder::new()
        .prefix("ozma-ssh-verify-")
        .tempdir()
        .map_err(|error| {
            ApiError::new(
                codes::SSH_UNAVAILABLE,
                format!("cannot create a temporary SSH verification directory: {error}"),
            )
        })?;

    let allowed_signers = directory.path().join("allowed_signers");
    let signature_path = directory.path().join("signature.sig");

    let signer_line = format!(
        "{ALLOWED_SIGNER_PRINCIPAL} namespaces=\"{SSH_CHALLENGE_NAMESPACE}\" {} {}\n",
        key.key_type(),
        key.key_base64()
    );
    fs::write(&allowed_signers, signer_line).map_err(|error| {
        ApiError::new(
            codes::SSH_UNAVAILABLE,
            format!("cannot write the temporary allowed-signers file: {error}"),
        )
    })?;
    restrict_permissions(&allowed_signers)?;

    fs::write(&signature_path, signature).map_err(|error| {
        ApiError::new(
            codes::SSH_UNAVAILABLE,
            format!("cannot write the temporary SSH signature file: {error}"),
        )
    })?;
    restrict_permissions(&signature_path)?;

    let mut child = Command::new("ssh-keygen")
        .args(["-Y", "verify", "-f"])
        .arg(&allowed_signers)
        .args([
            "-I",
            ALLOWED_SIGNER_PRINCIPAL,
            "-n",
            SSH_CHALLENGE_NAMESPACE,
            "-s",
        ])
        .arg(&signature_path)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|error| {
            ApiError::new(
                codes::SSH_UNAVAILABLE,
                format!("cannot run ssh-keygen for signature verification: {error}"),
            )
            .with_remediation("install an OpenSSH client with SSHSIG support")
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        stdin.write_all(message).map_err(|error| {
            ApiError::new(
                codes::SSH_UNAVAILABLE,
                format!("cannot send the challenge to ssh-keygen: {error}"),
            )
        })?;
    }

    let output = wait_with_timeout(child, Duration::from_secs(5))?;
    if output.status.success() {
        return Ok(());
    }

    let detail = String::from_utf8_lossy(&output.stderr);
    let detail = detail.trim();
    let detail: String = if detail.chars().count() > 300 {
        detail.chars().take(300).collect::<String>() + "…"
    } else {
        detail.to_owned()
    };
    Err(ApiError::new(
        codes::SSH_REJECTED,
        if detail.is_empty() {
            "SSHSIG verification failed".to_owned()
        } else {
            format!("SSHSIG verification failed: {detail}")
        },
    ))
}

fn wait_with_timeout(mut child: Child, timeout: Duration) -> Result<Output> {
    let started = Instant::now();
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(mut pipe) = child.stdout.take() {
                    let _ = pipe.read_to_end(&mut stdout);
                }
                if let Some(mut pipe) = child.stderr.take() {
                    let _ = pipe.read_to_end(&mut stderr);
                }
                return Ok(Output {
                    status,
                    stdout,
                    stderr,
                });
            }
            Ok(None) => {
                if started.elapsed() >= timeout {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err(ApiError::new(
                        codes::SSH_UNAVAILABLE,
                        "ssh-keygen signature verification timed out",
                    ));
                }
                std::thread::sleep(Duration::from_millis(10));
            }
            Err(error) => {
                return Err(ApiError::new(
                    codes::SSH_UNAVAILABLE,
                    format!("cannot wait for ssh-keygen: {error}"),
                ));
            }
        }
    }
}

#[cfg(unix)]
fn restrict_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    fs::set_permissions(path, fs::Permissions::from_mode(0o600)).map_err(|error| {
        ApiError::new(
            codes::SSH_CONFIG,
            format!("cannot restrict temporary SSH file permissions: {error}"),
        )
    })
}

#[cfg(not(unix))]
fn restrict_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn blob(seed: u8) -> Vec<u8> {
        let mut blob = Vec::new();
        blob.extend_from_slice(&(11u32).to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&(32u32).to_be_bytes());
        blob.extend_from_slice(&[seed; 32]);
        blob
    }

    fn key_line(seed: u8, comment: Option<&str>) -> String {
        let encoded = STANDARD.encode(blob(seed));
        match comment {
            Some(comment) => format!("ssh-ed25519 {encoded} {comment}"),
            None => format!("ssh-ed25519 {encoded}"),
        }
    }

    #[test]
    fn parses_options_comments_and_ignores_malformed_lines() {
        let valid = key_line(1, Some("alice@laptop"));
        let input = format!(
            "\n# a comment\n\
             no-port-forwarding,command=\"echo hello world\",environment=\"A=b c\" {valid}\n\
             ssh-ed25519 not-base64\n\
             ssh-rsa {}\n\
             garbage that is not a key\n",
            STANDARD.encode(blob(2))
        );

        let parsed = parse_authorized_keys(&input);
        assert_eq!(parsed.keys().len(), 1, "only one line is valid");
        assert_eq!(parsed.skipped_lines(), 3);
        assert_eq!(parsed.keys()[0].key_type(), "ssh-ed25519");
        assert_eq!(parsed.keys()[0].comment(), Some("alice@laptop"));
    }

    #[test]
    fn matches_identical_keys_and_rejects_other_keys() {
        let parsed = parse_authorized_keys(&key_line(1, Some("one")));
        let matching = key_line(1, Some("caller comment"));
        assert!(parsed.matches(matching.as_bytes()).is_some());

        let different = key_line(2, None);
        assert!(parsed.matches(different.as_bytes()).is_none());
        assert!(parsed.matches(b"not a key").is_none());
    }

    #[test]
    fn parse_handles_unpadded_base64() {
        let encoded = STANDARD_NO_PAD.encode(blob(3));
        let parsed = parse_authorized_keys(&format!("ssh-ed25519 {encoded}"));
        assert_eq!(parsed.keys().len(), 1);
    }

    #[test]
    fn challenges_expire_and_are_single_use() {
        let store = SshChallengeStore::with_limits(4, 60);
        let challenge = store.issue("alice", 100).unwrap();
        assert_eq!(challenge.namespace, SSH_CHALLENGE_NAMESPACE);
        assert_eq!(challenge.expires_at, 160);
        assert!(challenge.nonce.len() >= 40);
        assert_eq!(store.len(), 1);

        let consumed = store
            .consume(&challenge.challenge_id, "alice", 110)
            .unwrap();
        assert_eq!(consumed.nonce, challenge.nonce);
        assert!(store.is_empty());

        let replay = store
            .consume(&challenge.challenge_id, "alice", 110)
            .unwrap_err();
        assert_eq!(replay.code, codes::SSH_CHALLENGE_INVALID);
    }

    #[test]
    fn expired_challenges_report_expiry_and_are_removed() {
        let store = SshChallengeStore::with_limits(4, 60);
        let challenge = store.issue("alice", 100).unwrap();
        let error = store
            .consume(&challenge.challenge_id, "alice", 160)
            .unwrap_err();
        assert_eq!(error.code, codes::SSH_CHALLENGE_EXPIRED);
        assert!(error.retryable);
        assert!(store.is_empty());
    }

    #[test]
    fn challenges_are_bound_to_the_principal() {
        let store = SshChallengeStore::with_limits(4, 60);
        let challenge = store.issue("alice", 100).unwrap();
        let error = store
            .consume(&challenge.challenge_id, "bob", 110)
            .unwrap_err();
        assert_eq!(error.code, codes::SSH_CHALLENGE_INVALID);
        assert!(
            store.is_empty(),
            "a mismatched attempt still consumes the id"
        );
    }

    #[test]
    fn pending_challenges_are_bounded() {
        let store = SshChallengeStore::with_limits(1, 60);
        store.issue("alice", 100).unwrap();
        let error = store.issue("alice", 101).unwrap_err();
        assert_eq!(error.code, codes::SSH_CHALLENGE_INVALID);
        assert!(error.retryable);
    }

    #[test]
    fn empty_principals_are_rejected() {
        let store = SshChallengeStore::new();
        assert_eq!(
            store.issue("", 100).unwrap_err().code,
            codes::SSH_CHALLENGE_INVALID
        );
        assert_eq!(
            store.issue("alice\nroot", 100).unwrap_err().code,
            codes::SSH_CHALLENGE_INVALID
        );
    }

    #[test]
    fn missing_authorized_keys_is_unavailable() {
        let auth = SshKeyAuthenticator::new("/nonexistent/ozma/authorized_keys");
        assert_eq!(
            auth.load_authorized_keys().unwrap_err().code,
            codes::SSH_UNAVAILABLE
        );
    }

    #[test]
    fn local_authenticator_never_treats_a_bare_key_as_success() {
        let auth = SshKeyAuthenticator::new("/nonexistent/ozma/authorized_keys");
        let error = auth
            .authenticate(
                "alice",
                Credential::SshPublicKey(key_line(1, None).as_bytes()),
            )
            .unwrap_err();
        assert_eq!(error.code, codes::SSH_CHALLENGE_REQUIRED);

        let error = auth
            .authenticate("alice", Credential::Password("secret"))
            .unwrap_err();
        assert_eq!(error.code, codes::SSH_REJECTED);
    }

    #[test]
    fn malformed_authorized_keys_yields_no_match() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("authorized_keys");
        fs::write(&path, "this is not a key\nssh-ed25519 !!!\n").unwrap();

        let auth = SshKeyAuthenticator::new(&path);
        let parsed = auth.load_authorized_keys().unwrap();
        assert!(parsed.is_empty());
        assert_eq!(parsed.skipped_lines(), 2);
        assert!(auth
            .find_authorized_key(key_line(1, None).as_bytes())
            .unwrap()
            .is_none());
    }

    /// Real OpenSSH round-trip. Skipped (with a message) only when the host has
    /// no usable `ssh-keygen`.
    #[test]
    fn real_sshsig_verification_accepts_the_authorized_key() {
        let directory = tempfile::tempdir().unwrap();
        let Some((key_path, public_key)) = generate_key_in(directory.path(), "id") else {
            eprintln!("skipping real SSHSIG test; ssh-keygen is unavailable");
            return;
        };

        let authorized_keys = directory.path().join("authorized_keys");
        fs::write(&authorized_keys, &public_key).unwrap();
        let auth = SshKeyAuthenticator::new(&authorized_keys);
        let store = SshChallengeStore::with_limits(8, 60);

        let challenge = auth.issue_challenge(&store, "alice", 1_000).unwrap();
        let signature = sign_challenge(directory.path(), &key_path, &challenge.nonce);
        let identity = auth
            .verify_signature(
                &store,
                &challenge.challenge_id,
                "alice",
                public_key.as_bytes(),
                &signature,
                1_001,
            )
            .unwrap();
        assert_eq!(identity.principal, "alice");
        assert!(identity
            .permissions
            .contains(ozma_protocol::Permission::Admin));

        // Replaying the challenge must fail even with a valid signature.
        let replay = auth
            .verify_signature(
                &store,
                &challenge.challenge_id,
                "alice",
                public_key.as_bytes(),
                &signature,
                1_001,
            )
            .unwrap_err();
        assert_eq!(replay.code, codes::SSH_CHALLENGE_INVALID);
    }

    #[test]
    fn real_sshsig_verification_rejects_a_wrong_key() {
        let directory = tempfile::tempdir().unwrap();
        let Some((_authorized_key_path, authorized_public)) =
            generate_key_in(directory.path(), "authorized")
        else {
            eprintln!("skipping real SSHSIG wrong-key test; ssh-keygen is unavailable");
            return;
        };
        let Some((other_key_path, other_public)) = generate_key_in(directory.path(), "other")
        else {
            eprintln!("skipping real SSHSIG wrong-key test; ssh-keygen is unavailable");
            return;
        };

        let authorized_keys = directory.path().join("authorized_keys");
        fs::write(&authorized_keys, &authorized_public).unwrap();
        let auth = SshKeyAuthenticator::new(&authorized_keys);
        let store = SshChallengeStore::with_limits(8, 60);

        // An unknown key is rejected before signature verification.
        let challenge = auth.issue_challenge(&store, "alice", 1_000).unwrap();
        let other_signature = sign_challenge(directory.path(), &other_key_path, &challenge.nonce);
        let error = auth
            .verify_signature(
                &store,
                &challenge.challenge_id,
                "alice",
                other_public.as_bytes(),
                &other_signature,
                1_001,
            )
            .unwrap_err();
        assert_eq!(error.code, codes::SSH_REJECTED);

        // A key that is authorized, but signed by a different private key,
        // fails signature verification.
        let challenge = auth.issue_challenge(&store, "alice", 1_000).unwrap();
        let other_signature = sign_challenge(directory.path(), &other_key_path, &challenge.nonce);
        let error = auth
            .verify_signature(
                &store,
                &challenge.challenge_id,
                "alice",
                authorized_public.as_bytes(),
                &other_signature,
                1_001,
            )
            .unwrap_err();
        assert_eq!(error.code, codes::SSH_REJECTED);
    }

    /// Generate an Ed25519 key pair inside an existing temporary directory.
    /// Returns `None` when the host has no usable `ssh-keygen`.
    fn generate_key_in(directory: &Path, name: &str) -> Option<(PathBuf, String)> {
        let key_path = directory.join(format!("{name}_ed25519"));
        let status = Command::new("ssh-keygen")
            .args(["-q", "-t", "ed25519", "-N", "", "-f"])
            .arg(&key_path)
            .status()
            .ok()?;
        if !status.success() {
            return None;
        }
        let public_key = fs::read_to_string(key_path.with_extension("pub")).ok()?;
        Some((key_path, public_key))
    }

    fn sign_challenge(directory: &Path, key_path: &Path, nonce: &str) -> Vec<u8> {
        let message_path = directory.join(format!("message-{}", random_token(8)));
        let mut message = fs::File::create(&message_path).unwrap();
        message.write_all(nonce.as_bytes()).unwrap();
        drop(message);

        let status = Command::new("ssh-keygen")
            .args(["-Y", "sign", "-f"])
            .arg(key_path)
            .args(["-n", SSH_CHALLENGE_NAMESPACE])
            .arg(&message_path)
            .status()
            .unwrap();
        assert!(status.success(), "ssh-keygen -Y sign failed");

        let signature_path = PathBuf::from(format!("{}.sig", message_path.display()));
        fs::read(&signature_path).unwrap()
    }
}
