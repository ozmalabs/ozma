// SPDX-License-Identifier: AGPL-3.0-only
//! Stable identifiers used by every Ozma component.
//!
//! They are transparent strings on the wire so that a controller can address a
//! node without knowing its Rust representation, and so that persisted state
//! stays readable.

use serde::{Deserialize, Serialize};
use std::fmt;

macro_rules! id_type {
    ($name:ident, $doc:literal) => {
        #[doc = $doc]
        #[derive(Clone, Debug, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
        #[serde(transparent)]
        pub struct $name(pub String);

        impl $name {
            /// Construct an id from anything string-like.
            pub fn new(value: impl Into<String>) -> Self {
                Self(value.into())
            }

            /// Borrow the underlying string.
            pub fn as_str(&self) -> &str {
                &self.0
            }

            /// True when the id is empty. Empty ids are never valid on the wire.
            pub fn is_empty(&self) -> bool {
                self.0.is_empty()
            }
        }

        impl From<&str> for $name {
            fn from(value: &str) -> Self {
                Self(value.to_owned())
            }
        }

        impl From<String> for $name {
            fn from(value: String) -> Self {
                Self(value)
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(&self.0)
            }
        }
    };
}

id_type!(NodeId, "A stable node identity, normally the mDNS instance name.");
id_type!(SessionId, "A controller-issued session identity.");
id_type!(ClientId, "A paired client identity.");
id_type!(TrackId, "A media track inside a session.");
id_type!(DisplayId, "A virtual or physical display identity.");

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_transparent_strings() {
        let id = NodeId::new("node-a");
        assert_eq!(serde_json::to_string(&id).unwrap(), "\"node-a\"");
        let back: NodeId = serde_json::from_str("\"node-a\"").unwrap();
        assert_eq!(back, id);
    }
}
