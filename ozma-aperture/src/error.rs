// SPDX-License-Identifier: AGPL-3.0-only
//! Typed aperture errors.

use crate::dimension::Dimension;

/// A typed aperture failure with stable display text.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ApertureError {
    /// The effective aperture has no admissible value on one or more
    /// dimensions. Evaluation never panics on an empty intersection.
    #[error("aperture.empty: conflicting dimensions: {conflicting:?}")]
    Empty { conflicting: Vec<String> },

    /// The policy document was not valid JSON or did not match the schema.
    #[error("aperture.invalid_json: {message}")]
    InvalidJson { message: String },

    /// A policy or request value had the wrong JSON type or representation.
    #[error("aperture.invalid_value: dimension `{dimension}`: {message}")]
    InvalidValue { dimension: String, message: String },
}

impl ApertureError {
    /// Build an [`ApertureError::InvalidValue`] for a dimension.
    pub fn invalid_value(dimension: &Dimension, message: impl Into<String>) -> Self {
        ApertureError::InvalidValue {
            dimension: dimension.to_string(),
            message: message.into(),
        }
    }
}
