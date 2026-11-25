// SPDX-License-Identifier: AGPL-3.0-only WITH OzmaPluginException
//! Font metadata, shape/intent matching, and target export planning.

#![forbid(unsafe_code)]

pub mod intent;
pub mod matching;
pub mod metadata;

pub use intent::*;
pub use matching::*;
pub use metadata::*;
