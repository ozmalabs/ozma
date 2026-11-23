// SPDX-License-Identifier: AGPL-3.0-only
//! Client grants: the long-lived permissions a paired client may use.
//!
//! A [`Grant`] is not a token. It records what a client is allowed to ask for;
//! a session token is then issued for the narrower subset a session actually
//! uses. Grants are revocable and may expire.

use std::collections::HashMap;
use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};

use ozma_protocol::codes;
use ozma_protocol::{ApiError, ClientId, PermissionSet, Result};

/// A per-client permission grant.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Grant {
    pub client_id: ClientId,
    pub permissions: PermissionSet,
    /// When the grant was created, seconds since the Unix epoch.
    pub issued_at: u64,
    /// Optional expiry. `None` means the grant does not expire on its own.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<u64>,
    /// Revoked grants remain in the store for audit but never validate.
    #[serde(default)]
    pub revoked: bool,
    /// Last successful validation, useful for audit and pruning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_used_at: Option<u64>,
    /// Human-readable client name for audit/UX.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
}

/// Alias used in APIs that talk about client grants explicitly.
pub type ClientGrant = Grant;

impl Grant {
    pub fn new(client_id: impl Into<ClientId>, permissions: PermissionSet, issued_at: u64) -> Self {
        Self {
            client_id: client_id.into(),
            permissions,
            issued_at,
            expires_at: None,
            revoked: false,
            last_used_at: None,
            label: None,
        }
    }

    pub fn with_expiry(mut self, expires_at: u64) -> Self {
        self.expires_at = Some(expires_at);
        self
    }

    pub fn with_label(mut self, label: impl Into<String>) -> Self {
        self.label = Some(label.into());
        self
    }

    pub fn is_expired(&self, now: u64) -> bool {
        matches!(self.expires_at, Some(expires_at) if now >= expires_at)
    }

    /// True when the grant is neither revoked nor expired.
    pub fn is_active(&self, now: u64) -> bool {
        !self.revoked && !self.is_expired(now)
    }

    /// Permissions in `required` that this grant does not contain.
    pub fn missing_permissions(&self, required: &PermissionSet) -> PermissionSet {
        required.missing_from(&self.permissions)
    }

    /// Whether this grant covers every permission in `required`, ignoring
    /// revocation and expiry.
    pub fn covers(&self, required: &PermissionSet) -> bool {
        required.is_subset_of(&self.permissions)
    }
}

/// In-memory grant storage. No persistence in this pass; the controller or an
/// eventual database adapter owns durable state.
#[derive(Debug, Default)]
pub struct GrantStore {
    grants: Mutex<HashMap<ClientId, Grant>>,
}

impl GrantStore {
    pub fn new() -> Self {
        Self::default()
    }

    fn state(&self) -> MutexGuard<'_, HashMap<ClientId, Grant>> {
        self.grants
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    /// Insert or replace a grant.
    pub fn insert(&self, grant: Grant) -> Result<()> {
        if grant.client_id.is_empty() {
            return Err(ApiError::invalid("a grant requires a non-empty client_id"));
        }
        self.state().insert(grant.client_id.clone(), grant);
        Ok(())
    }

    /// Look up a grant, including revoked/expired ones for audit.
    pub fn get(&self, client_id: &ClientId) -> Option<Grant> {
        self.state().get(client_id).cloned()
    }

    /// Remove a grant entirely. Prefer [`GrantStore::revoke`] when audit
    /// matters.
    pub fn remove(&self, client_id: &ClientId) -> Option<Grant> {
        self.state().remove(client_id)
    }

    /// Mark a grant revoked. Returns false when the client is unknown.
    pub fn revoke(&self, client_id: &ClientId) -> bool {
        let mut grants = self.state();
        match grants.get_mut(client_id) {
            Some(grant) => {
                grant.revoked = true;
                true
            }
            None => false,
        }
    }

    /// Validate a client against a required permission set at `now`.
    ///
    /// On success, records `last_used_at` and returns the updated grant.
    pub fn validate(
        &self,
        client_id: &ClientId,
        required: &PermissionSet,
        now: u64,
    ) -> Result<Grant> {
        let mut grants = self.state();
        let grant = grants.get_mut(client_id).ok_or_else(|| {
            ApiError::new(
                codes::UNAUTHORIZED,
                "No permission grant exists for this client",
            )
            .with_details(serde_json::json!({ "client_id": client_id }))
        })?;

        if grant.revoked {
            return Err(ApiError::new(
                codes::FORBIDDEN,
                "The client's permission grant was revoked",
            )
            .with_details(serde_json::json!({ "client_id": client_id })));
        }

        if grant.is_expired(now) {
            return Err(ApiError::new(
                codes::TOKEN_EXPIRED,
                "The client's permission grant has expired",
            )
            .with_details(serde_json::json!({
                "client_id": client_id,
                "expires_at": grant.expires_at,
                "now": now,
            }))
            .retryable());
        }

        let missing = grant.missing_permissions(required);
        if !missing.is_empty() {
            let missing: Vec<String> = missing
                .iter()
                .map(|permission| permission.to_string())
                .collect();
            return Err(ApiError::new(
                codes::FORBIDDEN,
                "The client's grant is missing permissions",
            )
            .with_details(serde_json::json!({
                "client_id": client_id,
                "missing": missing,
            })));
        }

        grant.last_used_at = Some(now);
        Ok(grant.clone())
    }

    /// All grants, sorted by client id for deterministic output.
    pub fn list(&self) -> Vec<Grant> {
        let mut grants: Vec<Grant> = self.state().values().cloned().collect();
        grants.sort_by(|a, b| a.client_id.cmp(&b.client_id));
        grants
    }

    /// Number of stored grants, including revoked/expired entries.
    pub fn len(&self) -> usize {
        self.state().len()
    }

    pub fn is_empty(&self) -> bool {
        self.state().is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ozma_protocol::Permission;

    fn permissions() -> PermissionSet {
        [Permission::View, Permission::Control, Permission::Audio]
            .into_iter()
            .collect()
    }

    fn client() -> ClientId {
        ClientId::new("client-1")
    }

    #[test]
    fn inserts_gets_and_lists_grants() {
        let store = GrantStore::new();
        let grant = Grant::new(client(), permissions(), 100).with_label("Laptop");
        store.insert(grant.clone()).unwrap();

        assert_eq!(store.get(&client()), Some(grant));
        assert_eq!(store.len(), 1);
        assert!(!store.is_empty());
        assert_eq!(store.list()[0].label.as_deref(), Some("Laptop"));
    }

    #[test]
    fn rejects_an_empty_client_id() {
        let store = GrantStore::new();
        let error = store
            .insert(Grant::new("", permissions(), 100))
            .unwrap_err();
        assert_eq!(error.code, codes::INVALID_REQUEST);
    }

    #[test]
    fn validates_and_records_last_use() {
        let store = GrantStore::new();
        store
            .insert(Grant::new(client(), permissions(), 100))
            .unwrap();

        let used = store.validate(&client(), &permissions(), 200).unwrap();
        assert_eq!(used.last_used_at, Some(200));
        assert_eq!(store.get(&client()).unwrap().last_used_at, Some(200));
    }

    #[test]
    fn a_subset_request_is_accepted() {
        let store = GrantStore::new();
        store
            .insert(Grant::new(client(), permissions(), 100))
            .unwrap();
        let required: PermissionSet = [Permission::View].into_iter().collect();
        assert!(store.validate(&client(), &required, 200).is_ok());
    }

    #[test]
    fn unknown_client_is_unauthorized() {
        let store = GrantStore::new();
        let error = store.validate(&client(), &permissions(), 200).unwrap_err();
        assert_eq!(error.code, codes::UNAUTHORIZED);
    }

    #[test]
    fn revoked_grant_is_forbidden() {
        let store = GrantStore::new();
        store
            .insert(Grant::new(client(), permissions(), 100))
            .unwrap();
        assert!(store.revoke(&client()));
        assert!(store.get(&client()).unwrap().revoked);

        let error = store.validate(&client(), &permissions(), 200).unwrap_err();
        assert_eq!(error.code, codes::FORBIDDEN);
        assert_eq!(store.get(&client()).unwrap().last_used_at, None);
    }

    #[test]
    fn revoking_an_unknown_client_returns_false() {
        let store = GrantStore::new();
        assert!(!store.revoke(&client()));
    }

    #[test]
    fn expired_grant_is_rejected() {
        let store = GrantStore::new();
        store
            .insert(Grant::new(client(), permissions(), 100).with_expiry(300))
            .unwrap();

        assert!(store.validate(&client(), &permissions(), 299).is_ok());
        let error = store.validate(&client(), &permissions(), 300).unwrap_err();
        assert_eq!(error.code, codes::TOKEN_EXPIRED);
        assert!(error.retryable);
    }

    #[test]
    fn missing_permissions_are_reported() {
        let store = GrantStore::new();
        let granted: PermissionSet = [Permission::View].into_iter().collect();
        store.insert(Grant::new(client(), granted, 100)).unwrap();

        let required: PermissionSet = [Permission::View, Permission::Control]
            .into_iter()
            .collect();
        let error = store.validate(&client(), &required, 200).unwrap_err();
        assert_eq!(error.code, codes::FORBIDDEN);
        assert_eq!(error.details["missing"][0], "control");
    }

    #[test]
    fn remove_and_audit_flags_work() {
        let store = GrantStore::new();
        let grant = Grant::new(client(), permissions(), 100);
        store.insert(grant.clone()).unwrap();
        assert_eq!(store.remove(&client()), Some(grant));
        assert!(store.is_empty());
    }

    #[test]
    fn grant_helpers_agree_with_store() {
        let grant = Grant::new(client(), permissions(), 100).with_expiry(300);
        assert!(!grant.is_expired(299));
        assert!(grant.is_expired(300));
        assert!(grant.is_active(299));
        assert!(!grant.is_active(300));

        let required: PermissionSet = [Permission::View, Permission::Control, Permission::Mic]
            .into_iter()
            .collect();
        let missing = grant.missing_permissions(&required);
        assert_eq!(missing.len(), 1);
        assert!(missing.contains(Permission::Mic));
        assert!(!grant.covers(&required));
    }
}
