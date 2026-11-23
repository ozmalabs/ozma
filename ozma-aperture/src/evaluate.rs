// SPDX-License-Identifier: AGPL-3.0-only
//! Evaluation, residuals, limiting frames, and receipts.

use serde::{Deserialize, Serialize};

use crate::dimension::{Aperture, Dimension, Value};
use crate::error::ApertureError;
use crate::frame::{Frame, Provenance, Recourse};

/// Policy version used when [`evaluate`] is called without a policy document.
pub const UNVERSIONED_POLICY: &str = "unversioned";

/// The frame that narrowed a requested dimension.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct LimitingFrame {
    pub dimension: Dimension,
    pub frame: String,
    pub provenance: Provenance,
}

/// One requested dimension that the effective aperture does not admit.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ResidualEntry {
    pub dimension: Dimension,
    pub requested: Value,
    pub allowed: Value,
}

impl<'de> Deserialize<'de> for ResidualEntry {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        #[derive(Deserialize)]
        struct Raw {
            dimension: Dimension,
            requested: serde_json::Value,
            allowed: serde_json::Value,
        }

        let raw = Raw::deserialize(deserializer)?;
        let requested =
            Value::from_json(&raw.dimension, &raw.requested).map_err(serde::de::Error::custom)?;
        let allowed =
            Value::from_json(&raw.dimension, &raw.allowed).map_err(serde::de::Error::custom)?;

        Ok(ResidualEntry {
            dimension: raw.dimension,
            requested,
            allowed,
        })
    }
}

/// The evidence attached to every evaluation.
///
/// `recourse` is absent for an admitted evaluation, because there is no
/// residual to act on. For a residual, the top-level recourse is the strongest
/// recourse among the limiting frames (`escalate` > `propose` > `relax`).
/// `residual` and `limiting` may contain one entry per requested dimension that
/// exceeds the effective aperture. `input_hash` is a stable FNV-1a 64-bit hash
/// of the canonical JSON of the physiology, frames, request, and policy
/// version.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Receipt {
    pub requested: Aperture,
    pub effective: Aperture,
    pub limiting: Vec<LimitingFrame>,
    pub residual: Vec<ResidualEntry>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub recourse: Option<Recourse>,
    pub policy_version: String,
    pub input_hash: String,
}

/// A successful evaluation with no residual.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Admission {
    pub effective: Aperture,
    pub receipt: Receipt,
}

/// A successful evaluation whose request exceeds the effective aperture.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Residual {
    pub requested: Aperture,
    pub effective: Aperture,
    pub limiting: Vec<LimitingFrame>,
    pub recourse: Recourse,
    pub receipt: Receipt,
}

/// The result of folding physiology and frames against a request.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Evaluation {
    Admitted(Admission),
    Residual(Residual),
}

/// Evaluate a request against physiology and the frames currently in force.
///
/// The effective aperture is the meet of physiology and every frame. If that
/// meet is empty, this returns [`ApertureError::Empty`] with the conflicting
/// dimensions; it never panics. The receipt uses the
/// [`UNVERSIONED_POLICY`] version because no policy document was supplied.
pub fn evaluate(
    physiology: &Aperture,
    frames: &[Frame],
    request: &Aperture,
) -> Result<Evaluation, ApertureError> {
    evaluate_with_policy_version(physiology, frames, request, UNVERSIONED_POLICY)
}

/// Evaluate with an explicit policy version for the receipt.
pub fn evaluate_with_policy_version(
    physiology: &Aperture,
    frames: &[Frame],
    request: &Aperture,
    policy_version: &str,
) -> Result<Evaluation, ApertureError> {
    let effective = frames.iter().fold(physiology.clone(), |effective, frame| {
        effective.meet(&frame.aperture)
    });

    if effective.is_empty() {
        return Err(ApertureError::Empty {
            conflicting: effective.conflicts(),
        });
    }

    let input_hash = input_hash(physiology, frames, request, policy_version);
    let mut limiting = Vec::new();
    let mut residual = Vec::new();

    for (dimension, requested) in request.iter() {
        if effective.allows(dimension, requested) {
            continue;
        }

        let allowed = effective.get(dimension).cloned().unwrap_or(Value::Bottom);
        residual.push(ResidualEntry {
            dimension: dimension.clone(),
            requested: requested.clone(),
            allowed,
        });
        limiting.push(find_limiting_frame(
            dimension, requested, &effective, physiology, frames,
        ));
    }

    if residual.is_empty() {
        let receipt = Receipt {
            requested: request.clone(),
            effective: effective.clone(),
            limiting,
            residual,
            recourse: None,
            policy_version: policy_version.to_string(),
            input_hash,
        };
        return Ok(Evaluation::Admitted(Admission { effective, receipt }));
    }

    let recourse = dominant_recourse(
        limiting
            .iter()
            .map(|limiting| limiting.provenance.recourse()),
    );
    let receipt = Receipt {
        requested: request.clone(),
        effective: effective.clone(),
        limiting: limiting.clone(),
        residual,
        recourse: Some(recourse),
        policy_version: policy_version.to_string(),
        input_hash,
    };

    Ok(Evaluation::Residual(Residual {
        requested: request.clone(),
        effective,
        limiting,
        recourse,
        receipt,
    }))
}

fn find_limiting_frame(
    dimension: &Dimension,
    requested: &Value,
    effective: &Aperture,
    physiology: &Aperture,
    frames: &[Frame],
) -> LimitingFrame {
    // If the device's own physiology cannot admit the request, no frame change
    // can help. Device capability is a forced limit from the requester's view.
    if let Some(device_value) = physiology.get(dimension) {
        if !device_value.allows(dimension, requested) {
            return LimitingFrame {
                dimension: dimension.clone(),
                frame: "physiology".to_string(),
                provenance: Provenance::Forced,
            };
        }
    }

    let mut explaining: Vec<&Frame> = frames
        .iter()
        .filter(|frame| {
            frame
                .aperture
                .get(dimension)
                .is_some_and(|value| !value.allows(dimension, requested))
        })
        .collect();

    // Prefer the strongest provenance, then a frame whose value is exactly the
    // effective value, then a stable id. This keeps receipts deterministic and
    // never understates escalation when a forced frame also binds.
    let effective_value = effective.get(dimension);
    explaining.sort_by(|left, right| {
        provenance_priority(right.provenance)
            .cmp(&provenance_priority(left.provenance))
            .then_with(|| {
                let left_exact = left.aperture.get(dimension) == effective_value;
                let right_exact = right.aperture.get(dimension) == effective_value;
                right_exact.cmp(&left_exact)
            })
            .then_with(|| left.id.cmp(&right.id))
    });

    if let Some(frame) = explaining.first() {
        return LimitingFrame {
            dimension: dimension.clone(),
            frame: frame.id.clone(),
            provenance: frame.provenance,
        };
    }

    LimitingFrame {
        dimension: dimension.clone(),
        frame: "physiology".to_string(),
        provenance: Provenance::Forced,
    }
}

fn provenance_priority(provenance: Provenance) -> u8 {
    match provenance {
        Provenance::Chosen => 0,
        Provenance::Negotiated => 1,
        Provenance::Forced => 2,
    }
}

fn dominant_recourse(recourses: impl Iterator<Item = Recourse>) -> Recourse {
    let mut dominant = Recourse::Relax;
    for recourse in recourses {
        dominant = match (dominant, recourse) {
            (Recourse::Escalate, _) | (_, Recourse::Escalate) => Recourse::Escalate,
            (Recourse::Propose, _) | (_, Recourse::Propose) => Recourse::Propose,
            _ => Recourse::Relax,
        };
    }
    dominant
}

#[derive(Serialize)]
struct InputHashPayload<'a> {
    physiology: &'a Aperture,
    frames: &'a [Frame],
    request: &'a Aperture,
    policy_version: &'a str,
}

/// A deterministic 16-character hex hash of the evaluation inputs.
///
/// This is FNV-1a 64-bit over the canonical JSON encoding of physiology,
/// frames, request, and policy version. `Aperture` uses a `BTreeMap`, and sets
/// use a `BTreeSet`, so the encoding is stable across runs and platforms. This
/// is an evidence/diff hash, not a cryptographic MAC.
pub fn input_hash(
    physiology: &Aperture,
    frames: &[Frame],
    request: &Aperture,
    policy_version: &str,
) -> String {
    let payload = InputHashPayload {
        physiology,
        frames,
        request,
        policy_version,
    };
    let bytes = serde_json::to_vec(&payload).expect("aperture inputs are serializable");
    format!("{:016x}", fnv1a64(&bytes))
}

fn fnv1a64(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}
