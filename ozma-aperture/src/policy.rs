// SPDX-License-Identifier: AGPL-3.0-only
//! Versioned JSON policy documents.

use serde::{Deserialize, Serialize};

use crate::dimension::Aperture;
use crate::error::ApertureError;
use crate::evaluate::{evaluate_with_policy_version, Evaluation};
use crate::frame::Frame;

/// A policy manifest: the endpoint's maximum aperture plus the frames that are
/// in force for a profile.
///
/// This type performs no filesystem I/O. Callers read a file or receive a
/// document over the API and pass its text to [`PolicyDocument::from_json`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyDocument {
    /// The endpoint's maximum aperture.
    pub physiology: Aperture,
    /// Frames imposed by device/bus/policy (`forced`), the requester
    /// (`chosen`), or a multi-party proposal (`negotiated`).
    #[serde(default)]
    pub frames: Vec<Frame>,
    /// Stable policy/profile version included in every receipt.
    pub policy_version: String,
}

impl PolicyDocument {
    /// Parse a policy document from JSON.
    ///
    /// Unknown dimensions are preserved. Unknown enum tokens are preserved and
    /// fail closed if they ever conflict with a different value.
    pub fn from_json(json: &str) -> Result<Self, ApertureError> {
        serde_json::from_str(json).map_err(|error| ApertureError::InvalidJson {
            message: error.to_string(),
        })
    }

    /// Serialize this document as pretty JSON.
    pub fn to_json(&self) -> Result<String, ApertureError> {
        serde_json::to_string_pretty(self).map_err(|error| ApertureError::InvalidJson {
            message: error.to_string(),
        })
    }

    /// Evaluate a request against this document's physiology and frames.
    pub fn evaluate(&self, request: &Aperture) -> Result<Evaluation, ApertureError> {
        evaluate_with_policy_version(
            &self.physiology,
            &self.frames,
            request,
            &self.policy_version,
        )
    }
}
