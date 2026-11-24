// SPDX-License-Identifier: AGPL-3.0-only
//! Controller-facing capability and status documents.
//!
//! The multi-seat principle is one seat per node: the controller must see
//! `node_count == seat_count`. The status document carries the planned launch
//! ports and the supervisor's per-seat state without inventing a single merged
//! node.

use serde::{Deserialize, Serialize};

use crate::seat::{MultiseatPlan, PlanDiagnostic, SeatRuntimeStatus, SeatState, SeatStatus};

/// Static capability shape advertised by a multi-seat host.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiseatCapability {
    /// Whether at least one seat is planned.
    pub available: bool,
    /// Number of seats.
    pub seat_count: usize,
    /// Number of controller-visible nodes. Equal to `seat_count`.
    pub node_count: usize,
    /// Display backend name.
    pub display_backend: String,
    /// Input backend name.
    pub input_backend: String,
    /// Capture providers in use (`x11`, `pipewire`), sorted.
    pub capture_providers: Vec<String>,
}

/// Full multi-seat status document for the controller.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct MultiseatStatus {
    /// Capability summary.
    pub capability: MultiseatCapability,
    /// Requested seat count before any display cap.
    pub requested_seats: usize,
    /// Per-seat status, one entry per node.
    pub seats: Vec<SeatStatus>,
    /// Non-fatal planning diagnostics.
    #[serde(default)]
    pub diagnostics: Vec<PlanDiagnostic>,
}

impl MultiseatStatus {
    /// Build a status document from a plan, with every seat still planned.
    pub fn from_plan(plan: &MultiseatPlan) -> Self {
        Self::from_plan_with_runtime(plan, &[])
    }

    /// Build a status document from a plan and supervisor runtime statuses.
    ///
    /// Runtime statuses for unknown seats are ignored. A planned seat with no
    /// runtime entry stays [`SeatState::Planned`].
    pub fn from_plan_with_runtime(plan: &MultiseatPlan, runtime: &[SeatRuntimeStatus]) -> Self {
        let seats = plan
            .seats
            .iter()
            .map(|seat| {
                match runtime
                    .iter()
                    .rev()
                    .find(|status| status.seat_id == seat.config.id)
                {
                    Some(status) => seat.status(status.state, status.message.clone()),
                    None => seat.status(SeatState::Planned, None),
                }
            })
            .collect();

        Self {
            capability: MultiseatCapability {
                available: !plan.seats.is_empty(),
                seat_count: plan.seat_count(),
                node_count: plan.node_count(),
                display_backend: plan.display_backend.clone(),
                input_backend: plan.input_backend.clone(),
                capture_providers: plan.capture_providers(),
            },
            requested_seats: plan.requested_seats,
            seats,
            diagnostics: plan.diagnostics.clone(),
        }
    }
}
