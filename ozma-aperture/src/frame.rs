// SPDX-License-Identifier: AGPL-3.0-only
//! Frames, provenance, recourse, and all-party proposals.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

use crate::dimension::Aperture;

/// Who imposed a frame, and therefore what a residual means.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Provenance {
    /// The requester/user/controller imposed the limit on itself.
    Chosen,
    /// A device, bus, thermal, privacy, consent, or policy limit that the
    /// requester cannot lift alone.
    Forced,
    /// A proposal binding several parties.
    Negotiated,
}

impl Provenance {
    /// The recourse available for a residual caused by this provenance.
    pub fn recourse(self) -> Recourse {
        match self {
            Provenance::Chosen => Recourse::Relax,
            Provenance::Forced => Recourse::Escalate,
            Provenance::Negotiated => Recourse::Propose,
        }
    }
}

/// What the requester may do about a residual.
#[derive(Copy, Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Recourse {
    /// Drop or widen a chosen frame unilaterally, then re-evaluate.
    Relax,
    /// Emit a diagnostic/concern; an operator or policy change is required.
    Escalate,
    /// Offer a new aperture and adopt it only when every bound party accepts.
    Propose,
}

impl Recourse {
    /// Stable lower-case name used in receipts.
    pub fn as_str(self) -> &'static str {
        match self {
            Recourse::Relax => "relax",
            Recourse::Escalate => "escalate",
            Recourse::Propose => "propose",
        }
    }
}

impl std::fmt::Display for Recourse {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// An aperture plus the provenance that explains how it may be changed.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Frame {
    /// Stable identifier used in receipts and diagnostics.
    pub id: String,
    pub provenance: Provenance,
    pub aperture: Aperture,
}

impl Frame {
    /// Construct a frame.
    pub fn new(id: impl Into<String>, provenance: Provenance, aperture: Aperture) -> Self {
        Frame {
            id: id.into(),
            provenance,
            aperture,
        }
    }
}

/// A negotiated frame with all-party adoption and retraction semantics.
///
/// `accepted` records the parties that currently accept the proposal. A
/// proposal is adopted only when every member of `parties` is in `accepted`.
/// [`Proposal::retract`] removes a party's acceptance; the proposal is fully
/// retracted only when every bound party has retracted.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Proposal {
    pub frame: Frame,
    pub parties: BTreeSet<String>,
    pub accepted: BTreeSet<String>,
}

impl Proposal {
    /// Start a proposal with no acceptances.
    pub fn new<I, S>(frame: Frame, parties: I) -> Self
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        Proposal {
            frame,
            parties: parties.into_iter().map(Into::into).collect(),
            accepted: BTreeSet::new(),
        }
    }

    /// Record one party's acceptance.
    ///
    /// Returns `true` only when every bound party has accepted. Accepting as a
    /// non-party, or accepting unilaterally, does not adopt the proposal.
    pub fn accept(&mut self, party: &str) -> bool {
        if self.parties.contains(party) {
            self.accepted.insert(party.to_string());
        }
        self.is_adopted()
    }

    /// Withdraw one party's acceptance.
    ///
    /// Returns `true` only when every bound party has retracted, i.e. the
    /// proposal is fully retracted. A unilateral retraction after adoption
    /// leaves the proposal pending rather than adopted. Retracting as a
    /// non-party or as a party that has not accepted is a no-op.
    pub fn retract(&mut self, party: &str) -> bool {
        if !self.parties.contains(party) || !self.accepted.remove(party) {
            return false;
        }
        self.is_retracted()
    }

    /// True when every bound party has accepted.
    pub fn is_adopted(&self) -> bool {
        !self.parties.is_empty()
            && self
                .parties
                .iter()
                .all(|party| self.accepted.contains(party))
    }

    /// True when every bound party has retracted or not yet accepted.
    pub fn is_retracted(&self) -> bool {
        !self.parties.is_empty()
            && self
                .parties
                .iter()
                .all(|party| !self.accepted.contains(party))
    }

    /// The frame, but only when every party has accepted it.
    pub fn adopted_frame(&self) -> Option<&Frame> {
        self.is_adopted().then_some(&self.frame)
    }
}
