// SPDX-License-Identifier: AGPL-3.0-only
//! Extension point for local OS authentication.
//!
//! PAM and SSH-key verification are deliberately **not** implemented in this
//! pass. Platform adapters (M2) should implement [`LocalAuthenticator`] in
//! their own crate behind an optional feature so the default build does not
//! link PAM or a native crypto library.
//!
//! Nothing here fakes an authentication result: a node without a configured
//! backend simply has no authenticator.

use std::fmt;

use ozma_protocol::{PermissionSet, Result};

/// A credential supplied by a local operator.
///
/// The password variant redacts itself in `Debug` so an accidental log line
/// cannot leak it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Credential<'a> {
    /// A password or passphrase for a local account.
    Password(&'a str),
    /// An OpenSSH public key line, used only when `authorized_keys` policy is
    /// configured.
    SshPublicKey(&'a [u8]),
}

impl fmt::Debug for Credential<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Password(_) => f.write_str("Credential::Password([redacted])"),
            Self::SshPublicKey(key) => f
                .debug_tuple("Credential::SshPublicKey")
                .field(&format_args!("{} bytes", key.len()))
                .finish(),
        }
    }
}

/// The identity and permissions established by a successful local check.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AuthenticatedIdentity {
    pub principal: String,
    pub permissions: PermissionSet,
}

/// Platform authentication backend.
///
/// Implementations must be cheap to share across the session manager and must
/// not hold a lock while blocking. A future PAM adapter can use a dedicated
/// worker; an SSH-key adapter can parse `authorized_keys` from a configured
/// file. Neither belongs in this crate.
pub trait LocalAuthenticator: Send + Sync {
    /// Verify `credential` for `principal`.
    ///
    /// Return `unauthorized` for a bad credential and `unsupported` when the
    /// backend is not available on this platform. Never return a synthetic
    /// success.
    fn authenticate(
        &self,
        principal: &str,
        credential: Credential<'_>,
    ) -> Result<AuthenticatedIdentity>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn password_debug_is_redacted() {
        let credential = Credential::Password("hunter2");
        let debug = format!("{credential:?}");
        assert!(!debug.contains("hunter2"));
        assert!(debug.contains("redacted"));
    }

    #[test]
    fn ssh_key_debug_reports_only_length() {
        let credential = Credential::SshPublicKey(b"ssh-ed25519 AAAA");
        let debug = format!("{credential:?}");
        assert!(!debug.contains("ssh-ed25519"));
        assert!(debug.contains("16 bytes"));
    }
}
