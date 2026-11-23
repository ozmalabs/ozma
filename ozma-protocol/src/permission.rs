// SPDX-License-Identifier: AGPL-3.0-only
//! Scoped permissions carried by a session and by client grants.

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::fmt;

/// A capability a client may hold for a session or device.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Permission {
    /// Read-only view of a stream or display.
    View,
    /// Send keyboard, pointer, gamepad, touch, or pen input.
    Control,
    /// Receive audio.
    Audio,
    /// Send microphone audio.
    Mic,
    /// Read the host clipboard.
    ClipboardRead,
    /// Write the host clipboard.
    ClipboardWrite,
    /// Move files in either direction.
    FileTransfer,
    /// Attach or detach USB devices.
    Usb,
    /// Wake, reset, or power off the host/target.
    Power,
    /// Run configured session commands.
    Commands,
    /// Attach a remote display/virtual monitor.
    DisplayAttach,
    /// Administrative operations, including terminating other sessions.
    Admin,
}

impl Permission {
    pub const ALL: [Permission; 12] = [
        Permission::View,
        Permission::Control,
        Permission::Audio,
        Permission::Mic,
        Permission::ClipboardRead,
        Permission::ClipboardWrite,
        Permission::FileTransfer,
        Permission::Usb,
        Permission::Power,
        Permission::Commands,
        Permission::DisplayAttach,
        Permission::Admin,
    ];

    /// Permissions that are safe to grant for a view-only session.
    pub const VIEW_ONLY: [Permission; 2] = [Permission::View, Permission::Audio];
}

impl fmt::Display for Permission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let value = match self {
            Permission::View => "view",
            Permission::Control => "control",
            Permission::Audio => "audio",
            Permission::Mic => "mic",
            Permission::ClipboardRead => "clipboard_read",
            Permission::ClipboardWrite => "clipboard_write",
            Permission::FileTransfer => "file_transfer",
            Permission::Usb => "usb",
            Permission::Power => "power",
            Permission::Commands => "commands",
            Permission::DisplayAttach => "display_attach",
            Permission::Admin => "admin",
        };
        f.write_str(value)
    }
}

/// A set of permissions. Serialized as a JSON array of permission names.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(transparent)]
pub struct PermissionSet(pub BTreeSet<Permission>);

impl PermissionSet {
    pub fn empty() -> Self {
        Self::default()
    }

    pub fn all() -> Self {
        Self(Permission::ALL.into_iter().collect())
    }

    pub fn view_only() -> Self {
        Self(Permission::VIEW_ONLY.into_iter().collect())
    }

    pub fn insert(&mut self, permission: Permission) {
        self.0.insert(permission);
    }

    pub fn remove(&mut self, permission: Permission) -> bool {
        self.0.remove(&permission)
    }

    pub fn contains(&self, permission: Permission) -> bool {
        self.0.contains(&permission)
    }

    pub fn is_empty(&self) -> bool {
        self.0.is_empty()
    }

    pub fn len(&self) -> usize {
        self.0.len()
    }

    pub fn iter(&self) -> impl Iterator<Item = Permission> + '_ {
        self.0.iter().copied()
    }

    pub fn union(&self, other: &Self) -> Self {
        Self(self.0.union(&other.0).copied().collect())
    }

    pub fn intersection(&self, other: &Self) -> Self {
        Self(self.0.intersection(&other.0).copied().collect())
    }

    /// True when every permission in `self` is present in `granted`.
    pub fn is_subset_of(&self, granted: &Self) -> bool {
        self.0.is_subset(&granted.0)
    }

    /// Permissions required by a session that are missing from a grant.
    pub fn missing_from(&self, granted: &Self) -> Self {
        Self(self.0.difference(&granted.0).copied().collect())
    }
}

impl FromIterator<Permission> for PermissionSet {
    fn from_iter<T: IntoIterator<Item = Permission>>(iter: T) -> Self {
        Self(iter.into_iter().collect())
    }
}

impl IntoIterator for PermissionSet {
    type Item = Permission;
    type IntoIter = std::collections::btree_set::IntoIter<Permission>;

    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn subset_and_missing() {
        let granted = PermissionSet::from_iter([Permission::View, Permission::Control]);
        let required = PermissionSet::from_iter([Permission::View, Permission::Control, Permission::Audio]);
        assert!(!required.is_subset_of(&granted));
        assert_eq!(
            required.missing_from(&granted),
            PermissionSet::from_iter([Permission::Audio])
        );
    }

    #[test]
    fn serializes_as_an_array() {
        let set = PermissionSet::from_iter([Permission::View, Permission::Control]);
        let json = serde_json::to_value(&set).unwrap();
        assert!(json.is_array());
    }
}
