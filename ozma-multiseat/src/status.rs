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
                    Some(status) => {
                        let mut seat_status = seat.status(status.state, status.message.clone());
                        seat_status.pid = status.pid;
                        seat_status.restarts = status.restarts;
                        seat_status.last_error = status.last_error.clone();
                        seat_status.next_restart_in_ms = status.next_restart_in_ms;
                        seat_status
                    }
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

/// Render the bounded Prometheus text surface for a multi-seat status document.
///
/// Labels are seat ids and fixed state/port kinds only. Node argv, environment,
/// audio sink names, and error strings stay out of metrics so a scrape cannot
/// leak host paths or operator messages. `seat_count`/`node_count` are always
/// emitted together and are equal by construction.
pub fn render_metrics(status: &MultiseatStatus) -> String {
    let mut out = String::new();

    out.push_str(
        "# HELP ozma_multiseat_nodes Controller-visible nodes (one per seat).\n\
         # TYPE ozma_multiseat_nodes gauge\n",
    );
    out.push_str(&format!(
        "ozma_multiseat_nodes {}\n",
        status.capability.node_count
    ));

    out.push_str(
        "# HELP ozma_multiseat_seats Planned seats by supervisor state.\n\
         # TYPE ozma_multiseat_seats gauge\n",
    );
    for state in [
        SeatState::Planned,
        SeatState::Starting,
        SeatState::Running,
        SeatState::Stopped,
        SeatState::Failed,
    ] {
        let count = status
            .seats
            .iter()
            .filter(|seat| seat.state == state)
            .count();
        out.push_str(&format!(
            "ozma_multiseat_seats{{state=\"{}\"}} {}\n",
            escape_label(state.as_str()),
            count
        ));
    }

    out.push_str(
        "# HELP ozma_multiseat_seat_up Whether a node process is running for this seat.\n\
         # TYPE ozma_multiseat_seat_up gauge\n",
    );
    for seat in &status.seats {
        out.push_str(&format!(
            "ozma_multiseat_seat_up{{seat=\"{}\"}} {}\n",
            escape_label(&seat.seat_id),
            u8::from(seat.state == SeatState::Running)
        ));
    }

    out.push_str(
        "# HELP ozma_multiseat_seat_restarts_total Successful node restarts after failures.\n\
         # TYPE ozma_multiseat_seat_restarts_total counter\n",
    );
    for seat in &status.seats {
        out.push_str(&format!(
            "ozma_multiseat_seat_restarts_total{{seat=\"{}\"}} {}\n",
            escape_label(&seat.seat_id),
            seat.restarts
        ));
    }

    out.push_str(
        "# HELP ozma_multiseat_seat_port Planned node port, by kind.\n\
         # TYPE ozma_multiseat_seat_port gauge\n",
    );
    for seat in &status.seats {
        for (kind, value) in [("udp", seat.udp_port), ("http", seat.http_port)] {
            out.push_str(&format!(
                "ozma_multiseat_seat_port{{seat=\"{}\",kind=\"{}\"}} {}\n",
                escape_label(&seat.seat_id),
                escape_label(kind),
                value
            ));
        }
    }

    out
}

fn escape_label(value: &str) -> String {
    let mut escaped = String::with_capacity(value.len());
    for character in value.chars() {
        match character {
            '\\' => escaped.push_str("\\\\"),
            '"' => escaped.push_str("\\\""),
            '\n' => escaped.push_str("\\n"),
            other => escaped.push(other),
        }
    }
    escaped
}
